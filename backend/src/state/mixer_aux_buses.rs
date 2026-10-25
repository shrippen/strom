//! Dynamic aux buses of the audio mixer at the application level: the
//! running mixer's buses (see `mixer::dynamic`), kept in the block property
//! `dynamic_aux_buses` so a restart builds them again.

use std::collections::HashMap;

use strom_types::mixer::{
    AddMixerAuxBusRequest, DynamicAuxBus, MixerAuxBuses, DYNAMIC_AUX_BUSES_PROPERTY,
};
use strom_types::{FlowId, PropertyValue};

use super::AppState;
use crate::blocks::builtin::mixer::dynamic::{self, AuxBusError};
use crate::blocks::builtin::mixer::MIXER_BLOCK_ID;

impl AppState {
    /// The aux buses of a mixer block.
    pub async fn mixer_aux_buses(
        &self,
        flow_id: &FlowId,
        block_id: &str,
    ) -> Result<MixerAuxBuses, AuxBusError> {
        Ok(dynamic::aux_buses(
            &self.mixer_properties(flow_id, block_id).await?,
        ))
    }

    /// Add an aux bus to a running mixer. Returns the mixer's buses
    /// afterwards.
    pub async fn add_mixer_aux_bus(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        request: AddMixerAuxBusRequest,
    ) -> Result<MixerAuxBuses, AuxBusError> {
        // Read before the pipelines are locked: no lock on the flows is
        // taken while one on the pipelines is held. The write lock on the
        // pipelines keeps a second add from building the same number
        // (`is_built`) between the check and the build.
        let mut properties = self.mixer_properties(flow_id, block_id).await?;
        let mut pipelines = self.inner.pipelines.write().await;
        let manager = pipelines
            .get_mut(flow_id)
            .ok_or_else(|| not_running(flow_id))?;
        let aux = dynamic::choose_aux(&properties, &request, |aux| {
            dynamic::is_built(manager.pipeline(), block_id, aux)
        })?;
        let added = dynamic::added_bus_properties(&properties, aux, &request);
        properties.extend(added.iter().cloned());
        let bus = DynamicAuxBus {
            aux,
            channel: request.channel,
        };
        let elements = dynamic::add_aux_bus(manager.pipeline(), block_id, &properties, &bus)?;
        manager.adopt_elements(elements);
        drop(pipelines);
        self.store_aux_buses(flow_id, block_id, |buses| buses.push(bus), added, None)
            .await
    }

    /// Remove a dynamic aux bus from a running mixer. Returns the mixer's
    /// buses afterwards.
    pub async fn remove_mixer_aux_bus(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        aux: usize,
    ) -> Result<MixerAuxBuses, AuxBusError> {
        let properties = self.mixer_properties(flow_id, block_id).await?;
        let buses = dynamic::aux_buses(&properties);
        if (1..=buses.num_static).contains(&aux) {
            return Err(AuxBusError::Conflict(format!(
                "aux bus {} is built with the flow (num_aux_buses) and cannot be removed",
                aux
            )));
        }
        if !buses.dynamic.iter().any(|b| b.aux == aux) {
            return Err(AuxBusError::NotFound(format!(
                "aux bus {} is not a dynamic bus",
                aux
            )));
        }
        // A stopped flow only loses the stored bus.
        let mut pipelines = self.inner.pipelines.write().await;
        if let Some(manager) = pipelines.get_mut(flow_id) {
            let removed = dynamic::remove_aux_bus(manager.pipeline(), block_id, &properties, aux);
            manager.forget_elements(&removed);
        }
        drop(pipelines);
        // An AFL on the bus goes with it.
        self.set_mixer_solo_intent(flow_id, block_id, &format!("aux{}_afl", aux), false)
            .await;
        self.refresh_mixer_monitor_gates(flow_id, block_id, None)
            .await;
        self.store_aux_buses(
            flow_id,
            block_id,
            |buses| buses.retain(|b| b.aux != aux),
            Vec::new(),
            Some(aux),
        )
        .await
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
            let mut list = dynamic::aux_buses(&block.properties).dynamic;
            change(&mut list);
            list.sort_by_key(|b| b.aux);
            if let Some(aux) = dropped {
                block
                    .properties
                    .retain(|name, _| !dynamic::is_bus_property(name, aux));
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
        Ok(buses)
    }
}

fn not_running(flow_id: &FlowId) -> AuxBusError {
    AuxBusError::NotFound(format!("flow {} is not running", flow_id))
}
