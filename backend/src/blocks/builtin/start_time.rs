//! Where an aggregator's (compositor, audiomixer) output running time
//! starts: block property `start_time_selection`, shared by the vision
//! mixer and the audio mixer.

use gstreamer as gst;
use std::collections::HashMap;
use strom_types::{block::*, EnumValue, PropertyValue};

const PROPERTY: &str = "start_time_selection";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartTime {
    /// At zero: flows with their own base time (default).
    Zero,
    /// At the first input buffer: flows with base time 0
    /// (`direct_media_timing`), e.g. inputs coupled over `inter`, whose
    /// timestamps are clock time. "zero" would make the aggregator chase
    /// clock time from 0 forever.
    First,
}

impl StartTime {
    /// Value of the aggregator's `start-time-selection`.
    pub fn nick(self) -> &'static str {
        match self {
            Self::Zero => "zero",
            Self::First => "first",
        }
    }
}

/// Turn off an aggregator's start time selection when it starts at zero
/// (see [`crate::gst::aggregator_start::disarm_start_time_selection`]).
/// "first" needs the selection: it is what picks the first buffer.
pub(crate) fn disarm_zero_start(element: &gst::Element, start: StartTime) {
    if start == StartTime::Zero {
        crate::gst::aggregator_start::disarm_start_time_selection(element);
    }
}

pub fn parse_start_time(properties: &HashMap<String, PropertyValue>) -> StartTime {
    match properties.get(PROPERTY) {
        Some(PropertyValue::String(s)) if s == "first" => StartTime::First,
        _ => StartTime::Zero,
    }
}

pub fn start_time_property() -> ExposedProperty {
    ExposedProperty {
        name: PROPERTY.to_string(),
        label: "Start Time".to_string(),
        description: "Where output running time starts: Zero (default) or First buffer (for flows with direct media timing, e.g. inputs over inter)".to_string(),
        property_type: PropertyType::Enum {
            values: vec![
                EnumValue {
                    value: "zero".to_string(),
                    label: Some("Zero".to_string()),
                },
                EnumValue {
                    value: "first".to_string(),
                    label: Some("First buffer".to_string()),
                },
            ],
        },
        default_value: Some(PropertyValue::String("zero".to_string())),
        mapping: PropertyMapping {
            element_id: "_block".to_string(),
            property_name: PROPERTY.to_string(),
            transform: None,
        },
        live: false,
        persist: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_zero() {
        let mut props = HashMap::new();
        assert_eq!(parse_start_time(&props), StartTime::Zero);

        props.insert(
            PROPERTY.to_string(),
            PropertyValue::String("first".to_string()),
        );
        assert_eq!(parse_start_time(&props), StartTime::First);
    }
}
