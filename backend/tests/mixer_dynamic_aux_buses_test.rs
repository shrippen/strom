//! Dynamic aux buses of the audio mixer: buses added to and removed from a
//! running mixer, their output on an inter channel.
//!
//! What must hold (the reason the feature exists): Main keeps playing
//! through every add, remove and tap change, with no gap in its
//! timestamps; a new bus takes its send levels and tap through the block
//! properties like a static one; the buses survive a restart; a removed
//! bus leaves no element and no stored property behind.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom::blocks::builtin::mixer::dynamic::AuxBusError;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::element::Link;
use strom_types::mixer::{AddMixerAuxBusRequest, MixerAuxBuses};
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

/// A tone into channel 1 of a mixer with one static aux bus and room for
/// three more; Main into a clock-synced sink.
fn mixer_flow() -> Flow {
    let mut flow = Flow::new("mixer_dynamic_aux_buses");
    flow.elements.push(elem(
        "tone",
        "audiotestsrc",
        vec![("is-live", PV::Bool(true)), ("volume", PV::Float(0.5))],
    ));
    flow.elements
        .push(elem("mainsink", "fakesink", vec![("sync", PV::Bool(true))]));
    flow.links.push(Link {
        from: "tone:src".into(),
        to: format!("{BLOCK}:input_1"),
    });
    flow.links.push(Link {
        from: format!("{BLOCK}:main_out"),
        to: "mainsink:sink".into(),
    });
    let props: HashMap<String, PV> = [
        ("num_channels", PV::UInt(2)),
        ("num_aux_buses", PV::UInt(1)),
        ("max_aux_buses", PV::UInt(4)),
        ("dsp_backend", PV::String("rust".into())),
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
    flow
}

/// Main's buffers as they reach the sink: (pts, duration) in ns.
type Main = Arc<Mutex<Vec<(u64, u64)>>>;

struct Rig {
    /// Inter channel of the bus: channels are process-wide and the tests
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
        // `main` registers the inter elements at startup, once: registering
        // them again replaces their factories under the elements already made.
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
                    if let (Some(pts), Some(dur)) = (b.pts(), b.duration()) {
                        main.lock().unwrap().push((pts.nseconds(), dur.nseconds()));
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

    async fn add(&self, pre_fader: Option<bool>) -> Result<MixerAuxBuses, AuxBusError> {
        self.state
            .add_mixer_aux_bus(
                &self.flow_id,
                BLOCK,
                AddMixerAuxBusRequest {
                    aux: None,
                    channel: self.channel.clone(),
                    pre_fader,
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

/// Listens to the bus's inter channel and tells how loud it is.
struct Ears {
    pipeline: gst::Pipeline,
    peak: Arc<Mutex<f64>>,
}

impl Ears {
    fn new(channel: &str) -> Ears {
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("intersrc")
            .property("producer-name", channel)
            .build()
            .unwrap();
        let level = gst::ElementFactory::make("level")
            .property("interval", 50_000_000u64)
            .property("post-messages", true)
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .build()
            .unwrap();
        pipeline.add_many([&src, &level, &sink]).unwrap();
        gst::Element::link_many([&src, &level, &sink]).unwrap();
        let peak = Arc::new(Mutex::new(f64::NEG_INFINITY));
        let p = Arc::clone(&peak);
        pipeline.bus().unwrap().set_sync_handler(move |_, msg| {
            if let Some(s) = msg.structure().filter(|s| s.name() == "level") {
                if let Ok(list) = s.get::<gst::glib::ValueArray>("peak") {
                    let loudest = list
                        .iter()
                        .filter_map(|v| v.get::<f64>().ok())
                        .fold(f64::NEG_INFINITY, f64::max);
                    *p.lock().unwrap() = loudest;
                }
            }
            gst::BusSyncReply::Drop
        });
        pipeline.set_state(gst::State::Playing).unwrap();
        Ears { pipeline, peak }
    }

    /// Wait until the channel is loud (`loud`) or silent; fail after 3 s.
    async fn until(&self, loud: bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let peak = *self.peak.lock().unwrap();
            if (loud && peak > -40.0) || (!loud && peak < -80.0) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: the bus stayed at {peak:.1} dB"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Ears {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bus_comes_and_goes_while_main_plays() {
    let rig = Rig::start("mixer_aux_test_comes_and_goes").await;
    let baseline = rig.element_count().await;
    let ears = Ears::new(&rig.channel);

    let buses = rig.add(Some(false)).await.expect("add a bus");
    assert_eq!(buses.dynamic.len(), 1);
    assert_eq!(buses.dynamic[0].aux, 2, "the lowest free number");
    assert!(matches!(
        rig.stored("ch1_aux2_pre").await,
        Some(PV::Bool(false))
    ));
    // A new send starts at level 0; the level reaches the new element.
    ears.until(false, "a new send").await;
    rig.set("ch1_aux2_level", PV::Float(1.0)).await;
    ears.until(true, "send level 1").await;
    // After the fader: muting the channel silences the bus; moving the
    // send before the fader brings it back.
    rig.set("ch1_mute", PV::Bool(true)).await;
    ears.until(false, "muted, post fader").await;
    rig.set("ch1_aux2_pre", PV::Bool(true)).await;
    ears.until(true, "muted, pre fader").await;
    rig.set("ch1_mute", PV::Bool(false)).await;

    // A static bus stays; a bus that is not there is not found.
    assert!(matches!(
        rig.state.remove_mixer_aux_bus(&rig.flow_id, BLOCK, 1).await,
        Err(AuxBusError::Conflict(_))
    ));
    assert!(matches!(
        rig.state.remove_mixer_aux_bus(&rig.flow_id, BLOCK, 3).await,
        Err(AuxBusError::NotFound(_))
    ));
    // The channel is the bus's: a second bus cannot take it.
    assert!(matches!(rig.add(None).await, Err(AuxBusError::Conflict(_))));

    let buses = rig
        .state
        .remove_mixer_aux_bus(&rig.flow_id, BLOCK, 2)
        .await
        .expect("remove the bus");
    assert!(buses.dynamic.is_empty());
    assert_eq!(rig.element_count().await, baseline, "elements left behind");
    assert!(rig.element(&format!("{BLOCK}:aux1_mixer")).await.is_none());
    for name in ["ch1_aux2_level", "ch1_aux2_pre", "dynamic_aux_buses"] {
        assert!(rig.stored(name).await.is_none(), "{name} stayed stored");
    }
    rig.assert_main_unbroken("add, sends, tap, remove");
    drop(ears);
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bus_comes_back_after_a_restart() {
    let rig = Rig::start("mixer_aux_test_restart").await;
    rig.add(Some(true)).await.expect("add a bus");
    rig.set("ch1_aux2_level", PV::Float(1.0)).await;
    rig.state.stop_flow(&rig.flow_id).await.unwrap();
    rig.start_flow().await;
    assert!(rig
        .element(&format!("{BLOCK}:aux1_intersink"))
        .await
        .is_some());
    let buses = rig
        .state
        .mixer_aux_buses(&rig.flow_id, BLOCK)
        .await
        .unwrap();
    assert_eq!(buses.dynamic.len(), 1);
    let ears = Ears::new(&rig.channel);
    ears.until(true, "after the restart").await;
    drop(ears);
    rig.stop().await;
}
