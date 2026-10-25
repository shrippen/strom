//! Dynamic vision mixer inputs at the application level: the running
//! mixer's inputs (see `vision_mixer::dynamic`), kept in the block property
//! `dynamic_inputs` so a restart builds them again, and announced with
//! `VisionMixerInputsChanged`.

use strom_types::vision_mixer::{
    AddVisionMixerInputRequest, DynamicInput, VisionMixerInputInfo, VisionMixerInputs,
    DYNAMIC_INPUTS_PROPERTY,
};
use strom_types::{FlowId, PropertyValue, StromEvent};
use tracing::{info, warn};

use super::AppState;
use crate::blocks::builtin::vision_mixer::dynamic::{self, DynamicInputError};
use crate::blocks::builtin::vision_mixer::overlay::{self, VisionMixerOverlayState};
use std::sync::Arc;

impl AppState {
    /// The inputs of a running vision mixer.
    pub async fn vision_mixer_inputs(
        &self,
        flow_id: &FlowId,
        block_id: &str,
    ) -> Result<VisionMixerInputs, DynamicInputError> {
        Ok(describe(&*running_state(flow_id, block_id)?))
    }

    /// Add an input to a running vision mixer. Returns the mixer's inputs
    /// afterwards.
    pub async fn add_vision_mixer_input(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        request: AddVisionMixerInputRequest,
    ) -> Result<(DynamicInput, VisionMixerInputs), DynamicInputError> {
        let state = running_state(flow_id, block_id)?;
        let added = {
            let pipelines = self.inner.pipelines.read().await;
            let manager = pipelines.get(flow_id).ok_or_else(|| not_running(flow_id))?;
            dynamic::add_input(manager.pipeline(), &state, *flow_id, block_id, request)?
        };
        self.store_dynamic_inputs(flow_id, block_id, &state).await;
        Ok((added, describe(&state)))
    }

    /// Remove a dynamic input from a running vision mixer. Returns the
    /// mixer's inputs afterwards.
    pub async fn remove_vision_mixer_input(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        index: usize,
    ) -> Result<VisionMixerInputs, DynamicInputError> {
        let state = running_state(flow_id, block_id)?;
        {
            let pipelines = self.inner.pipelines.read().await;
            let manager = pipelines.get(flow_id).ok_or_else(|| not_running(flow_id))?;
            dynamic::remove_input(manager.pipeline(), &state, *flow_id, block_id, index)?;
        }
        self.store_dynamic_inputs(flow_id, block_id, &state).await;
        Ok(describe(&state))
    }

    /// Build the stored dynamic inputs of every vision mixer in `flow`,
    /// right after it started. An input that fails is logged; the property
    /// keeps it for the next start.
    pub(super) async fn restore_vision_mixer_inputs(&self, flow: &strom_types::Flow) {
        use crate::blocks::builtin::vision_mixer::properties as vm_props;
        for block in &flow.blocks {
            if block.block_definition_id != crate::blocks::builtin::vision_mixer::BLOCK_ID {
                continue;
            }
            let num_inputs = vm_props::num_inputs_or_max(&block.properties);
            let capacity = vm_props::capacity_or_num_inputs(&block.properties);
            let inputs =
                match vm_props::parse_dynamic_inputs(&block.properties, num_inputs, capacity) {
                    Ok(inputs) => inputs,
                    Err(e) => {
                        warn!("Vision mixer {}: {}", block.id, e);
                        continue;
                    }
                };
            if inputs.is_empty() {
                continue;
            }
            let Some(state) = overlay::get_overlay_state(&flow.id, &block.id) else {
                continue;
            };
            let pipelines = self.inner.pipelines.read().await;
            let Some(manager) = pipelines.get(&flow.id) else {
                return;
            };
            for input in inputs {
                let index = input.index;
                let request = AddVisionMixerInputRequest {
                    index: Some(index),
                    source: input.source.clone(),
                    label: input.label.clone(),
                };
                match dynamic::add_input(manager.pipeline(), &state, flow.id, &block.id, request) {
                    Ok(_) => info!("Vision mixer {}: restored input {}", block.id, index),
                    Err(e) => {
                        warn!(
                            "Vision mixer {}: could not restore input {}: {}",
                            block.id, index, e
                        );
                        if let Ok(mut d) = state.dynamic.lock() {
                            d.keep_unrestored(input);
                        }
                    }
                }
            }
            drop(pipelines);
            self.inner
                .events
                .broadcast(StromEvent::VisionMixerInputsChanged {
                    flow_id: flow.id,
                    block_id: block.id.clone(),
                    active_inputs: state.active_inputs(),
                });
        }
    }

    /// Write the mixer's dynamic inputs to its block property (saved with
    /// the flow) and announce the change.
    async fn store_dynamic_inputs(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        state: &VisionMixerOverlayState,
    ) {
        let inputs = state.dynamic.lock().map(|d| d.inputs()).unwrap_or_default();
        let json = serde_json::to_string(&inputs).unwrap_or_else(|_| "[]".to_string());
        {
            let mut flows = self.inner.flows.write().await;
            if let Some(block) = flows
                .get_mut(flow_id)
                .and_then(|f| f.blocks.iter_mut().find(|b| b.id == block_id))
            {
                if inputs.is_empty() {
                    block.properties.remove(DYNAMIC_INPUTS_PROPERTY);
                } else {
                    block.properties.insert(
                        DYNAMIC_INPUTS_PROPERTY.to_string(),
                        PropertyValue::String(json),
                    );
                }
            }
        }
        self.mark_flow_dirty(*flow_id).await;
        self.inner
            .events
            .broadcast(StromEvent::VisionMixerInputsChanged {
                flow_id: *flow_id,
                block_id: block_id.to_string(),
                active_inputs: state.active_inputs(),
            });
    }
}

fn not_running(flow_id: &FlowId) -> DynamicInputError {
    DynamicInputError::NotFound(format!("flow {} is not running", flow_id))
}

fn running_state(
    flow_id: &FlowId,
    block_id: &str,
) -> Result<Arc<VisionMixerOverlayState>, DynamicInputError> {
    overlay::get_overlay_state(flow_id, block_id).ok_or_else(|| {
        DynamicInputError::NotFound(format!(
            "no running vision mixer {} in flow {}",
            block_id, flow_id
        ))
    })
}

fn describe(state: &VisionMixerOverlayState) -> VisionMixerInputs {
    let labels = state.labels();
    let dynamic = state.dynamic.lock();
    let inputs = state
        .active_inputs()
        .into_iter()
        .map(|index| VisionMixerInputInfo {
            index,
            label: labels
                .get(index)
                .cloned()
                .unwrap_or_else(|| format!("In {}", index + 1)),
            source: dynamic.as_ref().ok().and_then(|d| d.source(index)),
        })
        .collect();
    let unrestored = dynamic.as_ref().map(|d| d.unrestored()).unwrap_or_default();
    VisionMixerInputs {
        capacity: state.capacity,
        num_static: state.num_static,
        inputs,
        unrestored,
    }
}
