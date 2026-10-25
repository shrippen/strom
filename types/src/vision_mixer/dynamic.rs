//! Dynamic vision mixer inputs: inputs added to and removed from a running
//! mixer, without rebuilding it.
//!
//! A mixer reserves pad indices for `max_inputs` inputs (its pad capacity,
//! see [`super::PadMap`]). Inputs `0..num_inputs` are static: built with the
//! flow and fed through flow links. Indices `num_inputs..max_inputs` are free
//! slots that an API call fills with a dynamic input (fed from an inter
//! channel inside the mixer, so no flow link is needed) and empties again.
//! Dynamic inputs are stored in the block property `dynamic_inputs`, so a
//! restart builds them again at the same indices.

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// Block property: number of input slots (pad capacity). `0` or missing
/// means `num_inputs` (a mixer without free slots).
pub const MAX_INPUTS_PROPERTY: &str = "max_inputs";

/// Block property: the dynamic inputs, as a JSON list of [`DynamicInput`].
pub const DYNAMIC_INPUTS_PROPERTY: &str = "dynamic_inputs";

/// Highest pad capacity (`max_inputs`) a vision mixer accepts. Free slots
/// cost nothing but pad indices; the real limit is what the machine can mix.
pub const MAX_INPUT_CAPACITY: usize = 128;

/// Highest `max_time_ms` of a dynamic input's inter channel (the Inter
/// Input block offers up to 2000 ms).
pub const MAX_INTER_MAX_TIME_MS: u64 = 10_000;

/// Where a dynamic input's video comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DynamicInputSource {
    /// An inter channel published by an Inter Output block (or any
    /// `intersink` with this producer name).
    Inter {
        /// Producer name of the channel (the Inter Input block's `channel`).
        channel: String,
        /// Buffer time of the subscriber in ms (the Inter Input block's
        /// `max_time`); `None` keeps the element default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_time_ms: Option<u64>,
    },
}

/// A dynamic input: its slot, source and label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct DynamicInput {
    /// Input index (`num_inputs <= index < max_inputs`).
    pub index: usize,
    pub source: DynamicInputSource,
    /// Multiview label; `None` shows the default "In N".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Request to add an input to a running vision mixer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct AddVisionMixerInputRequest {
    /// Slot to fill; `None` takes the lowest free one.
    #[serde(default)]
    pub index: Option<usize>,
    pub source: DynamicInputSource,
    #[serde(default)]
    pub label: Option<String>,
}

/// One input of a vision mixer, as the inputs endpoint lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct VisionMixerInputInfo {
    pub index: usize,
    /// Multiview label.
    pub label: String,
    /// `None` for a static input (fed by a flow link); the source of a
    /// dynamic one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<DynamicInputSource>,
}

/// The inputs of a running vision mixer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct VisionMixerInputs {
    /// Number of input slots (`max_inputs`).
    pub capacity: usize,
    /// Static inputs (`num_inputs`): indices below this cannot be removed.
    pub num_static: usize,
    /// Every input the mixer has, ascending by index.
    pub inputs: Vec<VisionMixerInputInfo>,
    /// Stored dynamic inputs that failed to build when the flow started
    /// (GStreamer refused them). They stay stored and are tried
    /// again at the next start; removing one drops it, adding an input at
    /// its index replaces it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unrestored: Vec<DynamicInput>,
}

/// Parse the `dynamic_inputs` property. An empty string is no inputs.
pub fn parse_dynamic_inputs(json: &str) -> Result<Vec<DynamicInput>, String> {
    if json.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(json).map_err(|e| format!("dynamic_inputs is not valid: {}", e))
}

/// Check dynamic inputs against the mixer's slots: each index in
/// `num_static..capacity`, no index twice, a non-empty channel.
pub fn validate_dynamic_inputs(
    inputs: &[DynamicInput],
    num_static: usize,
    capacity: usize,
) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for input in inputs {
        if input.index < num_static || input.index >= capacity {
            return Err(format!(
                "dynamic input {} is outside the free slots {}..{}",
                input.index, num_static, capacity
            ));
        }
        if !seen.insert(input.index) {
            return Err(format!("dynamic input {} is listed twice", input.index));
        }
        match &input.source {
            DynamicInputSource::Inter { channel, .. } if channel.trim().is_empty() => {
                return Err(format!("dynamic input {} has no channel", input.index));
            }
            DynamicInputSource::Inter {
                max_time_ms: Some(ms),
                ..
            } if *ms > MAX_INTER_MAX_TIME_MS => {
                return Err(format!(
                    "dynamic input {}: max_time_ms {} is above {}",
                    input.index, ms, MAX_INTER_MAX_TIME_MS
                ));
            }
            DynamicInputSource::Inter { .. } => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inter(index: usize, channel: &str) -> DynamicInput {
        DynamicInput {
            index,
            source: DynamicInputSource::Inter {
                channel: channel.to_string(),
                max_time_ms: None,
            },
            label: None,
        }
    }

    #[test]
    fn the_property_round_trips() {
        let inputs = vec![
            inter(4, "cam"),
            DynamicInput {
                label: Some("Guest".into()),
                ..inter(6, "guest")
            },
        ];
        let json = serde_json::to_string(&inputs).unwrap();
        assert_eq!(
            json,
            r#"[{"index":4,"source":{"type":"inter","channel":"cam"}},{"index":6,"source":{"type":"inter","channel":"guest"},"label":"Guest"}]"#
        );
        assert_eq!(parse_dynamic_inputs(&json).unwrap(), inputs);
        assert_eq!(parse_dynamic_inputs("").unwrap(), vec![]);
        assert!(parse_dynamic_inputs("{").is_err());
    }

    #[test]
    fn inputs_must_sit_in_free_slots_once() {
        assert!(validate_dynamic_inputs(&[inter(2, "a"), inter(3, "b")], 2, 4).is_ok());
        assert!(validate_dynamic_inputs(&[inter(1, "a")], 2, 4).is_err());
        assert!(validate_dynamic_inputs(&[inter(4, "a")], 2, 4).is_err());
        assert!(validate_dynamic_inputs(&[inter(2, "a"), inter(2, "b")], 2, 4).is_err());
        assert!(validate_dynamic_inputs(&[inter(2, " ")], 2, 4).is_err());
    }

    #[test]
    fn the_buffer_time_is_bounded() {
        let with = |ms| DynamicInput {
            index: 2,
            source: DynamicInputSource::Inter {
                channel: "a".into(),
                max_time_ms: Some(ms),
            },
            label: None,
        };
        assert!(validate_dynamic_inputs(&[with(MAX_INTER_MAX_TIME_MS)], 2, 4).is_ok());
        // Large enough to overflow in nanoseconds.
        assert!(validate_dynamic_inputs(&[with(20_000_000_000_000)], 2, 4).is_err());
    }
}
