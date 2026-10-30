//! Blackmagic DeckLink SDI/HDMI capture and playback block builders.
//!
//! Provides two combined blocks — one input, one output — both selectable
//! between `audio_video`, `video`, or `audio` via the `stream_mode` property:
//!
//! - **DeckLink Input** — always creates `decklinkvideosrc` because the plugin
//!   requires it for `decklinkaudiosrc` to operate on the same device. In
//!   `audio` mode the video src is drained internally to a `fakesink`.
//! - **DeckLink Output** — `decklink2sink` from GStreamer's decklink2 plugin
//!   (GStreamer 1.30). It takes sound only as part of the picture, so with
//!   sound a `decklink2combiner` puts both together. In `audio` mode black
//!   frames (`videotestsrc`, 1920x1080@25 UYVY) carry the sound.
//!
//! No format conversion elements are inserted on either side — peer blocks must
//! accept (input) or deliver (output) the card's native pixel/audio formats.
//!
//! The input uses GStreamer's decklink plugin, the output its decklink2
//! plugin (both gst-plugins-bad). The old `decklinkvideosink` shows every
//! frame late once an output starts a second time in the same process
//! (GStreamer work item 5391); decklink2 replaces it.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use strom_types::{
    block::{StreamMode, *},
    element::ElementPadRef,
    EnumValue, MediaType, PropertyValue,
};
use tracing::info;

/// DeckLink Input block builder.
///
/// Combined video + audio capture. The `stream_mode` property controls which pads
/// are exposed (`audio_video`, `video`, `audio`). `decklinkvideosrc` is always
/// created internally — required by the DeckLink GStreamer plugin even when only
/// audio is consumed externally.
pub struct DeckLinkInputBuilder;

impl BlockBuilder for DeckLinkInputBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let mode = properties
            .get("stream_mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(StreamMode::parse(s)),
                _ => None,
            })
            .unwrap_or_default();

        let mut outputs = Vec::new();
        if mode.has_video() {
            outputs.push(ExternalPad {
                label: if mode.has_audio() {
                    Some("V".to_string())
                } else {
                    None
                },
                name: "video_out".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "decklinkvideosrc".to_string(),
                internal_pad_name: "src".to_string(),
            });
        }
        if mode.has_audio() {
            outputs.push(ExternalPad {
                label: if mode.has_video() {
                    Some("A".to_string())
                } else {
                    None
                },
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "decklinkaudiosrc".to_string(),
                internal_pad_name: "src".to_string(),
            });
        }

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
        info!("Building DeckLink Input block: {}", instance_id);

        let stream_mode = properties
            .get("stream_mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(StreamMode::parse(s)),
                _ => None,
            })
            .unwrap_or_default();

        let device_number = properties
            .get("device_number")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as i32),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as i32),
                _ => None,
            })
            .unwrap_or(0);

        let video_mode = properties
            .get("mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("auto");

        let connection = properties
            .get("connection")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("auto");

        let video_format = properties
            .get("video_format")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("auto");

        let drop_no_signal_frames = properties
            .get("drop_no_signal_frames")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);

        let synchronized_capture = properties
            .get("synchronized_capture")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);

        let videosrc_id = format!("{}:decklinkvideosrc", instance_id);
        let videosrc = gst::ElementFactory::make("decklinkvideosrc")
            .name(&videosrc_id)
            .property("device-number", device_number)
            .property_from_str("mode", video_mode)
            .property_from_str("connection", connection)
            .property_from_str("video-format", video_format)
            .property("drop-no-signal-frames", drop_no_signal_frames)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("decklinkvideosrc: {}", e)))?;

        // BMD synced capture groups only work for inputs armed atomically as
        // a unit, which means all members must live in the same pipeline. We
        // derive the capture group ID from the flow ID so two synchronized_
        // capture blocks in the same flow always land in the same group, and
        // can never collide with another flow's group on the same card.
        // The capture-group property only exists on our patched plugin; if
        // the runtime has the stock plugin we just skip silently.
        if synchronized_capture && videosrc.has_property("capture-group") {
            let flow_id = properties
                .get("_flow_id")
                .and_then(|v| match v {
                    PropertyValue::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            let mut hasher = DefaultHasher::new();
            flow_id.hash(&mut hasher);
            let group_id = ((hasher.finish() & 0x7FFF_FFFF) as i32).max(0);
            videosrc.set_property("capture-group", group_id);
        }

        let mut elements: Vec<(String, gst::Element)> = vec![(videosrc_id.clone(), videosrc)];
        let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();

        // Audio-only: the video src must still run, drain it to a fakesink so the
        // pipeline can reach PLAYING. Without this, decklinkaudiosrc fails state
        // change with "Audio src needs a video src for its operation".
        if !stream_mode.has_video() {
            let drain_id = format!("{}:video_drain", instance_id);
            let drain = gst::ElementFactory::make("fakesink")
                .name(&drain_id)
                .property("sync", false)
                .property("async", false)
                .property("silent", true)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("fakesink (video_drain): {}", e))
                })?;
            internal_links.push((
                ElementPadRef::pad(&videosrc_id, "src"),
                ElementPadRef::pad(&drain_id, "sink"),
            ));
            elements.push((drain_id, drain));
        }

        if stream_mode.has_audio() {
            let audio_connection = properties
                .get("audio_connection")
                .and_then(|v| match v {
                    PropertyValue::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .unwrap_or("auto");

            let audio_channels = properties
                .get("audio_channels")
                .and_then(|v| match v {
                    PropertyValue::String(s) => Some(s.clone()),
                    PropertyValue::UInt(u) => Some(u.to_string()),
                    PropertyValue::Int(i) if *i > 0 => Some(i.to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| "2".to_string());

            let audiosrc_id = format!("{}:decklinkaudiosrc", instance_id);
            let audiosrc = gst::ElementFactory::make("decklinkaudiosrc")
                .name(&audiosrc_id)
                .property("device-number", device_number)
                .property_from_str("connection", audio_connection)
                .property_from_str("channels", &audio_channels)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("decklinkaudiosrc: {}", e))
                })?;

            elements.push((audiosrc_id, audiosrc));

            info!(
                "DeckLink Input audio: connection={}, channels={}",
                audio_connection, audio_channels
            );
        }

        info!(
            "DeckLink Input configured: device={}, stream_mode={}, mode={}, connection={}, video-format={}, drop-no-signal-frames={}",
            device_number, stream_mode.as_str(), video_mode, connection, video_format, drop_no_signal_frames
        );

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// DeckLink Output block builder.
///
/// Combined video + audio playback through `decklink2sink`. The `stream_mode`
/// property controls which pads are exposed (`audio_video`, `video`, `audio`).
/// The sink takes sound only attached to the frames, so with sound both go
/// through a `decklink2combiner`; in audio-only mode an internal
/// `videotestsrc` (black, 1920x1080@25 UYVY) gives the sound its frames.
pub struct DeckLinkOutputBuilder;

/// GStreamer's own default for a sink's processing deadline.
const DEFAULT_PROCESSING_DEADLINE_MS: u64 = 20;
/// More lead time than this only delays the picture.
const MAX_PROCESSING_DEADLINE_MS: u64 = 1000;

/// Internal video mode used to carry the sound when only audio is exposed.
/// Lowest broadcast-quality progressive mode supported by virtually all DeckLink
/// cards; the actual SDI output is irrelevant in this mode (just black frames).
const AUDIO_ONLY_INTERNAL_MODE: &str = "1080p25";
const AUDIO_ONLY_INTERNAL_FORMAT: &str = "8bit-yuv";

/// Element ids inside the output block.
const OUTPUT_SINK: &str = "decklink2sink";
const OUTPUT_COMBINER: &str = "decklink2combiner";
/// Mixer in front of the combiner's sound: the block's sound and silence.
const OUTPUT_SOUND: &str = "sound";

/// Channels of the output's sound (`audio_channels`): 2, 8 or 16, as the
/// card embeds them.
fn audio_channels(properties: &HashMap<String, PropertyValue>) -> Result<i32, BlockBuildError> {
    let value = match properties.get("audio_channels") {
        None => return Ok(2),
        Some(PropertyValue::String(s)) => s.parse::<i32>().ok(),
        Some(PropertyValue::UInt(u)) => i32::try_from(*u).ok(),
        Some(PropertyValue::Int(i)) => i32::try_from(*i).ok(),
        Some(_) => None,
    };
    match value {
        Some(n @ (2 | 8 | 16)) => Ok(n),
        _ => Err(BlockBuildError::ElementCreation(format!(
            "audio_channels {:?}: the card takes 2, 8 or 16",
            properties.get("audio_channels")
        ))),
    }
}

/// `factory`'s element named `name`; for a missing one an error that says
/// where the plugin comes from.
fn card_element(factory: &str, name: &str) -> Result<gst::Element, BlockBuildError> {
    if gst::ElementFactory::find(factory).is_none() {
        return Err(BlockBuildError::ElementCreation(format!(
            "{factory} not found: the DeckLink output needs GStreamer's decklink2 plugin \
             (GStreamer 1.30 or later, or built from GStreamer main and put on GST_PLUGIN_PATH)"
        )));
    }
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{factory}: {e}")))
}

/// Whether `element`'s enum property `property` has the value `nick`.
fn enum_has_nick(element: &gst::Element, property: &str, nick: &str) -> bool {
    element
        .find_property(property)
        .and_then(|pspec| gst::glib::EnumClass::with_type(pspec.value_type()))
        .is_some_and(|class| class.value_by_nick(nick).is_some())
}

impl BlockBuilder for DeckLinkOutputBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let mode = properties
            .get("stream_mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(StreamMode::parse(s)),
                _ => None,
            })
            .unwrap_or_default();

        let mut inputs = Vec::new();
        if mode.has_video() {
            let (element, pad) = if mode.has_audio() {
                (OUTPUT_COMBINER, "video")
            } else {
                (OUTPUT_SINK, "sink")
            };
            inputs.push(ExternalPad {
                label: if mode.has_audio() {
                    Some("V".to_string())
                } else {
                    None
                },
                name: "video_in".to_string(),
                media_type: MediaType::Video,
                internal_element_id: element.to_string(),
                internal_pad_name: pad.to_string(),
            });
        }
        if mode.has_audio() {
            inputs.push(ExternalPad {
                label: if mode.has_video() {
                    Some("A".to_string())
                } else {
                    None
                },
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: OUTPUT_SOUND.to_string(),
                internal_pad_name: "sink_0".to_string(),
            });
        }

        Some(ExternalPads {
            inputs,
            outputs: vec![],
        })
    }

    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building DeckLink Output block: {}", instance_id);

        let stream_mode = properties
            .get("stream_mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(StreamMode::parse(s)),
                _ => None,
            })
            .unwrap_or_default();

        let device_number = properties
            .get("device_number")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as i32),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as i32),
                _ => None,
            })
            .unwrap_or(0);

        // In audio-only mode the sink runs in a fixed internal mode regardless
        // of the user's `mode`/`video_format` settings — those only apply when the
        // video pad is actually wired up.
        let (video_mode, video_format) = if stream_mode.has_video() {
            let m = properties
                .get("mode")
                .and_then(|v| match v {
                    PropertyValue::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .unwrap_or("1080p25");
            let f = properties
                .get("video_format")
                .and_then(|v| match v {
                    PropertyValue::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .unwrap_or("8bit-yuv");
            (m, f)
        } else {
            (AUDIO_ONLY_INTERNAL_MODE, AUDIO_ONLY_INTERNAL_FORMAT)
        };

        let processing_deadline = gst::ClockTime::from_mseconds(
            properties
                .get("processing_deadline_ms")
                .and_then(|v| match v {
                    PropertyValue::UInt(u) => Some(*u),
                    PropertyValue::Int(i) if *i >= 0 => Some(*i as u64),
                    _ => None,
                })
                .unwrap_or(DEFAULT_PROCESSING_DEADLINE_MS)
                .min(MAX_PROCESSING_DEADLINE_MS),
        );

        let sink_id = format!("{}:{}", instance_id, OUTPUT_SINK);
        let sink = card_element(OUTPUT_SINK, &sink_id)?;
        sink.set_property("device-number", device_number);
        for (property, value) in [("mode", video_mode), ("video-format", video_format)] {
            if !enum_has_nick(&sink, property, value) {
                return Err(BlockBuildError::ElementCreation(format!(
                    "{OUTPUT_SINK} has no {property} \"{value}\""
                )));
            }
            sink.set_property_from_str(property, value);
        }
        sink.set_property("processing-deadline", processing_deadline.nseconds());

        let mut elements: Vec<(String, gst::Element)> = vec![(sink_id.clone(), sink)];
        let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();

        if stream_mode.has_audio() {
            let combiner_id = format!("{}:{}", instance_id, OUTPUT_COMBINER);
            let combiner = card_element(OUTPUT_COMBINER, &combiner_id)?;
            internal_links.push((
                ElementPadRef::pad(&combiner_id, "src"),
                ElementPadRef::pad(&sink_id, "sink"),
            ));
            elements.push((combiner_id.clone(), combiner));

            // The combiner waits for sound for every frame: sound that
            // ends or pauses would hold the picture. A mixer with live
            // silence keeps it going (and fills gaps); it starts at the
            // first buffer, as the flow's running time may be hours.
            let channels = audio_channels(properties)?;
            let sound_id = format!("{}:{}", instance_id, OUTPUT_SOUND);
            let sound = gst::ElementFactory::make("audiomixer")
                .name(&sound_id)
                .property_from_str("start-time-selection", "first")
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("audiomixer: {e}")))?;
            for pad in ["sink_0", "sink_1"] {
                sound.request_pad_simple("sink_%u").ok_or_else(|| {
                    BlockBuildError::ElementCreation(format!("audiomixer: no {pad}"))
                })?;
            }
            let silence_id = format!("{}:silence", instance_id);
            let silence = gst::ElementFactory::make("audiotestsrc")
                .name(&silence_id)
                .property("is-live", true)
                .property("do-timestamp", true)
                .property_from_str("wave", "silence")
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("audiotestsrc (silence): {e}"))
                })?;
            // The mixer converts formats but not positioned stereo into
            // unpositioned channels: silence in the sound's channels.
            let mut caps = gst::Caps::builder("audio/x-raw")
                .field("rate", 48000i32)
                .field("channels", channels);
            if channels > 2 {
                caps = caps.field("channel-mask", gst::Bitmask::new(0));
            }
            let silence_caps_id = format!("{}:silence_caps", instance_id);
            let silence_caps = gst::ElementFactory::make("capsfilter")
                .name(&silence_caps_id)
                .property("caps", caps.build())
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("capsfilter (silence): {e}"))
                })?;
            internal_links.push((
                ElementPadRef::pad(&silence_id, "src"),
                ElementPadRef::pad(&silence_caps_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&silence_caps_id, "src"),
                ElementPadRef::pad(&sound_id, "sink_1"),
            ));
            internal_links.push((
                ElementPadRef::pad(&sound_id, "src"),
                ElementPadRef::pad(&combiner_id, "audio"),
            ));
            elements.push((sound_id, sound));
            elements.push((silence_id, silence));
            elements.push((silence_caps_id, silence_caps));

            // Audio-only: black frames carry the sound.
            if !stream_mode.has_video() {
                let testsrc_id = format!("{}:video_filler", instance_id);
                let capsfilter_id = format!("{}:video_filler_caps", instance_id);

                let testsrc = gst::ElementFactory::make("videotestsrc")
                    .name(&testsrc_id)
                    .property_from_str("pattern", "black")
                    .property("is-live", true)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("videotestsrc (filler): {}", e))
                    })?;

                // Caps must match the sink's internal mode.
                let caps = gst::Caps::builder("video/x-raw")
                    .field("format", "UYVY")
                    .field("width", 1920i32)
                    .field("height", 1080i32)
                    .field("framerate", gst::Fraction::new(25, 1))
                    .field("interlace-mode", "progressive")
                    .build();
                let capsfilter = gst::ElementFactory::make("capsfilter")
                    .name(&capsfilter_id)
                    .property("caps", &caps)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("capsfilter (filler): {}", e))
                    })?;

                internal_links.push((
                    ElementPadRef::pad(&testsrc_id, "src"),
                    ElementPadRef::pad(&capsfilter_id, "sink"),
                ));
                internal_links.push((
                    ElementPadRef::pad(&capsfilter_id, "src"),
                    ElementPadRef::pad(&combiner_id, "video"),
                ));
                elements.push((testsrc_id, testsrc));
                elements.push((capsfilter_id, capsfilter));
            }
        }

        info!(
            "DeckLink Output configured: device={}, stream_mode={}, mode={}, video-format={}, lead time={}",
            device_number,
            stream_mode.as_str(),
            video_mode,
            video_format,
            processing_deadline
        );

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Get metadata for DeckLink blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![decklink_input_definition(), decklink_output_definition()]
}

/// Common video mode enum values for DeckLink
fn video_mode_enum_values() -> Vec<EnumValue> {
    vec![
        EnumValue {
            value: "auto".to_string(),
            label: Some("Auto".to_string()),
        },
        // HD modes
        EnumValue {
            value: "1080p2398".to_string(),
            label: Some("1080p 23.98".to_string()),
        },
        EnumValue {
            value: "1080p24".to_string(),
            label: Some("1080p 24".to_string()),
        },
        EnumValue {
            value: "1080p25".to_string(),
            label: Some("1080p 25".to_string()),
        },
        EnumValue {
            value: "1080p2997".to_string(),
            label: Some("1080p 29.97".to_string()),
        },
        EnumValue {
            value: "1080p30".to_string(),
            label: Some("1080p 30".to_string()),
        },
        EnumValue {
            value: "1080p50".to_string(),
            label: Some("1080p 50".to_string()),
        },
        EnumValue {
            value: "1080p5994".to_string(),
            label: Some("1080p 59.94".to_string()),
        },
        EnumValue {
            value: "1080p60".to_string(),
            label: Some("1080p 60".to_string()),
        },
        EnumValue {
            value: "1080i50".to_string(),
            label: Some("1080i 50".to_string()),
        },
        EnumValue {
            value: "1080i5994".to_string(),
            label: Some("1080i 59.94".to_string()),
        },
        EnumValue {
            value: "1080i60".to_string(),
            label: Some("1080i 60".to_string()),
        },
        EnumValue {
            value: "720p50".to_string(),
            label: Some("720p 50".to_string()),
        },
        EnumValue {
            value: "720p5994".to_string(),
            label: Some("720p 59.94".to_string()),
        },
        EnumValue {
            value: "720p60".to_string(),
            label: Some("720p 60".to_string()),
        },
        // UHD modes
        EnumValue {
            value: "2160p2398".to_string(),
            label: Some("4K 23.98".to_string()),
        },
        EnumValue {
            value: "2160p24".to_string(),
            label: Some("4K 24".to_string()),
        },
        EnumValue {
            value: "2160p25".to_string(),
            label: Some("4K 25".to_string()),
        },
        EnumValue {
            value: "2160p2997".to_string(),
            label: Some("4K 29.97".to_string()),
        },
        EnumValue {
            value: "2160p30".to_string(),
            label: Some("4K 30".to_string()),
        },
        EnumValue {
            value: "2160p50".to_string(),
            label: Some("4K 50".to_string()),
        },
        EnumValue {
            value: "2160p5994".to_string(),
            label: Some("4K 59.94".to_string()),
        },
        EnumValue {
            value: "2160p60".to_string(),
            label: Some("4K 60".to_string()),
        },
    ]
}

/// Get DeckLink Input block definition (combined video + audio).
fn decklink_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.decklink_input".to_string(),
        name: "DeckLink Input".to_string(),
        description: "Captures video and/or audio from a Blackmagic DeckLink SDI/HDMI card. Outputs the card's native pixel and audio formats — no internal conversion. Use a downstream videoformat/audioformat block to convert if needed.".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "stream_mode".to_string(),
                label: "Stream Mode".to_string(),
                description: "Which media tracks to expose: video only, audio only, or both. (decklinkvideosrc is always created internally — required by the DeckLink plugin even in audio-only mode.)".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "audio_video".to_string(),
                            label: Some("Audio + Video".to_string()),
                        },
                        EnumValue {
                            value: "video".to_string(),
                            label: Some("Video only".to_string()),
                        },
                        EnumValue {
                            value: "audio".to_string(),
                            label: Some("Audio only".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("audio_video".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "stream_mode".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "device_number".to_string(),
                label: "Device Number".to_string(),
                description: "DeckLink device number (0-based index for multi-card systems)"
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "device_number".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "mode".to_string(),
                label: "Video Mode".to_string(),
                description: "Video mode (resolution and framerate)".to_string(),
                property_type: PropertyType::Enum {
                    values: video_mode_enum_values(),
                },
                default_value: Some(PropertyValue::String("auto".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "mode".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "connection".to_string(),
                label: "Video Connection".to_string(),
                description: "Video input connection type".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "auto".to_string(),
                            label: Some("Auto".to_string()),
                        },
                        EnumValue {
                            value: "sdi".to_string(),
                            label: Some("SDI".to_string()),
                        },
                        EnumValue {
                            value: "hdmi".to_string(),
                            label: Some("HDMI".to_string()),
                        },
                        EnumValue {
                            value: "optical-sdi".to_string(),
                            label: Some("Optical SDI".to_string()),
                        },
                        EnumValue {
                            value: "component".to_string(),
                            label: Some("Component".to_string()),
                        },
                        EnumValue {
                            value: "composite".to_string(),
                            label: Some("Composite".to_string()),
                        },
                        EnumValue {
                            value: "svideo".to_string(),
                            label: Some("S-Video".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("auto".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "connection".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "video_format".to_string(),
                label: "Video Format".to_string(),
                description: "Pixel format the card delivers. Native formats only — no conversion is performed. 'auto' lets the plugin pick based on mode (typically 8bit-yuv); 10bit-yuv (v210) is the broadcast-grade choice.".to_string(),
                property_type: PropertyType::Enum {
                    values: decklink_video_format_enum_values(),
                },
                default_value: Some(PropertyValue::String("auto".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "video_format".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "drop_no_signal_frames".to_string(),
                label: "Drop No-Signal Frames".to_string(),
                description: "Drop frames the card flags as having no input signal instead of forwarding black/test-pattern downstream. Recommended for ingest so the encoder doesn't ship filler over SRT when the source is unplugged.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "drop_no_signal_frames".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "synchronized_capture".to_string(),
                label: "Synchronized Capture".to_string(),
                description: "Sample-align frames with other DeckLink Input blocks in this flow that also have synchronized capture enabled. The hardware arms all members atomically so their frames share a common time origin — useful for cross-port audio cancellation, mixing, or multi-camera capture from a single card. Requires a card supporting BMDDeckLinkSupportsSynchronizeToCaptureGroup (e.g. Quad 2, 8K Pro) and the patched gst-plugins-bad decklink plugin; silently ignored on stock builds or unsupported cards. Capture groups are flow-scoped: blocks in different flows cannot share a group (BMD's hardware-level sync requires all members to be armed in the same start cycle, which only happens within one pipeline). KNOWN BMD DRIVER QUIRK: stopping a synchronized-capture pipeline blocks for ~12 s before the stop completes; this is a Blackmagic driver-internal timeout reproducible in their own SDK examples, not a strom bug. Subsequent restart works normally.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "synchronized_capture".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "audio_connection".to_string(),
                label: "Audio Connection".to_string(),
                description: "Audio input connection type. Only used when audio is exposed.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "auto".to_string(),
                            label: Some("Auto".to_string()),
                        },
                        EnumValue {
                            value: "embedded".to_string(),
                            label: Some("Embedded (SDI/HDMI)".to_string()),
                        },
                        EnumValue {
                            value: "aes".to_string(),
                            label: Some("AES/EBU".to_string()),
                        },
                        EnumValue {
                            value: "analog".to_string(),
                            label: Some("Analog".to_string()),
                        },
                        EnumValue {
                            value: "analog-xlr".to_string(),
                            label: Some("Analog (XLR)".to_string()),
                        },
                        EnumValue {
                            value: "analog-rca".to_string(),
                            label: Some("Analog (RCA)".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("auto".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "audio_connection".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "audio_channels".to_string(),
                label: "Audio Channels".to_string(),
                description: "Number of audio channels to capture (48 kHz, S16LE/S32LE — chosen by downstream caps negotiation).".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "2".to_string(),
                            label: Some("2 (Stereo)".to_string()),
                        },
                        EnumValue {
                            value: "8".to_string(),
                            label: Some("8".to_string()),
                        },
                        EnumValue {
                            value: "16".to_string(),
                            label: Some("16".to_string()),
                        },
                        EnumValue {
                            value: "max".to_string(),
                            label: Some("Max (auto-detect)".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("2".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "audio_channels".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
        ],
        // Static fallback — matches the default `audio_video` mode. The actual
        // pads are computed dynamically in `DeckLinkInputBuilder::get_external_pads`
        // based on the `stream_mode` property.
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![
                ExternalPad {
                    label: Some("V".to_string()),
                    name: "video_out".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "decklinkvideosrc".to_string(),
                    internal_pad_name: "src".to_string(),
                },
                ExternalPad {
                    label: Some("A".to_string()),
                    name: "audio_out".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "decklinkaudiosrc".to_string(),
                    internal_pad_name: "src".to_string(),
                },
            ],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📹".to_string()),
            width: Some(2.0),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}

/// Get DeckLink Output block definition (combined video + audio).
fn decklink_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.decklink_output".to_string(),
        name: "DeckLink Output".to_string(),
        description: "Outputs video and/or audio to a Blackmagic DeckLink SDI/HDMI card (decklink2sink, GStreamer 1.30). Accepts the card's native pixel and audio formats only — no internal conversion. In audio-only mode black frames carry the sound.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "stream_mode".to_string(),
                label: "Stream Mode".to_string(),
                description: "Which media tracks to expose: video only, audio only, or both. (The card takes sound only with frames: in audio-only mode black frames carry it.)".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "audio_video".to_string(),
                            label: Some("Audio + Video".to_string()),
                        },
                        EnumValue {
                            value: "video".to_string(),
                            label: Some("Video only".to_string()),
                        },
                        EnumValue {
                            value: "audio".to_string(),
                            label: Some("Audio only".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("audio_video".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "stream_mode".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "device_number".to_string(),
                label: "Device Number".to_string(),
                description: "DeckLink device number (0-based index for multi-card systems)"
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "device_number".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "mode".to_string(),
                label: "Video Mode".to_string(),
                description: "Output video mode (resolution and framerate). Ignored in audio-only mode.".to_string(),
                property_type: PropertyType::Enum {
                    values: video_mode_enum_values(),
                },
                default_value: Some(PropertyValue::String("1080p25".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "mode".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "video_format".to_string(),
                label: "Video Format".to_string(),
                description: "Pixel format the card expects. Native formats only — no conversion is performed; upstream must deliver this exact format. Ignored in audio-only mode.".to_string(),
                property_type: PropertyType::Enum {
                    values: decklink_output_video_format_enum_values(),
                },
                default_value: Some(PropertyValue::String("8bit-yuv".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "video_format".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "processing_deadline_ms".to_string(),
                label: "Lead Time (ms)".to_string(),
                description: "How long before its display time a frame is handed to the \
                              sink (processing deadline). decklink2sink queues frames on \
                              the card by its own count, so a frame that comes a little \
                              late waits there; GStreamer's default is enough."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(DEFAULT_PROCESSING_DEADLINE_MS)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "processing_deadline_ms".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "audio_channels".to_string(),
                label: "Audio Channels".to_string(),
                description: "Channels of the sound the card embeds (S32LE or S16LE, 48 kHz). \
                              Gaps in the sound are filled with silence in this many channels."
                    .to_string(),
                property_type: PropertyType::Enum {
                    values: ["2", "8", "16"]
                        .iter()
                        .map(|n| EnumValue {
                            value: n.to_string(),
                            label: None,
                        })
                        .collect(),
                },
                default_value: Some(PropertyValue::String("2".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "audio_channels".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
        ],
        // Static fallback — matches the default `audio_video` mode. Actual pads
        // are computed dynamically in `DeckLinkOutputBuilder::get_external_pads`.
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    label: Some("V".to_string()),
                    name: "video_in".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: OUTPUT_COMBINER.to_string(),
                    internal_pad_name: "video".to_string(),
                },
                ExternalPad {
                    label: Some("A".to_string()),
                    name: "audio_in".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: OUTPUT_SOUND.to_string(),
                    internal_pad_name: "sink_0".to_string(),
                },
            ],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📺".to_string()),
            width: Some(2.0),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(properties: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
        properties
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn mode(stream_mode: &str) -> (&'static str, PropertyValue) {
        (
            "stream_mode",
            PropertyValue::String(stream_mode.to_string()),
        )
    }

    /// The output block, or `None` (the test passes) where GStreamer has
    /// no decklink2 plugin: it is new in 1.30.
    fn output(properties: &[(&str, PropertyValue)]) -> Option<BlockBuildResult> {
        gst::init().unwrap();
        if gst::ElementFactory::find("decklink2sink").is_none() {
            eprintln!("skipped: no decklink2 plugin");
            return None;
        }
        Some(
            DeckLinkOutputBuilder
                .build(
                    "out",
                    &props(properties),
                    &BlockBuildContext::new(Vec::new(), "all".to_string()),
                )
                .expect("DeckLink output block"),
        )
    }

    fn element<'a>(result: &'a BlockBuildResult, id: &str) -> &'a gst::Element {
        result
            .elements
            .iter()
            .find(|(element_id, _)| element_id == id)
            .map(|(_, element)| element)
            .unwrap_or_else(|| panic!("no element {id}"))
    }

    fn deadline_of(result: &BlockBuildResult, id: &str) -> gst::ClockTime {
        gst::ClockTime::from_nseconds(element(result, id).property::<u64>("processing-deadline"))
    }

    fn targets(pads: &ExternalPads) -> Vec<(String, String, String)> {
        pads.inputs
            .iter()
            .map(|p| {
                (
                    p.name.clone(),
                    p.internal_element_id.clone(),
                    p.internal_pad_name.clone(),
                )
            })
            .collect()
    }

    fn has_link(result: &BlockBuildResult, from: (&str, &str), to: (&str, &str)) -> bool {
        result.internal_links.iter().any(|(a, b)| {
            a.element_id == from.0
                && a.pad_name.as_deref() == Some(from.1)
                && b.element_id == to.0
                && b.pad_name.as_deref() == Some(to.1)
        })
    }

    // decklink2sink takes sound only as part of the picture: with sound,
    // both go into the combiner (the sound through a mixer that fills
    // gaps with silence); picture alone goes straight to the sink.
    #[test]
    fn sound_meets_the_picture_in_the_combiner() {
        let both = DeckLinkOutputBuilder
            .get_external_pads(&props(&[mode("audio_video")]))
            .unwrap();
        assert_eq!(
            targets(&both),
            vec![
                (
                    "video_in".into(),
                    "decklink2combiner".into(),
                    "video".into()
                ),
                ("audio_in".into(), "sound".into(), "sink_0".into()),
            ]
        );
        let sound = DeckLinkOutputBuilder
            .get_external_pads(&props(&[mode("audio")]))
            .unwrap();
        assert_eq!(
            targets(&sound),
            vec![("audio_in".into(), "sound".into(), "sink_0".into())]
        );
        let picture = DeckLinkOutputBuilder
            .get_external_pads(&props(&[mode("video")]))
            .unwrap();
        assert_eq!(
            targets(&picture),
            vec![("video_in".into(), "decklink2sink".into(), "sink".into())]
        );
    }

    // The block's elements as the pads above say: the combiner feeds the
    // sink, and with sound only black frames drive the picture.
    #[test]
    fn the_combiner_feeds_the_sink() {
        let Some(both) = output(&[mode("audio_video")]) else {
            return;
        };
        assert!(has_link(
            &both,
            ("out:decklink2combiner", "src"),
            ("out:decklink2sink", "sink")
        ));
        let Some(sound) = output(&[mode("audio")]) else {
            return;
        };
        assert!(has_link(
            &sound,
            ("out:video_filler_caps", "src"),
            ("out:decklink2combiner", "video")
        ));
        assert!(has_link(
            &sound,
            ("out:decklink2combiner", "src"),
            ("out:decklink2sink", "sink")
        ));
        let Some(picture) = output(&[mode("video")]) else {
            return;
        };
        assert!(picture.internal_links.is_empty());
        assert_eq!(picture.elements.len(), 1);
    }

    // The sink takes the device, the mode and the lead time. Without the
    // property it keeps 20 ms, GStreamer's own default: that half
    // documents it and guards nothing.
    #[test]
    fn the_sink_takes_device_mode_and_lead_time() {
        let Some(ahead) = output(&[
            ("device_number", PropertyValue::UInt(3)),
            ("mode", PropertyValue::String("720p50".into())),
            ("processing_deadline_ms", PropertyValue::UInt(100)),
        ]) else {
            return;
        };
        let sink = element(&ahead, "out:decklink2sink");
        assert_eq!(sink.property::<i32>("device-number"), 3);
        assert_eq!(enum_nick(sink, "mode"), "720p50");
        assert_eq!(
            deadline_of(&ahead, "out:decklink2sink"),
            gst::ClockTime::from_mseconds(100)
        );

        let Some(plain) = output(&[]) else {
            return;
        };
        assert_eq!(
            deadline_of(&plain, "out:decklink2sink"),
            gst::ClockTime::from_mseconds(20)
        );
    }

    fn enum_nick(element: &gst::Element, name: &str) -> String {
        let value = element.property_value(name);
        let (_, enum_value) = gst::glib::EnumValue::from_value(&value).unwrap();
        enum_value.nick().to_string()
    }

    // A lead time of hours (or u64::MAX ms, which would overflow) is cut
    // to a second.
    #[test]
    fn lead_time_has_a_ceiling() {
        let Some(huge) = output(&[("processing_deadline_ms", PropertyValue::UInt(u64::MAX))])
        else {
            return;
        };
        assert_eq!(
            deadline_of(&huge, "out:decklink2sink"),
            gst::ClockTime::SECOND
        );
    }

    // Without the decklink2 plugin the block fails and says what is
    // missing and where it comes from.
    #[test]
    fn a_missing_plugin_says_what_to_install() {
        gst::init().unwrap();
        let Err(BlockBuildError::ElementCreation(message)) =
            card_element("decklink2nosuchsink", "out:x")
        else {
            panic!("an element from nowhere");
        };
        assert!(message.contains("decklink2nosuchsink"), "{message}");
        assert!(message.contains("GStreamer 1.30"), "{message}");
        assert!(message.contains("GST_PLUGIN_PATH"), "{message}");
    }

    // Every mode and picture format the block offers exists on
    // decklink2sink; without the plugin the format list stays as it is.
    #[test]
    fn offered_modes_and_formats_exist_on_the_sink() {
        let definition = decklink_output_definition();
        let offered = |name: &str| -> Vec<String> {
            let property = definition
                .exposed_properties
                .iter()
                .find(|p| p.name == name)
                .unwrap();
            let PropertyType::Enum { values } = &property.property_type else {
                panic!("{name} is no enum");
            };
            values.iter().map(|v| v.value.clone()).collect()
        };
        assert_eq!(
            offered("video_format"),
            [
                "auto",
                "8bit-yuv",
                "10bit-yuv",
                "8bit-argb",
                "8bit-bgra",
                "10bit-rgb"
            ]
        );
        gst::init().unwrap();
        let Some(factory) = gst::ElementFactory::find(OUTPUT_SINK) else {
            return;
        };
        let sink = factory.create().build().unwrap();
        for (name, property) in [("mode", "mode"), ("video_format", "video-format")] {
            for value in offered(name) {
                assert!(
                    enum_has_nick(&sink, property, &value),
                    "{OUTPUT_SINK} has no {property} {value}"
                );
            }
        }
    }

    // A format or mode the sink does not know fails the build; it used to
    // panic in property_from_str.
    #[test]
    fn an_unknown_format_fails_the_build() {
        gst::init().unwrap();
        if gst::ElementFactory::find(OUTPUT_SINK).is_none() {
            return;
        }
        for (key, value) in [("video_format", "12bit-rgb"), ("mode", "1080p26")] {
            let result = DeckLinkOutputBuilder.build(
                "out",
                &props(&[(key, PropertyValue::String(value.into()))]),
                &BlockBuildContext::new(Vec::new(), "all".to_string()),
            );
            let Err(BlockBuildError::ElementCreation(message)) = result else {
                panic!("{key} {value} built");
            };
            assert!(message.contains(value), "{message}");
        }
    }

    // Without the decklink2 plugin the block fails and says what to
    // install (runs where GStreamer has no decklink2).
    #[test]
    fn without_the_plugin_the_build_says_what_to_install() {
        gst::init().unwrap();
        if gst::ElementFactory::find(OUTPUT_SINK).is_some() {
            return;
        }
        let result = DeckLinkOutputBuilder.build(
            "out",
            &props(&[]),
            &BlockBuildContext::new(Vec::new(), "all".to_string()),
        );
        let Err(BlockBuildError::ElementCreation(message)) = result else {
            panic!("built without decklink2");
        };
        assert!(message.contains("GStreamer 1.30"), "{message}");
    }

    // Sound that ends (or pauses) must not stop the picture: the
    // combiner waits for sound for every frame. The block up to the
    // card, with a fakesink in its place: 8 channels of sound for half a
    // second, picture for three seconds at 25 fps.
    #[test]
    fn the_picture_runs_on_when_the_sound_stops() {
        let Some(block) = output(&[
            mode("audio_video"),
            ("audio_channels", PropertyValue::String("8".into())),
        ]) else {
            return;
        };
        let pads = DeckLinkOutputBuilder
            .get_external_pads(&props(&[mode("audio_video")]))
            .unwrap();
        let pipeline = gst::Pipeline::new();
        let sink_id = format!("out:{OUTPUT_SINK}");
        for (id, element) in &block.elements {
            if *id != sink_id {
                pipeline.add(element).unwrap();
            }
        }
        let by_id = |id: &str| {
            block
                .elements
                .iter()
                .find(|(e, _)| e == id)
                .map(|(_, e)| e.clone())
                .unwrap_or_else(|| panic!("no element {id}"))
        };
        let pad = |element_id: &str, name: &str| {
            let element = by_id(element_id);
            element
                .static_pad(name)
                .or_else(|| element.request_pad_simple(name))
                .unwrap_or_else(|| panic!("no pad {name} on {element_id}"))
        };
        let frames = gst::ElementFactory::make("fakesink")
            .property("sync", true)
            .build()
            .unwrap();
        pipeline.add(&frames).unwrap();
        for (from, to) in &block.internal_links {
            let src = pad(&from.element_id, from.pad_name.as_deref().unwrap());
            let sink = if to.element_id == sink_id {
                frames.static_pad("sink").unwrap()
            } else {
                pad(&to.element_id, to.pad_name.as_deref().unwrap())
            };
            src.link(&sink).unwrap();
        }
        let picture = gst::parse::bin_from_description(
            "videotestsrc is-live=true num-buffers=75 \
             ! capsfilter caps=video/x-raw,format=UYVY,width=1920,height=1080,framerate=25/1",
            true,
        )
        .unwrap();
        let sound = gst::parse::bin_from_description(
            "audiotestsrc is-live=true num-buffers=50 samplesperbuffer=480 \
             ! capsfilter caps=audio/x-raw,format=S32LE,rate=48000,channels=8,\
             layout=interleaved,channel-mask=(bitmask)0",
            true,
        )
        .unwrap();
        pipeline.add(&picture).unwrap();
        pipeline.add(&sound).unwrap();
        for (bin, name) in [(&picture, "video_in"), (&sound, "audio_in")] {
            let target = pads.inputs.iter().find(|p| p.name == name).unwrap();
            let into = pad(
                &format!("out:{}", target.internal_element_id),
                &target.internal_pad_name,
            );
            bin.static_pad("src").unwrap().link(&into).unwrap();
        }
        let count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counted = count.clone();
        frames
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        pipeline.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3500));
        pipeline.set_state(gst::State::Null).unwrap();
        let shown = count.load(std::sync::atomic::Ordering::Relaxed);
        assert!(shown >= 60, "{shown} of 75 frames after the sound stopped");
    }
}
