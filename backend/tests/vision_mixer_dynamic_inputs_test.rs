//! Dynamic vision mixer inputs on the CPU backend: inputs added to and
//! removed from a running mixer, fed from inter channels.
//!
//! What must hold (the reason the feature exists): the program keeps
//! running through every add and remove, with no gap in its timestamps and
//! no renegotiation; a removal that would take something off air is
//! refused; adding and removing leaves no element or pad behind; the
//! inputs survive a restart.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use strom::blocks::builtin::vision_mixer::dynamic::DynamicInputError;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::element::Link;
use strom_types::vision_mixer::{AddVisionMixerInputRequest, DynamicInputSource, PadMap, Zone};
use strom_types::{Flow, FlowId, PropertyValue as PV, StromEvent};
use tempfile::NamedTempFile;

const BLOCK: &str = "dynmix";
const W: u32 = 320;
const H: u32 = 180;
/// Program frame duration at 30 fps.
const FRAME_NS: u64 = 1_000_000_000 / 30;
const NUM_INPUTS: usize = 2;
const MAX_INPUTS: usize = 6;
const NUM_PIPS: usize = 1;
const NUM_DSK: usize = 1;
/// Cell of the tile output: 6 slots make a grid of 3 × 2.
const CELL: (i32, i32) = (160, 90);
/// Longest gap between two program frames at the sink (a frame is 33 ms).
const MAX_ARRIVAL_GAP: Duration = Duration::from_millis(400);

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

/// A mixer with two static inputs (live test sources), four free slots,
/// one PiP (border underlays), one DSK, the preview and the tile output.
fn mixer_flow(backend: &str) -> Flow {
    let mut flow = Flow::new("vm_dynamic_inputs");
    for i in 0..NUM_INPUTS {
        flow.elements.push(elem(
            &format!("src{i}"),
            "videotestsrc",
            vec![
                ("pattern", PV::String(["smpte", "ball"][i].into())),
                ("is-live", PV::Bool(true)),
            ],
        ));
        flow.elements.push(elem(
            &format!("caps{i}"),
            "capsfilter",
            vec![(
                "caps",
                PV::String(format!(
                    "video/x-raw,format=I420,width={W},height={H},framerate=30/1"
                )),
            )],
        ));
        flow.links.push(Link {
            from: format!("src{i}:src"),
            to: format!("caps{i}:sink"),
        });
        flow.links.push(Link {
            from: format!("caps{i}:src"),
            to: format!("{BLOCK}:video_in_{i}"),
        });
    }
    let props: HashMap<String, PV> = [
        ("compositor_preference", PV::String(backend.into())),
        ("num_inputs", PV::UInt(NUM_INPUTS as u64)),
        ("max_inputs", PV::UInt(MAX_INPUTS as u64)),
        ("pgm_resolution", PV::String(format!("{W}x{H}"))),
        ("multiview_resolution", PV::String(format!("{W}x{H}"))),
        ("pgm_framerate", PV::String("30/1".into())),
        ("num_pips", PV::String(NUM_PIPS.to_string())),
        ("num_dsk_inputs", PV::String(NUM_DSK.to_string())),
        ("enable_preview_out", PV::Bool(true)),
        ("enable_tiles_out", PV::Bool(true)),
        ("tiles_cell", PV::String(format!("{}x{}", CELL.0, CELL.1))),
        // Not what the compositor picks on its own.
        ("tiles_format", PV::String("BGRA".into())),
        ("initial_pgm_input", PV::UInt(0)),
        ("initial_pvw_input", PV::UInt(1)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: props,
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    for (id, pad) in [
        ("pgmsink", "pgm_out"),
        ("mvsink", "multiview_out"),
        ("pvwsink", "pvw_out"),
        ("tilessink", "tiles_out"),
    ] {
        flow.elements
            .push(elem(id, "fakesink", vec![("sync", PV::Bool(true))]));
        flow.links.push(Link {
            from: format!("{BLOCK}:{pad}"),
            to: format!("{id}:sink"),
        });
    }
    flow
}

/// A live test source published on inter channel `channel`, in its own
/// pipeline (what an Inter Output block in another flow does).
fn producer(channel: &str, pattern: &str) -> gst::Element {
    let pipeline = gst::parse::launch(&format!(
        "videotestsrc is-live=true pattern={pattern} \
         ! video/x-raw,format=I420,width={W},height={H},framerate=30/1 \
         ! intersink producer-name={channel}"
    ))
    .expect("producer pipeline");
    pipeline.set_state(gst::State::Playing).unwrap();
    pipeline
}

fn inter(channel: &str) -> AddVisionMixerInputRequest {
    AddVisionMixerInputRequest {
        index: None,
        source: DynamicInputSource::Inter {
            channel: channel.to_string(),
            max_time_ms: None,
        },
        label: None,
    }
}

/// Program output timestamps and caps changes, recorded at the program sink
/// once it has negotiated.
#[derive(Default)]
struct Program {
    pts: Vec<u64>,
    /// When each buffer arrived: a stalled mixer still stamps its frames
    /// evenly, but they arrive late.
    arrivals: Vec<std::time::Instant>,
    caps_events: usize,
}

struct Rig {
    /// Mixer backend; also part of the channel names, as inter channels are
    /// process-wide and the tests run side by side.
    backend: String,
    state: AppState,
    flow_id: FlowId,
    program: Arc<Mutex<Program>>,
    main_loop: gst::glib::MainLoop,
    _files: (NamedTempFile, NamedTempFile, tempfile::TempDir),
}

impl Rig {
    async fn start() -> Rig {
        Self::start_on("cpu").await
    }

    /// The rig with the mixer on `backend` (`cpu` or `cuda`).
    async fn start_on(backend: &str) -> Rig {
        Self::start_with(backend, |_| {}).await
    }

    /// The rig with the mixer flow changed by `edit` first.
    async fn start_with(backend: &str, edit: impl FnOnce(&mut Flow)) -> Rig {
        gst::init().unwrap();
        // `main` registers the inter elements at startup, once: registering
        // them again replaces their factories under the elements already made.
        static INTER: std::sync::Once = std::sync::Once::new();
        INTER.call_once(|| gstrsinter::plugin_register_static().expect("inter plugin"));
        strom::gpu::detect_gpu_capabilities();
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
        let mut flow = mixer_flow(backend);
        edit(&mut flow);
        let flow_id = flow.id;
        state.upsert_flow(flow).await.unwrap();
        let rig = Rig {
            backend: backend.to_string(),
            state,
            flow_id,
            program: Arc::default(),
            main_loop,
            _files: (storage, blocks, dir),
        };
        rig.start_flow().await;
        rig
    }

    async fn start_flow(&self) {
        self.state.start_flow(&self.flow_id).await.unwrap();
        // Negotiated by now: from here on the program must not renegotiate.
        tokio::time::sleep(Duration::from_millis(1000)).await;
        // Record the program: a test-side probe, not part of the mixer.
        let program = Arc::clone(&self.program);
        if let Ok(mut p) = program.lock() {
            *p = Program::default();
        }
        let sink = self.element("pgmsink").await.expect("pgmsink");
        sink.static_pad("sink").unwrap().add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_, info| {
                if let Ok(mut p) = program.lock() {
                    match &info.data {
                        Some(gst::PadProbeData::Buffer(b)) => {
                            if let Some(pts) = b.pts() {
                                p.pts.push(pts.nseconds());
                                p.arrivals.push(std::time::Instant::now());
                            }
                        }
                        Some(gst::PadProbeData::Event(e)) if e.type_() == gst::EventType::Caps => {
                            p.caps_events += 1;
                        }
                        _ => {}
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    async fn element(&self, name: &str) -> Option<gst::Element> {
        let pipelines = self.state.pipelines_read().await;
        pipelines.get(&self.flow_id)?.pipeline().by_name(name)
    }

    /// This rig's name for inter channel `name`.
    fn channel(&self, name: &str) -> String {
        format!("{name}-{}", self.backend)
    }

    fn id(name: &str) -> String {
        format!("{BLOCK}:{name}")
    }

    /// Elements in the flow's pipeline, recursively.
    async fn element_count(&self) -> usize {
        let pipelines = self.state.pipelines_read().await;
        let pipeline = pipelines.get(&self.flow_id).unwrap().pipeline().clone();
        drop(pipelines);
        pipeline.iterate_recurse().into_iter().count()
    }

    /// Sink pads of the mixer's compositors.
    async fn compositor_pads(&self) -> usize {
        let mut n = 0;
        for c in ["mixer", "mv_comp", "pvw_comp", "tiles_comp"] {
            n += self.element(&Self::id(c)).await.unwrap().sink_pads().len();
        }
        n
    }

    async fn add(&self, req: AddVisionMixerInputRequest) -> Result<usize, DynamicInputError> {
        self.state
            .add_vision_mixer_input(&self.flow_id, BLOCK, req)
            .await
            .map(|(input, _)| input.index)
    }

    async fn remove(&self, index: usize) -> Result<(), DynamicInputError> {
        self.state
            .remove_vision_mixer_input(&self.flow_id, BLOCK, index)
            .await
            .map(|_| ())
    }

    async fn active(&self) -> Vec<usize> {
        self.state
            .vision_mixer_inputs(&self.flow_id, BLOCK)
            .await
            .unwrap()
            .inputs
            .iter()
            .map(|i| i.index)
            .collect()
    }

    fn pad_map() -> PadMap {
        PadMap::new(MAX_INPUTS, NUM_DSK, NUM_PIPS)
    }

    /// Wait until input `i` has delivered video to the program mixer.
    async fn wait_for_video(&self, i: usize) {
        let mixer = self.element(&Self::id("mixer")).await.unwrap();
        let pad = PadMap::sink(Self::pad_map().dist_input(i));
        for _ in 0..100 {
            if mixer
                .static_pad(&pad)
                .and_then(|p| p.current_caps())
                .is_some()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("input {i} never delivered video to the mixer");
    }

    /// The program kept running: one frame after the other, no gap, and
    /// its caps never changed.
    fn assert_program_unbroken(&self, what: &str) {
        let p = self.program.lock().unwrap();
        assert!(p.pts.len() > 30, "{what}: program ran too short");
        let worst = p
            .pts
            .windows(2)
            .map(|w| w[1].saturating_sub(w[0]))
            .max()
            .unwrap_or(0);
        assert!(
            worst <= FRAME_NS + FRAME_NS / 2,
            "{what}: the program skipped frames (largest step {} ms)",
            worst / 1_000_000
        );
        assert!(
            p.pts.windows(2).all(|w| w[1] > w[0]),
            "{what}: program timestamps went backwards"
        );
        assert_eq!(p.caps_events, 0, "{what}: the program renegotiated");
        // The sink plays in sync, so frames arrive at the frame rate. Allow
        // for a loaded machine, not for a stalled mixer.
        let stall = p
            .arrivals
            .windows(2)
            .map(|w| w[1] - w[0])
            .max()
            .unwrap_or_default();
        assert!(
            stall <= MAX_ARRIVAL_GAP,
            "{what}: the program stalled for {} ms",
            stall.as_millis()
        );
    }

    async fn stop(self) {
        self.state.stop_flow(&self.flow_id).await.unwrap();
        self.main_loop.quit();
    }
}

fn conflict(r: Result<(), DynamicInputError>) -> String {
    match r {
        Err(DynamicInputError::Conflict(m)) => m,
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_input_comes_and_goes_while_the_program_runs() {
    comes_and_goes(Rig::start().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NVIDIA CUDA (cudaupload, cudacompositor)"]
async fn cuda_an_input_comes_and_goes_while_the_program_runs() {
    comes_and_goes(Rig::start_on("cuda").await).await;
}

async fn comes_and_goes(rig: Rig) {
    let cam = rig.channel("dyn-cam-a");
    let _cam = producer(&cam, "snow");
    let mut events = rig.state.events().subscribe();
    let before = (rig.element_count().await, rig.compositor_pads().await);

    let index = rig.add(inter(&cam)).await.unwrap();
    assert_eq!(index, NUM_INPUTS, "the lowest free slot");
    assert_eq!(rig.active().await, vec![0, 1, 2]);
    rig.wait_for_video(index).await;

    // Its thumbnail has a tile of its own, and the stored flow lists it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mv = rig.element(&Rig::id("mv_comp")).await.unwrap();
    let thumb = mv
        .static_pad(&PadMap::sink(Rig::pad_map().mv_thumb(index)))
        .unwrap();
    assert!(thumb.property::<i32>("width") > 1, "no thumbnail tile");
    assert!(thumb.property::<f64>("alpha") > 0.99, "thumbnail hidden");
    // And its cell in the tile output, whose frame holds all six slots.
    let tiles = rig.element(&Rig::id("tiles_comp")).await.unwrap();
    let cell = tiles.static_pad(&PadMap::sink(index)).unwrap();
    assert_eq!(
        ["xpos", "ypos", "width", "height"].map(|p| cell.property::<i32>(p)),
        [2 * CELL.0, 0, CELL.0, CELL.1],
        "input {index} is not in its tile cell"
    );
    let caps = rig
        .element("tilessink")
        .await
        .unwrap()
        .static_pad("sink")
        .unwrap()
        .current_caps()
        .expect("the tile output negotiated");
    let s = caps.structure(0).unwrap();
    assert_eq!(
        (
            s.get::<i32>("width").unwrap(),
            s.get::<i32>("height").unwrap()
        ),
        (3 * CELL.0, 2 * CELL.1)
    );
    assert_eq!(
        s.get::<&str>("format").unwrap(),
        "BGRA",
        "tiles_format ignored"
    );
    let flow = rig.state.get_flow(&rig.flow_id).await.unwrap();
    let stored = &flow.blocks[0].properties["dynamic_inputs"];
    assert!(
        matches!(stored, PV::String(s) if s.contains(&cam)),
        "dynamic_inputs not stored: {stored:?}"
    );
    let mut announced = false;
    while let Ok(event) = events.try_recv() {
        if let StromEvent::VisionMixerInputsChanged { active_inputs, .. } = event {
            announced = active_inputs == vec![0, 1, 2];
        }
    }
    assert!(announced, "VisionMixerInputsChanged not sent");

    // It goes on air like any input.
    rig.state
        .select_vision_mixer_preview(&rig.flow_id, BLOCK, index)
        .await
        .unwrap();
    rig.state
        .trigger_transition(&rig.flow_id, BLOCK, 0, index, "cut", 0)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mixer = rig.element(&Rig::id("mixer")).await.unwrap();
    let pad = mixer
        .static_pad(&PadMap::sink(Rig::pad_map().dist_input(index)))
        .unwrap();
    assert!(pad.property::<f64>("alpha") > 0.99, "not on program");
    assert_eq!(
        (pad.property::<i32>("width"), pad.property::<i32>("height")),
        (W as i32, H as i32),
        "the cut did not fit the input to program"
    );

    // On program: refused. Back to a static input, then it goes.
    let message = conflict(rig.remove(index).await);
    assert!(message.contains("program"), "{message}");
    rig.state
        .trigger_transition(&rig.flow_id, BLOCK, index, 0, "cut", 0)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.state
        .select_vision_mixer_preview(&rig.flow_id, BLOCK, 1)
        .await
        .unwrap();
    rig.remove(index).await.unwrap();
    assert_eq!(rig.active().await, vec![0, 1]);
    assert_eq!(
        (rig.element_count().await, rig.compositor_pads().await),
        before,
        "the input left elements or pads behind"
    );

    // The freed slot is no input any more.
    assert!(rig
        .state
        .select_vision_mixer_preview(&rig.flow_id, BLOCK, index)
        .await
        .is_err());

    tokio::time::sleep(Duration::from_millis(500)).await;
    rig.assert_program_unbroken("add, take, remove");
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removal_that_would_take_something_off_air_is_refused() {
    let rig = Rig::start().await;
    let _cam = producer("dyn-cam-b", "snow");

    // Static inputs never go; free and unknown slots are not found.
    let message = conflict(rig.remove(0).await);
    assert!(message.contains("static"), "{message}");
    assert!(matches!(
        rig.remove(4).await,
        Err(DynamicInputError::NotFound(_))
    ));
    assert!(matches!(
        rig.remove(99).await,
        Err(DynamicInputError::NotFound(_))
    ));

    // Bad slots.
    let mut req = inter("dyn-cam-b");
    req.index = Some(1);
    assert!(matches!(
        rig.add(req).await,
        Err(DynamicInputError::Invalid(_))
    ));
    let mut req = inter("dyn-cam-b");
    req.index = Some(MAX_INPUTS);
    assert!(matches!(
        rig.add(req).await,
        Err(DynamicInputError::Invalid(_))
    ));
    assert!(matches!(
        rig.add(inter("  ")).await,
        Err(DynamicInputError::Invalid(_))
    ));

    let mut req = inter("dyn-cam-b");
    req.index = Some(4);
    let index = rig.add(req).await.unwrap();
    assert_eq!(index, 4);
    let mut again = inter("dyn-cam-b");
    again.index = Some(4);
    assert!(matches!(
        rig.add(again).await,
        Err(DynamicInputError::Conflict(_))
    ));
    rig.wait_for_video(index).await;

    // On preview.
    rig.state
        .select_vision_mixer_preview(&rig.flow_id, BLOCK, index)
        .await
        .unwrap();
    let message = conflict(rig.remove(index).await);
    assert!(message.contains("preview"), "{message}");
    rig.state
        .select_vision_mixer_preview(&rig.flow_id, BLOCK, 1)
        .await
        .unwrap();

    // Part of a PiP (as its background, then as a zone source).
    rig.state
        .apply_vision_mixer_pip_config(
            &rig.flow_id,
            BLOCK,
            0,
            Some(index),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    let message = conflict(rig.remove(index).await);
    assert!(message.contains("PiP"), "{message}");
    let zone = Zone {
        rect: None,
        capacity: None,
        sources: vec![index],
        border: None,
    };
    rig.state
        .apply_vision_mixer_pip_config(
            &rig.flow_id,
            BLOCK,
            0,
            Some(0),
            vec![zone],
            Default::default(),
        )
        .await
        .unwrap();
    let message = conflict(rig.remove(index).await);
    assert!(message.contains("PiP"), "{message}");
    rig.state
        .apply_vision_mixer_pip_config(
            &rig.flow_id,
            BLOCK,
            0,
            Some(0),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();

    // During a transition.
    rig.state
        .trigger_transition(&rig.flow_id, BLOCK, 0, 1, "fade", 1500)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let message = conflict(rig.remove(index).await);
    assert!(message.contains("transition"), "{message}");
    // A refused removal leaves the input in place.
    assert_eq!(rig.active().await, vec![0, 1, 4]);
    // Once it ends, the input goes. Under load the transition can end
    // later than its 1.5 s of wall clock, so wait for it.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        match rig.remove(index).await {
            Ok(()) => break,
            Err(DynamicInputError::Conflict(m))
                if m.contains("transition") && std::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => panic!("removal after the transition: {e}"),
        }
    }

    rig.assert_program_unbroken("refused removals");
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_t_bar_stays_put_while_inputs_come_and_go() {
    let rig = Rig::start().await;
    let cam = rig.channel("dyn-cam-tbar");
    let _cam = producer(&cam, "snow");
    // A dynamic input that is there before the bar moves, removed under it.
    let other = rig.add(inter(&cam)).await.unwrap();
    rig.wait_for_video(other).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    rig.state
        .select_vision_mixer_preview(&rig.flow_id, BLOCK, 1)
        .await
        .unwrap();
    rig.state
        .hold_vision_mixer_transition(&rig.flow_id, BLOCK, "fade", 0.5)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mixer = rig.element(&Rig::id("mixer")).await.unwrap();
    let pm = Rig::pad_map();
    let alphas = || {
        [0, 1].map(|i| {
            mixer
                .static_pad(&PadMap::sink(pm.dist_input(i)))
                .unwrap()
                .property::<f64>("alpha")
        })
    };
    let held = alphas();
    assert!(
        held.iter().all(|a| *a > 0.01),
        "the bar does not mix 0 and 1: {held:?}"
    );

    // An input comes (its caps arrive, the multiview re-flows) and goes.
    let index = rig.add(inter(&cam)).await.unwrap();
    rig.wait_for_video(index).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(alphas(), held, "adding an input moved the held bar");
    rig.remove(index).await.unwrap();
    rig.remove(other).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(alphas(), held, "removing an input moved the held bar");

    rig.assert_program_unbroken("held T-bar");
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifty_adds_and_removes_leave_nothing_behind() {
    fifty_cycles(Rig::start().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NVIDIA CUDA (cudaupload, cudacompositor)"]
async fn cuda_fifty_adds_and_removes_leave_nothing_behind() {
    fifty_cycles(Rig::start_on("cuda").await).await;
}

async fn fifty_cycles(rig: Rig) {
    let (cam_a, cam_b) = (rig.channel("dyn-cycle-a"), rig.channel("dyn-cycle-b"));
    let _a = producer(&cam_a, "snow");
    let _b = producer(&cam_b, "red");
    let before = (rig.element_count().await, rig.compositor_pads().await);
    for cycle in 0..50 {
        let a = rig.add(inter(&cam_a)).await.unwrap();
        let b = rig.add(inter(&cam_b)).await.unwrap();
        assert_eq!((a, b), (2, 3), "cycle {cycle}");
        if cycle % 10 == 0 {
            rig.wait_for_video(b).await;
        }
        rig.remove(a).await.unwrap();
        rig.remove(b).await.unwrap();
        assert_eq!(
            (rig.element_count().await, rig.compositor_pads().await),
            before,
            "cycle {cycle} left elements or pads behind"
        );
    }
    assert_eq!(rig.active().await, vec![0, 1]);
    tokio::time::sleep(Duration::from_millis(500)).await;
    rig.assert_program_unbroken("50 cycles");
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dynamic_inputs_come_back_after_a_restart() {
    let rig = Rig::start().await;
    let _cam = producer("dyn-cam-c", "snow");
    let mut req = inter("dyn-cam-c");
    req.index = Some(5);
    req.label = Some("Guest".into());
    rig.add(req).await.unwrap();

    rig.state.stop_flow(&rig.flow_id).await.unwrap();
    rig.start_flow().await;
    let inputs = rig
        .state
        .vision_mixer_inputs(&rig.flow_id, BLOCK)
        .await
        .unwrap();
    assert_eq!(inputs.capacity, MAX_INPUTS);
    assert_eq!(inputs.num_static, NUM_INPUTS);
    let restored = inputs
        .inputs
        .iter()
        .find(|i| i.index == 5)
        .expect("restored");
    assert_eq!(restored.label, "Guest");
    assert!(matches!(
        &restored.source,
        Some(DynamicInputSource::Inter { channel, .. }) if channel == "dyn-cam-c"
    ));
    rig.wait_for_video(5).await;
    rig.stop().await;
}

/// `max_inputs` below `num_inputs`, or a broken `dynamic_inputs`, fail the
/// build instead of starting a mixer with other inputs than asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_slot_properties_fail_the_start() {
    gst::init().unwrap();
    strom::gpu::detect_gpu_capabilities();
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
    for (key, value) in [
        ("max_inputs", PV::UInt(1)),
        ("max_inputs", PV::UInt(1000)),
        ("dynamic_inputs", PV::String("not json".into())),
        (
            "dynamic_inputs",
            PV::String(r#"[{"index":0,"source":{"type":"inter","channel":"x"}}]"#.into()),
        ),
    ] {
        let mut flow = mixer_flow("cpu");
        flow.blocks[0].properties.insert(key.to_string(), value);
        let id = flow.id;
        state.upsert_flow(flow).await.unwrap();
        assert!(
            state.start_flow(&id).await.is_err(),
            "{key} accepted a bad value"
        );
    }
}

/// The mixer with only input 0 built with the flow (static); the tile
/// output leaves input 0 out and has two cells, for inputs 1 and 2.
fn one_static_input_two_cells(flow: &mut Flow) {
    flow.elements.retain(|e| e.id != "src1" && e.id != "caps1");
    flow.links
        .retain(|l| !l.from.starts_with("src1") && !l.from.starts_with("caps1"));
    let props = &mut flow.blocks[0].properties;
    props.insert("num_inputs".into(), PV::UInt(1));
    props.insert("initial_pvw_input".into(), PV::UInt(0));
    props.insert("tiles_first_input".into(), PV::UInt(1));
    props.insert("tiles_cells".into(), PV::UInt(2));
}

/// A mixer that starts with one input and grows: the others come at
/// runtime, the program cuts to them and back; the tile picture is two
/// cells wide whatever the capacity, for the inputs it names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mixer_with_one_input_grows_and_tiles_only_its_cells() {
    grows(Rig::start_with("cpu", one_static_input_two_cells).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NVIDIA CUDA (cudaupload, cudacompositor)"]
async fn cuda_a_mixer_with_one_input_grows_and_tiles_only_its_cells() {
    grows(Rig::start_with("cuda", one_static_input_two_cells).await).await;
}

async fn grows(rig: Rig) {
    let tiles = rig.element("tilessink").await.expect("tilessink");
    let caps = tiles
        .static_pad("sink")
        .unwrap()
        .current_caps()
        .expect("tiles caps");
    let s = caps.structure(0).unwrap();
    assert_eq!(
        (
            s.get::<i32>("width").unwrap(),
            s.get::<i32>("height").unwrap()
        ),
        (2 * CELL.0, CELL.1),
        "the tile picture is the cells', not the capacity's"
    );
    let tiles_comp = rig.element(&Rig::id("tiles_comp")).await.unwrap();
    assert_eq!(tiles_comp.sink_pads().len(), 0, "input 0 has no cell");

    let _producers: Vec<gst::Element> = ["a", "b", "c"]
        .iter()
        .map(|n| producer(&rig.channel(&format!("grow-{n}")), "ball"))
        .collect();
    let mut added = Vec::new();
    for n in ["a", "b", "c"] {
        added.push(
            rig.add(inter(&rig.channel(&format!("grow-{n}"))))
                .await
                .expect("add"),
        );
    }
    assert_eq!(added, [1, 2, 3]);
    for i in &added {
        rig.wait_for_video(*i).await;
    }
    // Inputs 1 and 2 sit in cells 0 and 1; input 3 has none.
    let xpos = |pad: &str| {
        tiles_comp
            .static_pad(pad)
            .map(|p| p.property::<i32>("xpos"))
    };
    assert_eq!(xpos("sink_0"), Some(0));
    assert_eq!(xpos("sink_1"), Some(CELL.0));
    assert_eq!(tiles_comp.sink_pads().len(), 2);
    assert!(rig
        .element(&Rig::id("queue_to_tiles_out_3"))
        .await
        .is_none());

    // On air and back.
    rig.state
        .select_vision_mixer_preview(&rig.flow_id, BLOCK, 2)
        .await
        .unwrap();
    rig.state
        .trigger_transition(&rig.flow_id, BLOCK, 0, 2, "cut", 0)
        .await
        .expect("cut to input 2");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mixer = rig.element(&Rig::id("mixer")).await.unwrap();
    let pad = mixer
        .static_pad(&PadMap::sink(Rig::pad_map().dist_input(2)))
        .unwrap();
    assert!(
        pad.property::<f64>("alpha") > 0.99,
        "input 2 not on program"
    );
    rig.state
        .trigger_transition(&rig.flow_id, BLOCK, 2, 0, "cut", 0)
        .await
        .expect("cut back to input 0");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Input 2 went back to preview with the cut: it stays, the others go.
    let message = conflict(rig.remove(2).await);
    assert!(message.contains("preview"), "{message}");
    for i in [3, 1] {
        rig.remove(i).await.expect("remove");
    }
    assert_eq!(tiles_comp.sink_pads().len(), 1, "a cell's pad stayed");
    assert_eq!(xpos("sink_1"), Some(CELL.0), "input 2 left its cell");
    rig.assert_program_unbroken("one input, three come and go");
    rig.stop().await;
}
