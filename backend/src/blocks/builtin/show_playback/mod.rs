//! Show Playback block: sound zones and picture stages, and the sounds and
//! pictures a theatre show plays into them at run time (effects,
//! atmospheres, music; backdrops, projections, videos).
//!
//! The zones are built with the flow; each one is a bus with a block output:
//!
//! ```text
//! zone_mixer_Z (audiomixer, force-live) -> zone_caps_Z -> zone_vol_Z (volume)
//!     -> zone_out_Z (identity) -> [zone_out_Z]
//! ```
//!
//! A sound is a branch added to the running pipeline (see [`sound`]), from
//! its file to a mixer pad of every zone it is routed to:
//!
//! ```text
//! uridecodebin -> convert -> resample -> caps (F32, 48 kHz) -> vol (cue level)
//!     -> tee -> queue -> convert (mix-matrix: file channels -> zone channels)
//!            -> caps (zone channels) -> zone_mixer_Z
//! ```
//!
//! A preloaded sound is decoded up to the cue level, where a blocking probe
//! holds its first buffer; `play` shifts it to the zones' running time and
//! lets it go, so it is heard within the zones' latency. All zones of a
//! block run in one pipeline and on one clock.
//!
//! A stage is a compositor on black with a black dimmer on top, a block
//! output each; pictures are branches like sounds (see [`picture`]):
//!
//! ```text
//! stage_black_S (videotestsrc, live) -> stage_bcaps_S -> stage_mixer_S (compositor)
//!     -> stage_caps_S -> stage_out_S (identity) -> [stage_out_S]
//! ```

pub mod picture;
pub mod sound;

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use strom_types::element::ElementPadRef;
use strom_types::show_playback::{
    ShowStage, ShowZone, MAX_LAYER, MAX_STAGES, MAX_STAGE_PX, MAX_ZONES, MAX_ZONE_CHANNELS,
};
use strom_types::{block::*, FlowId, MediaType, PropertyValue};
use tracing::{info, warn};

/// This block's definition id.
pub const BLOCK_ID: &str = "builtin.show_playback";

/// The property holding the zones, as JSON (`[{"name":"Hall","channels":2}]`).
pub const ZONES_PROPERTY: &str = "zones";

/// The property holding the stages, as JSON
/// (`[{"name":"Back","width":1920,"height":1080}]`); none by default.
pub const STAGES_PROPERTY: &str = "stages";

/// The property holding the stages' frame rate (`30`, `30000/1001`).
pub const FRAMERATE_PROPERTY: &str = "framerate";

/// The compositor pad of a stage's dimmer.
pub(crate) const STAGE_DIMMER_PAD: &str = "sink_0";

/// The dimmer is above every picture layer.
const STAGE_DIMMER_ZORDER: u32 = MAX_LAYER + 1;

/// Rate every zone runs at.
pub const ZONE_RATE: i32 = 48_000;

/// Aggregator latency of a zone (ms): slack for a sound's first buffer.
const ZONE_LATENCY_MS: u64 = 30;

/// The zones of a block nobody has configured.
fn default_zones() -> Vec<ShowZone> {
    vec![ShowZone {
        name: "Main".into(),
        channels: 2,
    }]
}

/// The zones in `properties`, capped to what the block supports. Broken
/// JSON gives the default, with a warning.
pub fn parse_zones(properties: &HashMap<String, PropertyValue>) -> Vec<ShowZone> {
    let Some(PropertyValue::String(json)) = properties.get(ZONES_PROPERTY) else {
        return default_zones();
    };
    let zones: Vec<ShowZone> = match serde_json::from_str(json) {
        Ok(zones) => zones,
        Err(e) => {
            warn!(
                "Show Playback: zones {:?} are not valid JSON ({}), using one stereo zone",
                json, e
            );
            return default_zones();
        }
    };
    let zones: Vec<ShowZone> = zones
        .into_iter()
        .take(MAX_ZONES)
        .map(|z| ShowZone {
            channels: z.channels.clamp(1, MAX_ZONE_CHANNELS),
            ..z
        })
        .collect();
    if zones.is_empty() {
        default_zones()
    } else {
        zones
    }
}

/// The stages in `properties`, sides clamped; broken JSON gives none, with
/// a warning.
pub fn parse_stages(properties: &HashMap<String, PropertyValue>) -> Vec<ShowStage> {
    let Some(PropertyValue::String(json)) = properties.get(STAGES_PROPERTY) else {
        return Vec::new();
    };
    match serde_json::from_str::<Vec<ShowStage>>(json) {
        Ok(stages) => stages
            .into_iter()
            .take(MAX_STAGES)
            .map(|s| ShowStage {
                // Even sides: the compositor's 4:2:0 formats need them.
                width: s.width.clamp(16, MAX_STAGE_PX) & !1,
                height: s.height.clamp(16, MAX_STAGE_PX) & !1,
                ..s
            })
            .collect(),
        Err(e) => {
            warn!(
                "Show Playback: stages {:?} are not valid JSON ({}), no stages",
                json, e
            );
            Vec::new()
        }
    }
}

/// The stages' frame rate; 30 when missing or broken.
pub fn parse_framerate(properties: &HashMap<String, PropertyValue>) -> gst::Fraction {
    let text = match properties.get(FRAMERATE_PROPERTY) {
        Some(PropertyValue::String(s)) => s.clone(),
        Some(PropertyValue::Int(n)) => n.to_string(),
        Some(PropertyValue::UInt(n)) => n.to_string(),
        _ => return gst::Fraction::new(30, 1),
    };
    let (n, d) = text.split_once('/').unwrap_or((text.as_str(), "1"));
    match (n.trim().parse::<i32>(), d.trim().parse::<i32>()) {
        (Ok(n), Ok(d)) if n > 0 && d > 0 && n / d <= 120 => gst::Fraction::new(n, d),
        _ => {
            warn!("Show Playback: framerate {:?} is not valid, using 30", text);
            gst::Fraction::new(30, 1)
        }
    }
}

/// Key of a block's run-time state: block ids are unique within a flow only.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ShowPlaybackKey {
    pub flow_id: FlowId,
    pub block_id: String,
}

/// One block's zones and sounds while its flow runs. Weak handles only: the
/// pipeline owns the elements.
pub struct ShowPlayback {
    pub block_id: String,
    pub zones: Vec<ShowZone>,
    /// Per zone: its mixer.
    pub(crate) mixers: Vec<gst::glib::WeakRef<gst::Element>>,
    /// Per zone: its level.
    pub(crate) zone_volumes: Vec<gst::glib::WeakRef<gst::Element>>,
    pub stages: Vec<ShowStage>,
    /// Per stage: its compositor.
    pub(crate) compositors: Vec<gst::glib::WeakRef<gst::Element>>,
    pub(crate) pictures: Mutex<HashMap<String, Arc<picture::Picture>>>,
    pub(crate) media_path: std::path::PathBuf,
    pub(crate) sounds: Mutex<HashMap<String, Arc<sound::Sound>>>,
    /// Numbers element names of new branches.
    pub(crate) next_branch: std::sync::atomic::AtomicU64,
}

/// Every running Show Playback block, by flow and block id. A flow's next build
/// replaces its entries; a stopped flow's entries no longer reach a pipeline.
pub static SHOW_PLAYBACK_REGISTRY: LazyLock<Mutex<HashMap<ShowPlaybackKey, Arc<ShowPlayback>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The running block `key`, if its pipeline is still there.
pub fn running(key: &ShowPlaybackKey) -> Option<Arc<ShowPlayback>> {
    let registry = SHOW_PLAYBACK_REGISTRY
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let block = registry.get(key)?.clone();
    block.pipeline().map(|_| block)
}

impl ShowPlayback {
    /// The pipeline the zones run in.
    pub(crate) fn pipeline(&self) -> Option<gst::Bin> {
        self.mixers
            .first()?
            .upgrade()?
            .parent()?
            .downcast::<gst::Bin>()
            .ok()
    }

    /// The zones' running time now.
    pub(crate) fn running_time(&self) -> Option<gst::ClockTime> {
        self.mixers.first()?.upgrade()?.current_running_time()
    }
}

pub struct ShowPlaybackBuilder;

impl BlockBuilder for ShowPlaybackBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let zones = parse_zones(properties)
            .into_iter()
            .enumerate()
            .map(|(i, zone)| ExternalPad {
                label: Some(zone.name),
                name: format!("zone_out_{i}"),
                media_type: MediaType::Audio,
                internal_element_id: format!("zone_out_{i}"),
                internal_pad_name: "src".to_string(),
            });
        let stages = parse_stages(properties)
            .into_iter()
            .enumerate()
            .map(|(i, stage)| ExternalPad {
                label: Some(stage.name),
                name: format!("stage_out_{i}"),
                media_type: MediaType::Video,
                internal_element_id: format!("stage_out_{i}"),
                internal_pad_name: "src".to_string(),
            });
        let outputs = zones.chain(stages).collect();
        Some(ExternalPads {
            inputs: vec![],
            outputs,
        })
    }

    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        let zones = parse_zones(properties);
        info!(
            "Building Show Playback block {} with {} zone(s)",
            instance_id,
            zones.len()
        );
        let make = |factory: &str, name: String| {
            gst::ElementFactory::make(factory)
                .name(&name)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", factory, e)))
        };
        let mut elements = Vec::new();
        let mut internal_links = Vec::new();
        let mut mixers = Vec::new();
        let mut zone_volumes = Vec::new();
        for (i, zone) in zones.iter().enumerate() {
            let id = |part: &str| format!("{}:{}_{}", instance_id, part, i);
            let mixer = crate::blocks::builtin::mixer::make_audiomixer(
                &id("zone_mixer"),
                true,
                ZONE_LATENCY_MS,
                ZONE_LATENCY_MS,
                crate::blocks::builtin::start_time::StartTime::Zero,
            )?;
            let caps = make("capsfilter", id("zone_caps"))?;
            caps.set_property("caps", zone_caps(zone.channels));
            let volume = make("volume", id("zone_vol"))?;
            let out = make("identity", id("zone_out"))?;
            mixers.push(mixer.downgrade());
            zone_volumes.push(volume.downgrade());
            for (from, to) in [
                ("zone_mixer", "zone_caps"),
                ("zone_caps", "zone_vol"),
                ("zone_vol", "zone_out"),
            ] {
                internal_links.push((
                    ElementPadRef::pad(id(from), "src"),
                    ElementPadRef::pad(id(to), "sink"),
                ));
            }
            elements.push((id("zone_mixer"), mixer));
            elements.push((id("zone_caps"), caps));
            elements.push((id("zone_vol"), volume));
            elements.push((id("zone_out"), out));
        }

        let stages = parse_stages(properties);
        let framerate = parse_framerate(properties);
        let mut compositors = Vec::new();
        for (i, stage) in stages.iter().enumerate() {
            let id = |part: &str| format!("{}:{}_{}", instance_id, part, i);
            let size = |name: &str| {
                gst::Caps::builder(name)
                    .field("width", stage.width as i32)
                    .field("height", stage.height as i32)
                    .field("framerate", framerate)
                    .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                    .build()
            };
            // The live black keeps the stage running without pictures; it
            // is the dimmer too, on top, clear at full brightness.
            let black = make("videotestsrc", id("stage_black"))?;
            black.set_property("is-live", true);
            black.set_property_from_str("pattern", "black");
            let black_caps = make("capsfilter", id("stage_bcaps"))?;
            black_caps.set_property("caps", size("video/x-raw"));
            let mixer = make("compositor", id("stage_mixer"))?;
            mixer.set_property_from_str("background", "black");
            let dimmer = mixer
                .request_pad_simple("sink_%u")
                .ok_or_else(|| BlockBuildError::ElementCreation("compositor gave no pad".into()))?;
            dimmer.set_property("zorder", STAGE_DIMMER_ZORDER);
            dimmer.set_property("alpha", 0.0f64);
            let caps = make("capsfilter", id("stage_caps"))?;
            caps.set_property("caps", size("video/x-raw"));
            let out = make("identity", id("stage_out"))?;
            compositors.push(mixer.downgrade());
            internal_links.push((
                ElementPadRef::pad(id("stage_black"), "src"),
                ElementPadRef::pad(id("stage_bcaps"), "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(id("stage_bcaps"), "src"),
                ElementPadRef::pad(id("stage_mixer"), dimmer.name().as_str()),
            ));
            for (from, to) in [("stage_mixer", "stage_caps"), ("stage_caps", "stage_out")] {
                internal_links.push((
                    ElementPadRef::pad(id(from), "src"),
                    ElementPadRef::pad(id(to), "sink"),
                ));
            }
            elements.push((id("stage_black"), black));
            elements.push((id("stage_bcaps"), black_caps));
            elements.push((id("stage_mixer"), mixer));
            elements.push((id("stage_caps"), caps));
            elements.push((id("stage_out"), out));
        }

        let flow_id = match properties.get("_flow_id") {
            Some(PropertyValue::String(s)) => s.parse::<FlowId>().ok(),
            _ => None,
        };
        let media_path = match properties.get("_media_path") {
            Some(PropertyValue::String(s)) => std::path::PathBuf::from(s),
            _ => std::path::PathBuf::from("./media"),
        };
        let state = Arc::new(ShowPlayback {
            block_id: instance_id.to_string(),
            zones,
            mixers,
            zone_volumes,
            stages,
            compositors,
            pictures: Mutex::new(HashMap::new()),
            media_path,
            sounds: Mutex::new(HashMap::new()),
            next_branch: std::sync::atomic::AtomicU64::new(0),
        });
        match flow_id {
            Some(flow_id) => {
                SHOW_PLAYBACK_REGISTRY
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(
                        ShowPlaybackKey {
                            flow_id,
                            block_id: instance_id.to_string(),
                        },
                        state,
                    );
            }
            None => warn!(
                "Show Playback {}: no flow id, so its sounds cannot be controlled",
                instance_id
            ),
        }

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// What a zone of `channels` channels carries: 32-bit float at
/// [`ZONE_RATE`], unpositioned channels beyond stereo.
pub(crate) fn zone_caps(channels: u32) -> gst::Caps {
    let mut caps = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("layout", "interleaved")
        .field("rate", ZONE_RATE)
        .field("channels", channels as i32);
    if channels > 2 {
        caps = caps.field("channel-mask", gst::Bitmask::new(0));
    }
    caps.build()
}

pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![definition()]
}

fn definition() -> BlockDefinition {
    BlockDefinition {
        id: BLOCK_ID.to_string(),
        name: "Show Playback".to_string(),
        description: "Sound zones and picture stages for a show, and the sounds and \
                      pictures played into them at run time over the API: preloaded to \
                      start at once, sounds routed channel by channel, pictures on \
                      layers with an opacity, faded along an S-curve."
            .to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: STAGES_PROPERTY.to_string(),
                label: "Stages".to_string(),
                description: format!(
                    "The stages as JSON, e.g. [{{\"name\":\"Back\",\"width\":1920,\
                     \"height\":1080}}]: up to {} stages, one video output each, black \
                     without pictures.",
                    MAX_STAGES
                ),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("[]".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: STAGES_PROPERTY.to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: FRAMERATE_PROPERTY.to_string(),
                label: "Stage frame rate".to_string(),
                description: "Frame rate of every stage, e.g. 30 or 30000/1001.".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("30".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: FRAMERATE_PROPERTY.to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: ZONES_PROPERTY.to_string(),
                label: "Zones".to_string(),
                description: format!(
                    "The zones as JSON, e.g. [{{\"name\":\"Hall\",\"channels\":2}}]: up to {} \
                 zones of 1 to {} channels, one output each.",
                    MAX_ZONES, MAX_ZONE_CHANNELS
                ),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(
                    r#"[{"name":"Main","channels":2}]"#.to_string(),
                )),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: ZONES_PROPERTY.to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![ExternalPad {
                label: Some("Main".to_string()),
                name: "zone_out_0".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "zone_out_0".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(zones: &str) -> HashMap<String, PropertyValue> {
        [(
            ZONES_PROPERTY.to_string(),
            PropertyValue::String(zones.into()),
        )]
        .into()
    }

    #[test]
    fn zones_are_capped_and_default_when_broken() {
        let zones = parse_zones(&props(
            r#"[{"name":"Hall","channels":2},{"name":"Foyer","channels":30}]"#,
        ));
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[1].channels, MAX_ZONE_CHANNELS);
        let many: Vec<String> = (0..20)
            .map(|i| format!(r#"{{"name":"Z{i}","channels":1}}"#))
            .collect();
        let many = parse_zones(&props(&format!("[{}]", many.join(","))));
        assert_eq!(many.len(), 16, "lmix shows have up to 16 zones");
        assert_eq!(parse_zones(&props("nope")), default_zones());
        assert_eq!(parse_zones(&props("[]")), default_zones());
        assert_eq!(parse_zones(&HashMap::new()), default_zones());
    }

    #[test]
    fn stages_are_capped_even_and_none_by_default() {
        let mut props = HashMap::new();
        props.insert(
            STAGES_PROPERTY.to_string(),
            PropertyValue::String(
                r#"[{"name":"Back","width":1919,"height":1080},{"name":"Tiny","width":2,"height":99999}]"#
                    .into(),
            ),
        );
        let stages = parse_stages(&props);
        assert_eq!((stages[0].width, stages[0].height), (1918, 1080));
        assert_eq!((stages[1].width, stages[1].height), (16, MAX_STAGE_PX));
        assert!(parse_stages(&HashMap::new()).is_empty());
        props.insert(
            STAGES_PROPERTY.to_string(),
            PropertyValue::String("{".into()),
        );
        assert!(parse_stages(&props).is_empty());
    }

    #[test]
    fn framerates_read_as_fractions() {
        let rate = |text: &str| {
            parse_framerate(&HashMap::from([(
                FRAMERATE_PROPERTY.to_string(),
                PropertyValue::String(text.into()),
            )]))
        };
        assert_eq!(rate("25"), gst::Fraction::new(25, 1));
        assert_eq!(rate("30000/1001"), gst::Fraction::new(30000, 1001));
        assert_eq!(rate("0"), gst::Fraction::new(30, 1));
        assert_eq!(rate("fast"), gst::Fraction::new(30, 1));
    }

    #[test]
    fn stages_are_video_outputs_after_the_zones() {
        let mut properties = props(r#"[{"name":"Hall","channels":2}]"#);
        properties.insert(
            STAGES_PROPERTY.to_string(),
            PropertyValue::String(r#"[{"name":"Back","width":1920,"height":1080}]"#.into()),
        );
        let pads = ShowPlaybackBuilder.get_external_pads(&properties).unwrap();
        let names: Vec<_> = pads.outputs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["zone_out_0", "stage_out_0"]);
        assert_eq!(pads.outputs[1].media_type, MediaType::Video);
    }

    #[test]
    fn each_zone_is_an_output() {
        let pads = ShowPlaybackBuilder
            .get_external_pads(&props(
                r#"[{"name":"Hall","channels":2},{"name":"Foyer","channels":1}]"#,
            ))
            .unwrap();
        let names: Vec<_> = pads.outputs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["zone_out_0", "zone_out_1"]);
        assert_eq!(pads.outputs[1].label.as_deref(), Some("Foyer"));
    }
}
