use std::collections::HashMap;

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::PropertyValue;

use super::{DEFAULT_CHANNELS, MAX_AUX_BUSES, MAX_CHANNELS, MAX_GROUPS};

/// Parse number of channels from properties.
pub(super) fn parse_num_channels(properties: &HashMap<String, PropertyValue>) -> usize {
    properties
        .get("num_channels")
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some(*i as usize),
            PropertyValue::UInt(u) => Some(*u as usize),
            PropertyValue::String(s) => s.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(DEFAULT_CHANNELS)
        .clamp(1, MAX_CHANNELS)
}

/// Parse number of aux buses from properties.
pub(super) fn parse_num_aux_buses(properties: &HashMap<String, PropertyValue>) -> usize {
    properties
        .get("num_aux_buses")
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some(*i as usize),
            PropertyValue::UInt(u) => Some(*u as usize),
            PropertyValue::String(s) => s.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(0)
        .clamp(0, MAX_AUX_BUSES)
}

/// Parse number of groups from properties.
pub(super) fn parse_num_groups(properties: &HashMap<String, PropertyValue>) -> usize {
    properties
        .get("num_groups")
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some(*i as usize),
            PropertyValue::UInt(u) => Some(*u as usize),
            PropertyValue::String(s) => s.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(0)
        .clamp(0, MAX_GROUPS)
}

/// Highest bus sample rate the mixer accepts (Hz).
const MAX_SAMPLE_RATE: u64 = 384_000;

/// Parse the bus sample rate in Hz; 0 (default) leaves it to the aggregators.
/// Values above [`MAX_SAMPLE_RATE`] fall back to 0.
pub(super) fn parse_sample_rate(properties: &HashMap<String, PropertyValue>) -> i32 {
    match get_u64_prop(properties, "sample_rate", 0) {
        rate @ 1..=MAX_SAMPLE_RATE => rate as i32,
        _ => 0,
    }
}

/// Get a float property with default.
pub(super) fn get_float_prop(
    properties: &HashMap<String, PropertyValue>,
    name: &str,
    default: f64,
) -> f64 {
    properties
        .get(name)
        .and_then(|v| match v {
            PropertyValue::Float(f) => Some(*f),
            PropertyValue::Int(i) => Some(*i as f64),
            _ => None,
        })
        .unwrap_or(default)
}

/// Get a u64 property with default.
pub(super) fn get_u64_prop(
    properties: &HashMap<String, PropertyValue>,
    name: &str,
    default: u64,
) -> u64 {
    properties
        .get(name)
        .and_then(|v| match v {
            PropertyValue::UInt(u) => Some(*u),
            PropertyValue::Int(i) => Some(*i as u64),
            PropertyValue::Float(f) => Some(*f as u64),
            PropertyValue::String(s) => s.parse::<u64>().ok(),
            _ => None,
        })
        .unwrap_or(default)
}

/// Get a bool property with default.
pub(super) fn get_bool_prop(
    properties: &HashMap<String, PropertyValue>,
    name: &str,
    default: bool,
) -> bool {
    properties
        .get(name)
        .and_then(|v| match v {
            PropertyValue::Bool(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(default)
}

/// Get a string property with default.
pub(super) fn get_string_prop<'a>(
    properties: &'a HashMap<String, PropertyValue>,
    name: &str,
    default: &'a str,
) -> &'a str {
    properties
        .get(name)
        .and_then(|v| match v {
            PropertyValue::String(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or(default)
}

/// Convert dB to linear scale.
pub(super) fn db_to_linear(db: f64) -> f64 {
    10.0_f64.powf(db / 20.0)
}

/// Convert linear scale to dB.
pub(super) fn linear_to_db(linear: f64) -> f64 {
    if linear <= 0.0 {
        -120.0 // floor
    } else {
        20.0 * linear.log10()
    }
}

/// Translate a property name and value from LV2 conventions to lsp-rs conventions.
///
/// The ExposedProperty mappings use LV2 property names (gt, at, rt, al, cr, mk, kn, th, f-N, g-N, q-N).
/// When the target element is from lsp-plugins-rs, this function translates the property name
/// and adjusts the value format where needed (e.g., LV2 uses linear gain, Rust uses dB).
///
/// Returns a list of (translated_prop_name, translated_value) pairs, or empty if no translation needed.
/// May return multiple pairs when one LV2 property maps to multiple Rust properties.
pub fn translate_property_for_element(
    element: &gst::Element,
    prop_name: &str,
    value: &PropertyValue,
) -> Vec<(String, PropertyValue)> {
    // Use GObject type name instead of factory() which can SIGSEGV
    // when static plugins and LV2 plugins coexist.
    let type_name = element.type_().name();

    if type_name == "LspRsGate" {
        let pairs = match prop_name {
            "gt" => {
                // LV2: gt is linear (already transformed by db_to_linear).
                // Rust: open-threshold/close-threshold are dB. Reverse the transform.
                let db_val = match value {
                    PropertyValue::Float(v) => linear_to_db(*v),
                    _ => return vec![],
                };
                vec![
                    ("open-threshold".to_string(), PropertyValue::Float(db_val)),
                    ("close-threshold".to_string(), PropertyValue::Float(db_val)),
                ]
            }
            "at" => vec![("attack".to_string(), value.clone())],
            "rt" => vec![("release".to_string(), value.clone())],
            "enabled" => return vec![], // same name, no translation needed
            _ => return vec![],
        };
        return pairs;
    }

    if type_name == "LspRsCompressor" {
        let pair = match prop_name {
            "al" => ("threshold".to_string(), value.clone()),
            "cr" => ("ratio".to_string(), value.clone()),
            "at" => ("attack".to_string(), value.clone()),
            "rt" => ("release".to_string(), value.clone()),
            "mk" => ("makeup-gain".to_string(), value.clone()),
            "kn" => ("knee".to_string(), value.clone()),
            "enabled" => return vec![],
            _ => return vec![],
        };
        return vec![pair];
    }

    if type_name == "LspRsEqualizer" {
        // EQ band properties: f-N -> bandN-frequency, g-N -> bandN-gain, q-N -> bandN-q
        if let Some(band) = prop_name.strip_prefix("f-") {
            return vec![(format!("band{}-frequency", band), value.clone())];
        }
        if let Some(band) = prop_name.strip_prefix("g-") {
            // LV2: g-N is linear (already transformed by db_to_linear).
            // Rust: bandN-gain is dB. Reverse the transform.
            let db_val = match value {
                PropertyValue::Float(v) => linear_to_db(*v),
                _ => return vec![],
            };
            return vec![(format!("band{}-gain", band), PropertyValue::Float(db_val))];
        }
        if let Some(band) = prop_name.strip_prefix("q-") {
            return vec![(format!("band{}-q", band), value.clone())];
        }
        return vec![];
    }

    if type_name == "LspRsLimiter" {
        let pair = match prop_name {
            "th" => {
                // LV2: th is linear (already transformed by db_to_linear).
                // Rust: threshold is dB. Reverse the transform.
                let db_val = match value {
                    PropertyValue::Float(v) => linear_to_db(*v),
                    _ => return vec![],
                };
                ("threshold".to_string(), PropertyValue::Float(db_val))
            }
            "enabled" => return vec![],
            _ => return vec![],
        };
        return vec![pair];
    }

    vec![]
}
