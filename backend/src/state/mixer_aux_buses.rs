//! Dynamic aux buses of the audio mixer at the application level: the
//! running mixer's buses (see `mixer::dynamic`), kept in the block property
//! `dynamic_aux_buses` so a restart builds them again.

use std::collections::HashMap;

use strom_types::mixer::{
    is_aux_bus_property, AddMixerAuxBusRequest, DynamicAuxBus, MixerAuxBuses,
    DYNAMIC_AUX_BUSES_PROPERTY,
};
use strom_types::{FlowId, PropertyValue, StromEvent};

use super::AppState;
use crate::blocks::builtin::mixer::dynamic::stored_dynamic_aux_buses;
use crate::blocks::builtin::mixer::dynamic::{self, AuxBusError};
use crate::blocks::builtin::mixer::MIXER_BLOCK_ID;

impl AppState {
    /// The aux buses of a mixer block.
    pub async fn mixer_aux_buses(
        &self,
        flow_id: &FlowId,
        block_id: &str,
    ) -> Result<MixerAuxBuses, AuxBusError> {
        let properties = self.mixer_properties(flow_id, block_id).await?;
        stored_list(&properties)?;
        Ok(dynamic::aux_buses(&properties))
    }

    /// Add an aux bus to a running mixer. Returns the mixer's buses
    /// afterwards.
    pub async fn add_mixer_aux_bus(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        request: AddMixerAuxBusRequest,
    ) -> Result<MixerAuxBuses, AuxBusError> {
        // One change at a time: a second add would not see the first one's
        // number or channel until it is stored.
        let _serial = self.inner.mixer_aux_buses.lock().await;
        // Read before the pipelines are locked: no lock on the flows is
        // taken while one on the pipelines is held.
        let mut properties = self.mixer_properties(flow_id, block_id).await?;
        self.check_channel_free(flow_id, block_id, &request.channel)
            .await?;
        let mut pipelines = self.inner.pipelines.write().await;
        let manager = pipelines
            .get_mut(flow_id)
            .ok_or_else(|| not_running(flow_id))?;
        let aux = dynamic::choose_aux(&properties, &request, |aux| {
            dynamic::is_built(manager.pipeline(), block_id, aux)
        })?;
        let added = dynamic::added_bus_properties(&properties, aux, &request);
        properties.extend(added.iter().cloned());
        let bus_channel = request.channel.clone();
        let bus = DynamicAuxBus {
            aux,
            channel: request.channel,
        };
        let elements = dynamic::add_aux_bus(manager.pipeline(), block_id, &properties, &bus)?;
        manager.adopt_elements(elements);
        drop(pipelines);
        let stored = self
            .store_aux_buses(flow_id, block_id, |buses| buses.push(bus), added, None)
            .await;
        // Registered only while the bus runs: a stop in between took the
        // flow's channels out already.
        let running = self
            .inner
            .pipelines
            .read()
            .await
            .get(flow_id)
            .is_some_and(|m| dynamic::is_built_dynamic(m.pipeline(), block_id, aux));
        if stored.is_ok() && running {
            self.register_aux_bus_channel(flow_id, block_id, &bus_channel)
                .await;
        }
        if stored.is_err() {
            // The block went while the bus was built: a bus nobody stored
            // would run on unseen.
            if let Some(manager) = self.inner.pipelines.write().await.get_mut(flow_id) {
                let removed =
                    dynamic::remove_aux_bus(manager.pipeline(), block_id, &properties, aux);
                manager.forget_elements(&removed);
            }
        }
        stored
    }

    /// Remove a dynamic aux bus from a mixer, running or not. Returns the
    /// mixer's buses afterwards. A stored bus that no longer fits the
    /// mixer's numbers (not built) can be removed too.
    pub async fn remove_mixer_aux_bus(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        aux: usize,
    ) -> Result<MixerAuxBuses, AuxBusError> {
        let serial = self.inner.mixer_aux_buses.lock().await;
        let properties = self.mixer_properties(flow_id, block_id).await?;
        let buses = dynamic::aux_buses(&properties);
        let stored = stored_list(&properties)?;
        let listed = stored.iter().find(|b| b.aux == aux).cloned();
        // What runs is the pipeline's: the stored numbers may have changed
        // since it was built, or the list lost the bus.
        let mut pipelines = self.inner.pipelines.write().await;
        let built = pipelines
            .get(flow_id)
            .is_some_and(|m| dynamic::is_built_dynamic(m.pipeline(), block_id, aux));
        if listed.is_none() && !built {
            return Err(if (1..=buses.num_static).contains(&aux) {
                AuxBusError::Conflict(format!(
                    "aux bus {} is built with the flow (num_aux_buses) and cannot be removed",
                    aux
                ))
            } else {
                AuxBusError::NotFound(format!("aux bus {} is not a dynamic bus", aux))
            });
        }
        let mut channel = None;
        if built {
            if let Some(manager) = pipelines.get_mut(flow_id) {
                channel = dynamic::bus_channel(manager.pipeline(), block_id, aux);
                let removed =
                    dynamic::remove_aux_bus(manager.pipeline(), block_id, &properties, aux);
                manager.forget_elements(&removed);
            }
        }
        drop(pipelines);
        // Only the entry this bus made: a bus that did not run registered
        // nothing, and the name may be another flow's.
        if let Some(channel) = channel {
            let ours = self.channels().get(&channel).await.is_some_and(|info| {
                info.source_flow_id == *flow_id && info.output_name == aux_bus_output(block_id)
            });
            if ours {
                self.channels().unregister(&channel).await;
            }
        }
        // The bus's settings go with it, so one added later under its number
        // starts from the defaults; a number the flow now builds itself keeps
        // them (they are that bus's).
        let dropped = (aux > buses.num_static).then_some(aux);
        let result = self
            .store_aux_buses(
                flow_id,
                block_id,
                |buses| buses.retain(|b| b.aux != aux),
                Vec::new(),
                dropped,
            )
            .await;
        drop(serial);
        if built {
            // An AFL on the bus goes with it.
            self.set_mixer_solo_intent(flow_id, block_id, &format!("aux{}_afl", aux), false)
                .await;
            self.refresh_mixer_monitor_gates(flow_id, block_id, None)
                .await;
        }
        result
    }

    /// Enter a dynamic bus's channel in the channel registry, so a second
    /// producer on it is refused (see `check_channel_free`). The flow's stop
    /// takes it out again with its other channels.
    pub(crate) async fn register_aux_bus_channel(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        channel: &str,
    ) {
        self.channels()
            .register(crate::sharing::ChannelInfo {
                source_flow_id: *flow_id,
                output_name: aux_bus_output(block_id),
                channel_name: channel.to_string(),
                media_type: strom_types::MediaType::Audio,
            })
            .await;
    }

    /// Refuse `channel` for a new bus when another mixer stores a bus on it
    /// or a running flow publishes on it: two producers on one inter channel
    /// fail, and the flow that starts second would not start at all.
    async fn check_channel_free(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        channel: &str,
    ) -> Result<(), AuxBusError> {
        let taken = {
            let flows = self.inner.flows.read().await;
            flows.values().find_map(|flow| {
                flow.blocks
                    .iter()
                    .filter(|b| b.block_definition_id == MIXER_BLOCK_ID)
                    .filter(|b| !(flow.id == *flow_id && b.id == block_id))
                    .find_map(|b| {
                        // A list that does not parse is refused until it is
                        // fixed; its channels are not known here.
                        let stored = stored_dynamic_aux_buses(&b.properties).ok()?;
                        let bus = stored.iter().find(|bus| bus.channel == channel)?;
                        Some(format!(
                            "channel {} is taken by aux bus {} of block {} in flow {}",
                            channel, bus.aux, b.id, flow.id
                        ))
                    })
            })
        };
        if let Some(reason) = taken {
            return Err(AuxBusError::Conflict(reason));
        }
        if let Some(info) = self.channels().get(channel).await {
            return Err(AuxBusError::Conflict(format!(
                "channel {} is published by flow {}",
                channel, info.source_flow_id
            )));
        }
        Ok(())
    }

    /// The stored properties of mixer block `block_id`.
    async fn mixer_properties(
        &self,
        flow_id: &FlowId,
        block_id: &str,
    ) -> Result<HashMap<String, PropertyValue>, AuxBusError> {
        let flows = self.inner.flows.read().await;
        let block = flows
            .get(flow_id)
            .and_then(|f| f.blocks.iter().find(|b| b.id == block_id))
            .ok_or_else(|| {
                AuxBusError::NotFound(format!("no block {} in flow {}", block_id, flow_id))
            })?;
        if block.block_definition_id != MIXER_BLOCK_ID {
            return Err(AuxBusError::NotFound(format!(
                "block {} is not an audio mixer",
                block_id
            )));
        }
        Ok(block.properties.clone())
    }

    /// Change the stored bus list with `change` and store `set` with it;
    /// `dropped` names a bus whose properties go (a removed bus, so one
    /// added later under its number starts from the defaults). Returns the
    /// buses afterwards.
    async fn store_aux_buses(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        change: impl FnOnce(&mut Vec<DynamicAuxBus>),
        set: Vec<(String, PropertyValue)>,
        dropped: Option<usize>,
    ) -> Result<MixerAuxBuses, AuxBusError> {
        let buses = {
            let mut flows = self.inner.flows.write().await;
            let block = flows
                .get_mut(flow_id)
                .and_then(|f| f.blocks.iter_mut().find(|b| b.id == block_id))
                .ok_or_else(|| {
                    AuxBusError::NotFound(format!("no block {} in flow {}", block_id, flow_id))
                })?;
            // Every stored bus, also one that no longer fits and is not
            // built: it comes back when the numbers fit again.
            let mut list = stored_list(&block.properties)?;
            change(&mut list);
            list.sort_by_key(|b| b.aux);
            if let Some(aux) = dropped {
                block
                    .properties
                    .retain(|name, _| !is_aux_bus_property(name, aux));
            }
            block.properties.extend(set);
            if list.is_empty() {
                block.properties.remove(DYNAMIC_AUX_BUSES_PROPERTY);
            } else {
                let json = serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string());
                block.properties.insert(
                    DYNAMIC_AUX_BUSES_PROPERTY.to_string(),
                    PropertyValue::String(json),
                );
            }
            dynamic::aux_buses(&block.properties)
        };
        self.mark_flow_dirty(*flow_id).await;
        // Clients holding the flow (the mixer editor) see the new list
        // before they save theirs.
        self.inner
            .events
            .broadcast(StromEvent::FlowUpdated { flow_id: *flow_id });
        Ok(buses)
    }
}

/// The stored bus list of a mixer with these properties. One that does not
/// parse is refused rather than replaced: storing over it would lose every
/// bus in it.
fn stored_list(
    properties: &HashMap<String, PropertyValue>,
) -> Result<Vec<DynamicAuxBus>, AuxBusError> {
    stored_dynamic_aux_buses(properties)
        .map_err(|e| AuxBusError::Conflict(format!("{}; fix or remove it first", e)))
}

/// The registry's output name of a mixer's dynamic aux buses.
fn aux_bus_output(block_id: &str) -> String {
    format!("{}:aux_bus", block_id)
}

fn not_running(flow_id: &FlowId) -> AuxBusError {
    AuxBusError::NotFound(format!("flow {} is not running", flow_id))
}
