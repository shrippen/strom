//! Default values for the audio mixer block.
//!
//! Single source of truth shared by both backend and frontend.

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

// ── Channel / Main bus processing defaults ──────────────────────────
pub const DEFAULT_FADER: f32 = 1.0;
pub const DEFAULT_GAIN: f32 = 0.0;
pub const DEFAULT_PAN: f32 = 0.0;

// HPF
pub const DEFAULT_HPF_FREQ: f32 = 80.0;

// Gate
pub const DEFAULT_GATE_THRESHOLD: f32 = -40.0;
pub const DEFAULT_GATE_ATTACK: f32 = 5.0;
pub const DEFAULT_GATE_RELEASE: f32 = 100.0;

// Compressor (shared between channel and main bus)
pub const DEFAULT_COMP_THRESHOLD: f32 = -20.0;
pub const DEFAULT_COMP_RATIO: f32 = 4.0;
pub const DEFAULT_COMP_ATTACK: f32 = 10.0;
pub const DEFAULT_COMP_RELEASE: f32 = 100.0;
pub const DEFAULT_COMP_MAKEUP: f32 = 0.0;
pub const DEFAULT_COMP_KNEE: f32 = -6.0;

// EQ bands: (freq Hz, gain dB, Q)
pub const DEFAULT_EQ_BANDS: [(f32, f32, f32); 4] = [
    (80.0, 0.0, 1.0),   // Low
    (400.0, 0.0, 1.0),  // Low-mid
    (2000.0, 0.0, 1.0), // High-mid
    (8000.0, 0.0, 1.0), // High
];

// Limiter
pub const DEFAULT_LIMITER_THRESHOLD: f32 = -3.0;

// ── Structural defaults ─────────────────────────────────────────────
pub const DEFAULT_CHANNELS: usize = 8;
pub const MAX_CHANNELS: usize = 128;
pub const MAX_AUX_BUSES: usize = 32;
pub const MAX_GROUPS: usize = 32;

// ── Routing defaults ──────────────────────────────────────────────
/// Default aux send pre/post-fader mode per bus.
/// All aux buses default to post-fader (FX/headphone sends, affected by
/// the channel fader). Flip individual buses to pre-fader in the UI when
/// using them for monitor/IEM sends.
pub const DEFAULT_AUX_PRE: [bool; MAX_AUX_BUSES] = [false; MAX_AUX_BUSES];

/// Minimum compressor knee value in linear scale (corresponds to -24 dB)
pub const MIN_KNEE_LINEAR: f64 = 0.0631;

// ── Latency / live defaults ─────────────────────────────────────────
pub const DEFAULT_LATENCY_MS: u64 = 30;
pub const DEFAULT_MIN_UPSTREAM_LATENCY_MS: u64 = 30;
/// Default aggregator latency of the Solo and Monitor buses, and of Main when
/// the mixer has groups: buses that sum other buses of the same mixer. Capped
/// at the block `latency`.
pub const DEFAULT_INTERNAL_BUS_LATENCY_MS: u64 = 30;

// ── Volume ramp / anti-zipper defaults ──────────────────────────────
/// Default ramp duration applied to `volume`-element `volume` updates
/// when no explicit `ramp_ms` is provided. Eliminates zipper noise from
/// fader-drag updates without making the fader feel laggy.
pub const DEFAULT_VOLUME_RAMP_MS: u32 = 20;

/// Anti-click ramp applied automatically when `mute` is toggled. Not
/// user-configurable — short enough to feel instant, long enough to
/// prevent the discontinuity click of a hard mute.
pub const MUTE_ANTICLICK_RAMP_MS: u32 = 30;

// ── Dynamic aux buses ───────────────────────────────────────────────
//
// A mixer builds its aux buses `1..=num_aux_buses` with the flow; they leave
// through the block pads `aux_out_N`. With `max_aux_buses` above
// `num_aux_buses`, the numbers up to `max_aux_buses` are free: an API call
// adds a bus there while the flow runs (and removes it again), without
// rebuilding the mixer. A dynamic bus publishes its output on an inter
// channel instead of a block pad, so adding one needs no flow link. Dynamic
// buses are stored in the block property `dynamic_aux_buses`, so a restart
// builds them again under the same numbers.

/// Block property: highest aux bus number (bus capacity). Missing or below
/// `num_aux_buses` means `num_aux_buses` (a mixer without free buses).
pub const MAX_AUX_BUSES_PROPERTY: &str = "max_aux_buses";

/// Block property: the dynamic aux buses, as a JSON list of
/// [`DynamicAuxBus`].
pub const DYNAMIC_AUX_BUSES_PROPERTY: &str = "dynamic_aux_buses";

/// An aux bus added while the flow runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct DynamicAuxBus {
    /// Bus number, as in the properties `auxN_fader` and `chM_auxN_level`
    /// (`num_aux_buses < aux <= max_aux_buses`).
    pub aux: usize,
    /// Inter channel the bus's output is published on (the producer name an
    /// Inter Input block or `intersrc` subscribes to).
    pub channel: String,
}

/// Request to add an aux bus to a running mixer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct AddMixerAuxBusRequest {
    /// Bus number to fill; `None` takes the lowest free one.
    #[serde(default)]
    pub aux: Option<usize>,
    /// Inter channel to publish the bus's output on.
    pub channel: String,
    /// Tap every channel's send before the fader (`true`) or after it.
    /// Sets each `chM_auxN_pre`; `None` keeps the stored values (post fader
    /// when there are none).
    #[serde(default)]
    pub pre_fader: Option<bool>,
}

/// The aux buses of a running mixer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct MixerAuxBuses {
    /// Highest bus number (`max_aux_buses`).
    pub capacity: usize,
    /// Buses built with the flow (`num_aux_buses`): they cannot be removed.
    pub num_static: usize,
    /// The dynamic buses, ascending by number.
    pub dynamic: Vec<DynamicAuxBus>,
}

/// Parse the `dynamic_aux_buses` property. An empty string is no buses.
pub fn parse_dynamic_aux_buses(json: &str) -> Result<Vec<DynamicAuxBus>, String> {
    if json.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(json).map_err(|e| format!("dynamic_aux_buses is not valid: {}", e))
}

/// Check dynamic buses against the mixer's numbers: each in
/// `num_static + 1..=capacity`, no number or channel twice, no empty channel.
pub fn validate_dynamic_aux_buses(
    buses: &[DynamicAuxBus],
    num_static: usize,
    capacity: usize,
) -> Result<(), String> {
    let mut numbers = std::collections::BTreeSet::new();
    let mut channels = std::collections::BTreeSet::new();
    for bus in buses {
        if bus.aux <= num_static || bus.aux > capacity {
            return Err(format!(
                "aux bus {} is outside the free buses {}..={}",
                bus.aux,
                num_static + 1,
                capacity
            ));
        }
        if !numbers.insert(bus.aux) {
            return Err(format!("aux bus {} is listed twice", bus.aux));
        }
        if bus.channel.trim().is_empty() {
            return Err(format!("aux bus {} has no channel", bus.aux));
        }
        if !channels.insert(bus.channel.as_str()) {
            return Err(format!(
                "aux bus {}: channel {} is taken by another bus",
                bus.aux, bus.channel
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus(aux: usize, channel: &str) -> DynamicAuxBus {
        DynamicAuxBus {
            aux,
            channel: channel.to_string(),
        }
    }

    #[test]
    fn dynamic_aux_buses_round_trip() {
        let buses = vec![bus(2, "stream"), bus(5, "hall")];
        let json = serde_json::to_string(&buses).unwrap();
        assert_eq!(parse_dynamic_aux_buses(&json).unwrap(), buses);
        assert_eq!(parse_dynamic_aux_buses(" ").unwrap(), Vec::new());
        assert!(parse_dynamic_aux_buses("[{\"aux\":2}]").is_err());
    }

    #[test]
    fn dynamic_aux_buses_stay_in_the_free_numbers() {
        assert!(validate_dynamic_aux_buses(&[bus(2, "a"), bus(4, "b")], 1, 4).is_ok());
        assert!(validate_dynamic_aux_buses(&[bus(1, "a")], 1, 4).is_err());
        assert!(validate_dynamic_aux_buses(&[bus(5, "a")], 1, 4).is_err());
        assert!(validate_dynamic_aux_buses(&[bus(2, "a"), bus(2, "b")], 1, 4).is_err());
        assert!(validate_dynamic_aux_buses(&[bus(2, "a"), bus(3, "a")], 1, 4).is_err());
        assert!(validate_dynamic_aux_buses(&[bus(2, " ")], 1, 4).is_err());
    }
}
