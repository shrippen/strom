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
    /// Stored buses the mixer does not build: they no longer fit the
    /// numbers (`num_aux_buses`, `max_aux_buses`). They come back when the
    /// numbers fit again; removing one takes it out of the store.
    #[serde(default)]
    pub unbuilt: Vec<DynamicAuxBus>,
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

/// The buses of `buses` a mixer can build, in order, and why each other one
/// is left out: outside `num_static + 1..=capacity`, a number or channel
/// already taken by an earlier entry, or no channel. A stored list that
/// stopped fitting (the operator changed `num_aux_buses` or `max_aux_buses`)
/// so loses only the buses that no longer fit, and keeps them stored.
pub fn buildable_dynamic_aux_buses(
    buses: &[DynamicAuxBus],
    num_static: usize,
    capacity: usize,
) -> (Vec<DynamicAuxBus>, Vec<String>) {
    let mut built: Vec<DynamicAuxBus> = Vec::new();
    let mut skipped = Vec::new();
    for bus in buses {
        let mut with = built.clone();
        with.push(bus.clone());
        match validate_dynamic_aux_buses(&with, num_static, capacity) {
            Ok(()) => built = with,
            Err(reason) => skipped.push(reason),
        }
    }
    (built, skipped)
}

/// Whether block property `name` belongs to aux bus `aux` (1-based): its
/// master (`auxN_…`) or a send into it (`chM_auxN_…`). They go with the bus.
pub fn is_aux_bus_property(name: &str, aux: usize) -> bool {
    let master = format!("aux{}_", aux);
    let send = format!("_aux{}_", aux);
    name.starts_with(&master) || (name.starts_with("ch") && name.contains(&send))
}

/// Carry the dynamic aux buses of a mixer's stored properties `old` over
/// into `new`, properties written by a client that does not know them (the
/// mixer editor): `max_aux_buses`, `dynamic_aux_buses` and, with
/// `settings`, every property of a stored dynamic bus that `new` does not
/// set. Without it, saving the editor would drop buses added at runtime.
pub fn keep_dynamic_aux_buses(
    old: &std::collections::HashMap<String, crate::PropertyValue>,
    new: &mut std::collections::HashMap<String, crate::PropertyValue>,
    settings: bool,
) {
    for name in [MAX_AUX_BUSES_PROPERTY, DYNAMIC_AUX_BUSES_PROPERTY] {
        if let Some(value) = old.get(name) {
            new.entry(name.to_string()).or_insert_with(|| value.clone());
        }
    }
    if !settings {
        return;
    }
    let Some(crate::PropertyValue::String(json)) = old.get(DYNAMIC_AUX_BUSES_PROPERTY) else {
        return;
    };
    let Ok(buses) = parse_dynamic_aux_buses(json) else {
        return;
    };
    // A bus whose number the flow now builds itself is the editor's.
    let num_static = match new
        .get("num_aux_buses")
        .or_else(|| old.get("num_aux_buses"))
    {
        Some(crate::PropertyValue::Int(n)) => usize::try_from(*n).unwrap_or(0),
        Some(crate::PropertyValue::UInt(n)) => usize::try_from(*n).unwrap_or(0),
        _ => 0,
    };
    for (name, value) in old {
        if buses
            .iter()
            .any(|bus| bus.aux > num_static && is_aux_bus_property(name, bus.aux))
        {
            new.entry(name.clone()).or_insert_with(|| value.clone());
        }
    }
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

    #[test]
    fn buses_that_stopped_fitting_are_left_out_one_by_one() {
        let stored = [
            bus(2, "stage"),
            bus(4, "stream"),
            bus(3, "stage"),
            bus(5, "x"),
        ];
        let (built, skipped) = buildable_dynamic_aux_buses(&stored, 2, 4);
        // Bus 2 is static now and 5 beyond the capacity; 3 takes the
        // channel of 2, which is left out.
        assert_eq!(built, [bus(4, "stream"), bus(3, "stage")]);
        assert_eq!(skipped.len(), 2, "{skipped:?}");
    }

    #[test]
    fn the_editor_keeps_the_dynamic_buses() {
        use crate::PropertyValue;
        use std::collections::HashMap;
        let json = serde_json::to_string(&[bus(3, "stream")]).unwrap();
        let old: HashMap<String, PropertyValue> = [
            (MAX_AUX_BUSES_PROPERTY, PropertyValue::UInt(4)),
            (DYNAMIC_AUX_BUSES_PROPERTY, PropertyValue::String(json)),
            ("aux3_fader", PropertyValue::Float(0.5)),
            ("ch1_aux3_level", PropertyValue::Float(0.7)),
            ("aux1_fader", PropertyValue::Float(0.2)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let mut saved: HashMap<String, PropertyValue> =
            [("ch1_aux3_level".to_string(), PropertyValue::Float(0.9))].into();
        keep_dynamic_aux_buses(&old, &mut saved, true);
        let float = |name: &str| match saved.get(name) {
            Some(PropertyValue::Float(f)) => Some(*f),
            _ => None,
        };
        assert_eq!(float("aux3_fader"), Some(0.5));
        assert_eq!(
            float("ch1_aux3_level"),
            Some(0.9),
            "the editor's value wins"
        );
        assert!(saved.contains_key(DYNAMIC_AUX_BUSES_PROPERTY));
        assert!(
            !saved.contains_key("aux1_fader"),
            "a static bus is the editor's"
        );

        let mut reset = HashMap::new();
        keep_dynamic_aux_buses(&old, &mut reset, false);
        assert_eq!(reset.len(), 2);
    }

    #[test]
    fn a_bus_now_static_is_the_editors() {
        use crate::PropertyValue;
        use std::collections::HashMap;
        let json = serde_json::to_string(&[bus(2, "stage")]).unwrap();
        let old: HashMap<String, PropertyValue> = [
            (DYNAMIC_AUX_BUSES_PROPERTY, PropertyValue::String(json)),
            ("aux2_fader", PropertyValue::Float(0.5)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        // The editor shows bus 2 and leaves its fader at the default out.
        let mut saved: HashMap<String, PropertyValue> =
            [("num_aux_buses".to_string(), PropertyValue::Int(2))].into();
        keep_dynamic_aux_buses(&old, &mut saved, true);
        assert!(!saved.contains_key("aux2_fader"));
    }
}
