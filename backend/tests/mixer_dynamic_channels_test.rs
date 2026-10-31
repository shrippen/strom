//! Dynamic channels of the audio mixer: channels added to and removed from
//! a running mixer, fed from an inter channel.
//!
//! What must hold (the reason the feature exists): Main keeps playing
//! through every add and remove, with no gap in its timestamps; a new
//! channel is heard on Main and takes its settings through the block
//! properties like a static one; the channels survive a restart; a removed
//! channel leaves no element and no stored property behind.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom::blocks::builtin::mixer::dynamic_channels::ChannelError;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::element::Link;
use strom_types::flow::GStreamerClockType;
use strom_types::mixer::{AddMixerChannelRequest, MixerChannels};
use strom_types::{Flow, FlowId, PropertyValue as PV};
use tempfile::NamedTempFile;

const BLOCK: &str = "mix";

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

/// Flows that hand media over `inter` share their timing.
fn timed(mut flow: Flow) -> Flow {
    flow.properties.clock_type = GStreamerClockType::Monotonic;
    flow.properties.direct_media_timing = Some(true);
    flow
}

/// A mixer with one static channel, unfed, and room for three more; Main
/// into a clock-synced sink.
fn mixer_flow() -> Flow {
    let mut flow = Flow::new("mixer_dynamic_channels");
    flow.elements
        .push(elem("mainsink", "fakesink", vec![("sync", PV::Bool(true))]));
    flow.links.push(Link {
        from: format!("{BLOCK}:main_out"),
        to: "mainsink:sink".into(),
    });
    let props: HashMap<String, PV> = [
        ("num_channels", PV::UInt(1)),
        ("max_channels", PV::UInt(4)),
        ("dsp_backend", PV::String("rust".into())),
        // Base time 0 (`timed`): Main starts at the first buffer.
        ("start_time_selection", PV::String("first".into())),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK.to_string(),
        block_definition_id: "builtin.mixer".to_string(),
        name: None,
        properties: props,
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    timed(flow)
}

/// A tone published on inter channel `channel`.
fn tone_flow(channel: &str) -> Flow {
    let mut flow = Flow::new("mixer_dynamic_channels_tone");
    flow.elements.push(elem(
        "tone",
        "audiotestsrc",
        vec![("is-live", PV::Bool(true)), ("volume", PV::Float(0.5))],
    ));
    flow.elements.push(elem(
        "out",
        "intersink",
        vec![("producer-name", PV::String(channel.into()))],
    ));
    flow.links.push(Link {
        from: "tone:src".into(),
        to: "out:sink".into(),
    });
    timed(flow)
}

/// Main's buffers as they reach the sink: (pts, duration, peak sample).
type Main = Arc<Mutex<Vec<(u64, u64, f32)>>>;

struct Rig {
    /// Inter channel of the tone: channels are process-wide and the tests
    /// run side by side.
    channel: String,
    state: AppState,
    flow_id: FlowId,
    main: Main,
    main_loop: gst::glib::MainLoop,
    _files: (NamedTempFile, NamedTempFile, tempfile::TempDir),
}

impl Rig {
    async fn start(channel: &str) -> Rig {
        gst::init().unwrap();
        // `main` registers the inter elements at startup, once.
        static INTER: std::sync::Once = std::sync::Once::new();
        INTER.call_once(|| gstrsinter::plugin_register_static().expect("inter plugin"));
        let main_loop = gst::glib::MainLoop::new(None, false);
        {
            let ml = main_loop.clone();
            std::thread::spawn(move || ml.run());
        }
        let storage = NamedTempFile::new().unwrap();
        let blocks = NamedTempFile::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            JsonFileStorage::new(storage.path()),
            blocks.path(),
            dir.path(),
            vec![],
            "all".to_string(),
            vec![],
            false,
            false,
        );
        let tone = tone_flow(channel);
        let tone_id = tone.id;
        state.upsert_flow(tone).await.unwrap();
        state.start_flow(&tone_id).await.unwrap();
        let flow = mixer_flow();
        let flow_id = flow.id;
        state.upsert_flow(flow).await.unwrap();
        let rig = Rig {
            channel: channel.to_string(),
            state,
            flow_id,
            main: Arc::default(),
            main_loop,
            _files: (storage, blocks, dir),
        };
        rig.start_flow().await;
        rig
    }

    async fn start_flow(&self) {
        self.state.start_flow(&self.flow_id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        let main = Arc::clone(&self.main);
        main.lock().unwrap().clear();
        let sink = self.element("mainsink").await.expect("mainsink");
        sink.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(gst::PadProbeData::Buffer(b)) = &info.data {
                    if let (Some(pts), Some(dur), Ok(map)) =
                        (b.pts(), b.duration(), b.map_readable())
                    {
                        // Main is F32LE (the bus format).
                        let peak = map
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|s| f32::from_le_bytes(*s).abs())
                            .fold(0.0, f32::max);
                        main.lock()
                            .unwrap()
                            .push((pts.nseconds(), dur.nseconds(), peak));
                    }
                }
                gst::PadProbeReturn::Ok
            });
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    async fn element(&self, name: &str) -> Option<gst::Element> {
        let pipelines = self.state.pipelines_read().await;
        pipelines.get(&self.flow_id)?.pipeline().by_name(name)
    }

    async fn element_count(&self) -> usize {
        let pipelines = self.state.pipelines_read().await;
        let pipeline = pipelines.get(&self.flow_id).unwrap().pipeline().clone();
        drop(pipelines);
        pipeline.iterate_recurse().into_iter().count()
    }

    async fn add(&self, channel: Option<usize>) -> Result<MixerChannels, ChannelError> {
        self.state
            .add_mixer_channel(
                &self.flow_id,
                BLOCK,
                AddMixerChannelRequest {
                    channel,
                    source: self.channel.clone(),
                    max_time_ms: None,
                },
            )
            .await
    }

    async fn set(&self, name: &str, value: PV) {
        let (_, rejected) = self
            .state
            .update_block_properties(
                &self.flow_id,
                BLOCK,
                HashMap::from([(name.to_string(), value)]),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(rejected.is_empty(), "{name} rejected: {rejected:?}");
    }

    async fn stored(&self, name: &str) -> Option<PV> {
        let flow = self.state.get_flow(&self.flow_id).await.unwrap();
        let block = flow.blocks.iter().find(|b| b.id == BLOCK).unwrap();
        block.properties.get(name).cloned()
    }

    /// Wait until Main is loud (`loud`) or silent; fail after 3 s.
    async fn until(&self, loud: bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            // The last 100 ms of Main.
            let peak = {
                let main = self.main.lock().unwrap();
                main.iter().rev().take(10).map(|b| b.2).fold(0.0, f32::max)
            };
            if (loud && peak > 0.05) || (!loud && peak < 1e-4) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: Main stayed at peak {peak}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Main played on without a gap: each buffer starts where the one
    /// before ended.
    fn assert_main_unbroken(&self, what: &str) {
        let main = self.main.lock().unwrap();
        assert!(
            main.len() > 20,
            "{what}: Main ran too short ({})",
            main.len()
        );
        let worst = main
            .windows(2)
            .map(|w| w[1].0.abs_diff(w[0].0 + w[0].1))
            .max()
            .unwrap_or(0);
        assert!(
            worst <= 1_000_000,
            "{what}: Main's timestamps jumped by {} µs",
            worst / 1000
        );
    }

    async fn stop(self) {
        self.state.stop_flow(&self.flow_id).await.unwrap();
        self.main_loop.quit();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_channel_comes_and_goes_while_main_plays() {
    let rig = Rig::start("mixer_channel_test_comes_and_goes").await;
    let baseline = rig.element_count().await;
    rig.until(false, "no channel fed").await;

    let channels = rig.add(None).await.expect("add a channel");
    assert_eq!(channels.dynamic.len(), 1);
    assert_eq!(channels.dynamic[0].channel, 2, "the lowest free number");
    rig.until(true, "the new channel").await;
    // Its settings reach it like a static channel's.
    rig.set("ch2_mute", PV::Bool(true)).await;
    rig.until(false, "muted").await;
    rig.set("ch2_mute", PV::Bool(false)).await;
    rig.until(true, "unmuted").await;

    // A static channel stays; a channel that is not there is not found;
    // a taken number is refused.
    assert!(matches!(
        rig.state.remove_mixer_channel(&rig.flow_id, BLOCK, 1).await,
        Err(ChannelError::Conflict(_))
    ));
    assert!(matches!(
        rig.state.remove_mixer_channel(&rig.flow_id, BLOCK, 3).await,
        Err(ChannelError::NotFound(_))
    ));
    assert!(matches!(
        rig.add(Some(2)).await,
        Err(ChannelError::Conflict(_))
    ));

    let channels = rig
        .state
        .remove_mixer_channel(&rig.flow_id, BLOCK, 2)
        .await
        .expect("remove the channel");
    assert!(channels.dynamic.is_empty());
    rig.until(false, "removed").await;
    assert_eq!(rig.element_count().await, baseline, "elements left behind");
    for name in ["ch2_mute", "dynamic_channels"] {
        assert!(rig.stored(name).await.is_none(), "{name} stayed stored");
    }
    rig.assert_main_unbroken("add, mute, remove");
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_channel_comes_back_after_a_restart() {
    let rig = Rig::start("mixer_channel_test_restart").await;
    rig.add(Some(3)).await.expect("add channel 3");
    rig.set("ch3_fader", PV::Float(0.5)).await;
    rig.state.stop_flow(&rig.flow_id).await.unwrap();
    rig.start_flow().await;
    let channels = rig.state.mixer_channels(&rig.flow_id, BLOCK).await.unwrap();
    assert_eq!(channels.dynamic.len(), 1);
    assert_eq!(channels.dynamic[0].channel, 3);
    assert!(rig.element(&format!("{BLOCK}:volume_2")).await.is_some());
    rig.until(true, "after the restart").await;
    assert!(matches!(rig.stored("ch3_fader").await, Some(PV::Float(_))));
    rig.stop().await;
}
