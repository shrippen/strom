//! Dynamic channels of the audio mixer at the application level: the
//! running mixer's channels (see `mixer::dynamic_channels`), kept in the
//! block property `dynamic_channels` so a restart builds them again.

use std::collections::HashMap;

use strom_types::mixer::{
    is_channel_property, AddMixerChannelRequest, DynamicChannel, MixerChannels,
    DYNAMIC_CHANNELS_PROPERTY,
};
use strom_types::{FlowId, PropertyValue, StromEvent};

use super::AppState;
use crate::blocks::builtin::mixer::dynamic_channels::{self, ChannelError};
use crate::blocks::builtin::mixer::MIXER_BLOCK_ID;

impl AppState {
    /// The channels of a mixer block.
    pub async fn mixer_channels(
        &self,
        flow_id: &FlowId,
        block_id: &str,
    ) -> Result<MixerChannels, ChannelError> {
        let properties = self.mixer_channel_properties(flow_id, block_id).await?;
        stored_list(&properties)?;
        Ok(dynamic_channels::channels(&properties))
    }

    /// Add a channel to a running mixer. Returns the mixer's channels
    /// afterwards.
    pub async fn add_mixer_channel(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        request: AddMixerChannelRequest,
    ) -> Result<MixerChannels, ChannelError> {
        // One change at a time, also against the aux buses: a bus added
        // meanwhile would miss this channel's send, and this channel the bus.
        let _serial = self.inner.mixer_aux_buses.lock().await;
        // Read before the pipelines are locked: no lock on the flows is
        // taken while one on the pipelines is held.
        let properties = self.mixer_channel_properties(flow_id, block_id).await?;
        let mut pipelines = self.inner.pipelines.write().await;
        let manager = pipelines
            .get_mut(flow_id)
            .ok_or_else(|| not_running(flow_id))?;
        let channel = dynamic_channels::choose_channel(&properties, &request, |ch| {
            dynamic_channels::is_built(manager.pipeline(), block_id, ch)
        })?;
        let elements =
            dynamic_channels::add_channel(manager.pipeline(), block_id, &properties, &channel)?;
        manager.adopt_elements(elements);
        drop(pipelines);
        let number = channel.channel;
        let stored = self
            .store_channels(flow_id, block_id, |list| list.push(channel), None)
            .await;
        if stored.is_err() {
            // The block went while the channel was built: a channel nobody
            // stored would play on unseen.
            if let Some(manager) = self.inner.pipelines.write().await.get_mut(flow_id) {
                let removed = dynamic_channels::remove_channel(
                    manager.pipeline(),
                    block_id,
                    &properties,
                    number,
                );
                manager.forget_elements(&removed);
            }
        }
        stored
    }

    /// Remove a dynamic channel from a mixer, running or not. Returns the
    /// mixer's channels afterwards. Its stored settings go with it.
    pub async fn remove_mixer_channel(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        ch: usize,
    ) -> Result<MixerChannels, ChannelError> {
        if ch == 0 {
            return Err(ChannelError::Invalid(
                "channels are numbered from 1".to_string(),
            ));
        }
        let serial = self.inner.mixer_aux_buses.lock().await;
        let properties = self.mixer_channel_properties(flow_id, block_id).await?;
        let channels = dynamic_channels::channels(&properties);
        // Also a stored dynamic channel that `num_channels` took in since:
        // it is the flow's now, and its settings stay.
        if (1..=channels.num_static).contains(&ch) {
            return Err(ChannelError::Conflict(format!(
                "channel {} is built with the flow (num_channels) and cannot be removed",
                ch
            )));
        }
        let listed = stored_list(&properties)?.iter().any(|c| c.channel == ch);
        let mut pipelines = self.inner.pipelines.write().await;
        let built = pipelines
            .get(flow_id)
            .is_some_and(|m| dynamic_channels::is_built_dynamic(m.pipeline(), block_id, ch));
        if !listed && !built {
            return Err(ChannelError::NotFound(format!(
                "channel {} is not a dynamic channel",
                ch
            )));
        }
        if built {
            if let Some(manager) = pipelines.get_mut(flow_id) {
                let removed =
                    dynamic_channels::remove_channel(manager.pipeline(), block_id, &properties, ch);
                manager.forget_elements(&removed);
            }
        }
        drop(pipelines);
        let result = self
            .store_channels(
                flow_id,
                block_id,
                |list| list.retain(|c| c.channel != ch),
                Some(ch),
            )
            .await;
        drop(serial);
        if built {
            // A PFL or AFL on the channel goes with it.
            for solo in ["pfl", "afl"] {
                self.set_mixer_solo_intent(flow_id, block_id, &format!("ch{}_{}", ch, solo), false)
                    .await;
            }
            self.refresh_mixer_monitor_gates(flow_id, block_id, None)
                .await;
        }
        result
    }

    /// The stored properties of mixer block `block_id`.
    async fn mixer_channel_properties(
        &self,
        flow_id: &FlowId,
        block_id: &str,
    ) -> Result<HashMap<String, PropertyValue>, ChannelError> {
        let flows = self.inner.flows.read().await;
        let block = flows
            .get(flow_id)
            .and_then(|f| f.blocks.iter().find(|b| b.id == block_id))
            .ok_or_else(|| {
                ChannelError::NotFound(format!("no block {} in flow {}", block_id, flow_id))
            })?;
        if block.block_definition_id != MIXER_BLOCK_ID {
            return Err(ChannelError::NotFound(format!(
                "block {} is not an audio mixer",
                block_id
            )));
        }
        Ok(block.properties.clone())
    }

    /// Change the stored channel list with `change`; `dropped` names a
    /// channel whose properties go (a removed channel, so one added later
    /// under its number starts from the defaults). Returns the channels
    /// afterwards.
    async fn store_channels(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        change: impl FnOnce(&mut Vec<DynamicChannel>),
        dropped: Option<usize>,
    ) -> Result<MixerChannels, ChannelError> {
        let channels = {
            let mut flows = self.inner.flows.write().await;
            let block = flows
                .get_mut(flow_id)
                .and_then(|f| f.blocks.iter_mut().find(|b| b.id == block_id))
                .ok_or_else(|| {
                    ChannelError::NotFound(format!("no block {} in flow {}", block_id, flow_id))
                })?;
            let mut list = stored_list(&block.properties)?;
            change(&mut list);
            list.sort_by_key(|c| c.channel);
            if let Some(ch) = dropped {
                block
                    .properties
                    .retain(|name, _| !is_channel_property(name, ch));
            }
            if list.is_empty() {
                block.properties.remove(DYNAMIC_CHANNELS_PROPERTY);
            } else {
                let json = serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string());
                block.properties.insert(
                    DYNAMIC_CHANNELS_PROPERTY.to_string(),
                    PropertyValue::String(json),
                );
            }
            dynamic_channels::channels(&block.properties)
        };
        self.mark_flow_dirty(*flow_id).await;
        // Clients holding the flow (the mixer editor) see the new list
        // before they save theirs.
        self.inner
            .events
            .broadcast(StromEvent::FlowUpdated { flow_id: *flow_id });
        Ok(channels)
    }
}

/// The stored channel list of a mixer with these properties. One that does
/// not parse is refused rather than replaced: storing over it would lose
/// every channel in it.
fn stored_list(
    properties: &HashMap<String, PropertyValue>,
) -> Result<Vec<DynamicChannel>, ChannelError> {
    crate::blocks::builtin::mixer::stored_dynamic_channels(properties)
        .map_err(|e| ChannelError::Conflict(format!("{}; fix or remove it first", e)))
}

fn not_running(flow_id: &FlowId) -> ChannelError {
    ChannelError::NotFound(format!("flow {} is not running", flow_id))
}
