//! T-bar with the effect of a transition (lmix SWI.3): the take from
//! program to preview held at a position shows that transition there.
//! CPU backend, so it runs in CI.

use gstreamer::prelude::*;
use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, PropertyValue as PV};
use tempfile::NamedTempFile;

const W: usize = 320;
const H: usize = 180;
const BLOCK: &str = "vm";

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

/// Red on program (input 0), blue on preview (input 1), program into an
/// RGBA appsink.
fn build_flow(dsk: bool) -> Flow {
    let mut flow = Flow::new("vm_tbar");
    let props: HashMap<String, PV> = [
        ("compositor_preference", PV::String("cpu".into())),
        ("num_inputs", PV::UInt(2)),
        ("pgm_resolution", PV::String(format!("{}x{}", W, H))),
        ("multiview_resolution", PV::String(format!("{}x{}", W, H))),
        ("initial_pgm_input", PV::UInt(0)),
        ("initial_pvw_input", PV::UInt(1)),
        (
            "num_dsk_inputs",
            PV::String(if dsk { "1" } else { "0" }.into()),
        ),
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
    let sources: &[(&str, &str)] = if dsk {
        &[
            ("red", "video_in_0"),
            ("blue", "video_in_1"),
            ("green", "dsk_in_0"),
        ]
    } else {
        &[("red", "video_in_0"), ("blue", "video_in_1")]
    };
    for (i, (pattern, pad)) in sources.iter().enumerate() {
        flow.elements.push(elem(
            &format!("src{i}"),
            "videotestsrc",
            vec![
                ("pattern", PV::String((*pattern).into())),
                ("is-live", PV::Bool(true)),
            ],
        ));
        flow.elements.push(elem(
            &format!("caps{i}"),
            "capsfilter",
            vec![(
                "caps",
                PV::String(format!("video/x-raw,width={W},height={H},framerate=30/1")),
            )],
        ));
        flow.links.push(strom_types::Link {
            from: format!("src{i}:src"),
            to: format!("caps{i}:sink"),
        });
        flow.links.push(strom_types::Link {
            from: format!("caps{i}:src"),
            to: format!("{BLOCK}:{pad}"),
        });
    }
    flow.elements.push(elem("conv", "videoconvert", vec![]));
    flow.elements.push(elem(
        "rgba",
        "capsfilter",
        vec![("caps", PV::String("video/x-raw,format=RGBA".into()))],
    ));
    flow.elements.push(elem(
        "pgmsink",
        "appsink",
        vec![
            ("sync", PV::Bool(false)),
            ("max-buffers", PV::UInt(1)),
            ("drop", PV::Bool(true)),
        ],
    ));
    for block in &mut flow.blocks {
        if let Some(builder) = strom::blocks::builtin::get_builder(&block.block_definition_id) {
            block.computed_external_pads = builder.get_external_pads(&block.properties);
        }
    }
    for (from, to) in [
        (format!("{BLOCK}:pgm_out"), "conv:sink".to_string()),
        ("conv:src".into(), "rgba:sink".into()),
        ("rgba:src".into(), "pgmsink:sink".into()),
    ] {
        flow.links.push(strom_types::Link { from, to });
    }
    flow
}

fn build_manager(flow: &Flow) -> (PipelineManager, NamedTempFile) {
    gstreamer::init().unwrap();
    strom::gpu::detect_gpu_capabilities();
    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let manager = PipelineManager::new(
        flow,
        EventBroadcaster::with_capacity(10),
        &registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
    )
    .expect("vision mixer pipeline with preview output should build");
    (manager, temp_file)
}

/// What a band of an RGBA frame mostly is: 'r', 'b', 'k' (black), 'm'
/// (red and blue mixed) or '?'.
fn band(sample: &gstreamer::Sample, x0: usize, x1: usize) -> char {
    use gstreamer_video::{VideoFrameRef, VideoInfo};
    let info = VideoInfo::from_caps(sample.caps().expect("caps")).expect("video info");
    let buffer = sample.buffer().expect("buffer");
    let frame = VideoFrameRef::from_buffer_ref_readable(buffer, &info).expect("map frame");
    let stride = info.stride()[0] as usize;
    let data = frame.plane_data(0).expect("plane 0");
    let mut counts: HashMap<char, u64> = HashMap::new();
    let mut n = 0u64;
    for y in (H / 4)..(3 * H / 4) {
        for x in x0..x1 {
            let o = y * stride + x * 4;
            let (r, g, b) = (data[o], data[o + 1], data[o + 2]);
            let c = match (r, g, b) {
                (r, g, b) if r > 200 && g < 60 && b < 60 => 'r',
                (r, g, b) if b > 200 && g < 60 && r < 60 => 'b',
                (r, g, b) if r < 30 && g < 30 && b < 30 => 'k',
                (r, g, b) if r > 90 && b > 90 && g < 60 => 'm',
                _ => '?',
            };
            *counts.entry(c).or_default() += 1;
            n += 1;
        }
    }
    counts
        .into_iter()
        .find(|(_, k)| k * 10 > n * 8)
        .map_or('?', |(c, _)| c)
}

/// Hold `transition` at `position`, then wait for program's left and
/// right halves to show `left` and `right`.
fn hold(
    manager: &PipelineManager,
    sink: &gstreamer_app::AppSink,
    transition: &str,
    position: f64,
    left: char,
    right: char,
) {
    manager
        .hold_vision_mixer_transition(BLOCK, transition, position)
        .expect("hold");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut last = ('?', '?');
    while std::time::Instant::now() < deadline {
        let Some(sample) = sink.try_pull_sample(gstreamer::ClockTime::from_mseconds(500)) else {
            continue;
        };
        last = (band(&sample, 0, W / 2), band(&sample, W / 2, W));
        if last == (left, right) {
            return;
        }
    }
    panic!("{transition} at {position}: program never showed {left}|{right}, last {last:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_take_shows_its_transition_at_the_bar() {
    let (mut manager, _registry) = build_manager(&build_flow(false));
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");

    hold(&manager, &sink, "fade", 0.0, 'r', 'r');
    // Slide left: blue comes in from the right over red.
    hold(&manager, &sink, "slide_left", 0.5, 'r', 'b');
    // Push right: blue pushes red out to the right.
    hold(&manager, &sink, "push_right", 0.5, 'b', 'r');
    hold(&manager, &sink, "dip_to_black", 0.5, 'k', 'k');
    hold(&manager, &sink, "fade", 0.5, 'm', 'm');
    // Back to the start: program as it was.
    hold(&manager, &sink, "slide_left", 0.0, 'r', 'r');

    assert!(manager
        .hold_vision_mixer_transition(BLOCK, "fade", 1.5)
        .is_err());
    manager.stop().expect("stop");
}

/// The bar rests only the input pads: an overlay fading in keeps fading,
/// and fade to black refuses the bar (it would bring program back under
/// the black).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_take_leaves_overlays_and_fade_to_black_alone() {
    let (mut manager, _registry) = build_manager(&build_flow(true));
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");
    hold(&manager, &sink, "fade", 0.0, 'r', 'r');

    manager
        .set_dsk_enabled(BLOCK, 0, 2, true, 3000)
        .expect("fade the overlay in over 3 s");
    std::thread::sleep(std::time::Duration::from_millis(300));
    manager
        .hold_vision_mixer_transition(BLOCK, "fade", 0.3)
        .expect("hold");
    std::thread::sleep(std::time::Duration::from_millis(700));
    let dsk_pad = manager
        .pipeline()
        .by_name(&format!("{BLOCK}:mixer"))
        .expect("mixer")
        .static_pad("sink_2")
        .expect("DSK pad");
    let alpha = dsk_pad.property::<f64>("alpha");
    assert!(
        alpha > 0.05 && alpha < 0.9,
        "the overlay should still be fading in after 1 s of 3, alpha {alpha}"
    );

    manager.fade_to_black(BLOCK, 0).expect("fade to black");
    assert!(
        manager
            .hold_vision_mixer_transition(BLOCK, "fade", 0.3)
            .is_err(),
        "the bar under fade to black"
    );
    manager.stop().expect("stop");
}

/// Every (alpha, xpos) a mixer pad passes through while `act` runs: the
/// mixer may render a frame between any two property writes.
fn states_during(pad: &gstreamer::Pad, act: impl FnOnce()) -> Vec<(f64, i32)> {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    let id = pad.connect_notify(None, move |pad, _| {
        log.lock()
            .unwrap()
            .push((pad.property::<f64>("alpha"), pad.property::<i32>("xpos")));
    });
    act();
    pad.disconnect(id);
    let states = seen.lock().unwrap().clone();
    states
}

/// A fader step goes straight from one bar position to the next, and the
/// take at the end straight to the new program: no write in between puts
/// a pad back in its resting place first (that would flash on air).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bar_step_never_passes_through_the_resting_pads() {
    let (mut manager, _registry) = build_manager(&build_flow(false));
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");
    let mixer = manager
        .pipeline()
        .by_name(&format!("{BLOCK}:mixer"))
        .expect("mixer");
    let incoming = mixer.static_pad("sink_1").expect("preview pad");

    hold(&manager, &sink, "slide_left", 0.3, 'r', '?');
    let step = states_during(&incoming, || {
        manager
            .hold_vision_mixer_transition(BLOCK, "slide_left", 0.31)
            .expect("hold");
    });
    assert!(!step.is_empty(), "the step should move the incoming pad");
    assert!(
        step.iter().all(|&(alpha, xpos)| alpha == 1.0 && xpos > 0),
        "the incoming pad passed through its resting place: {step:?}"
    );

    hold(&manager, &sink, "fade", 0.95, 'b', 'b');
    let take = states_during(&incoming, || {
        manager
            .trigger_transition(BLOCK, 0, 1, "cut", 0)
            .expect("take");
    });
    assert!(
        take.iter().all(|&(alpha, _)| alpha >= 0.95),
        "the take from the bar hid the new program on the way: {take:?}"
    );
    manager.stop().expect("stop");
}

/// A bar moved while a take animates, or while program fades back from
/// black, would cut the animation short: the bar waits for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bar_waits_for_a_running_take_or_fade_from_black() {
    let (mut manager, _registry) = build_manager(&build_flow(false));
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");
    hold(&manager, &sink, "fade", 0.0, 'r', 'r');

    manager
        .trigger_transition(BLOCK, 0, 1, "fade", 2000)
        .expect("auto take");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        manager
            .hold_vision_mixer_transition(BLOCK, "fade", 0.3)
            .is_err(),
        "the bar during an auto take"
    );

    std::thread::sleep(std::time::Duration::from_millis(2200));
    manager.fade_to_black(BLOCK, 0).expect("fade to black");
    manager
        .fade_to_black(BLOCK, 2000)
        .expect("fade back from black");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        manager
            .hold_vision_mixer_transition(BLOCK, "fade", 0.3)
            .is_err(),
        "the bar while program fades back from black"
    );

    std::thread::sleep(std::time::Duration::from_millis(2200));
    manager
        .hold_vision_mixer_transition(BLOCK, "fade", 0.3)
        .expect("the bar once everything has landed");
    manager.stop().expect("stop");
}

/// A push opens no gap between the two pictures on the way, and a take
/// without a duration is a cut whatever its transition: neither puts the
/// new program out of the picture for a frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_push_step_and_a_zero_length_slide_show_no_black() {
    let (mut manager, _registry) = build_manager(&build_flow(false));
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");
    let mixer = manager
        .pipeline()
        .by_name(&format!("{BLOCK}:mixer"))
        .expect("mixer");
    let outgoing = mixer.static_pad("sink_0").expect("program pad");
    let incoming = mixer.static_pad("sink_1").expect("preview pad");

    // Push left: the new picture follows the old one out to the left.
    hold(&manager, &sink, "push_left", 0.1, 'r', '?');
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut ids = Vec::new();
    for pad in [&outgoing, &incoming] {
        let (log, a, b) = (seen.clone(), outgoing.downgrade(), incoming.downgrade());
        ids.push(pad.connect_notify(Some("xpos"), move |_, _| {
            if let (Some(a), Some(b)) = (a.upgrade(), b.upgrade()) {
                log.lock()
                    .unwrap()
                    .push((a.property::<i32>("xpos"), b.property::<i32>("xpos")));
            }
        }));
    }
    manager
        .hold_vision_mixer_transition(BLOCK, "push_left", 0.9)
        .expect("hold");
    manager
        .hold_vision_mixer_transition(BLOCK, "push_left", 0.2)
        .expect("hold");
    for (pad, id) in [&outgoing, &incoming].into_iter().zip(ids) {
        pad.disconnect(id);
    }
    let states = seen.lock().unwrap().clone();
    assert!(states.len() >= 4, "both pads should move twice: {states:?}");
    assert!(
        states.iter().all(|&(old, new)| new <= old + W as i32),
        "a gap opened between the pictures: {states:?}"
    );

    hold(&manager, &sink, "fade", 0.0, 'r', 'r');
    let take = states_during(&incoming, || {
        manager
            .trigger_transition(BLOCK, 0, 1, "slide_left", 0)
            .expect("take");
    });
    assert!(
        take.iter().all(|&(alpha, xpos)| alpha == 0.0 || xpos == 0),
        "the new program left the picture on a cut: {take:?}"
    );
    manager.stop().expect("stop");
}

/// A take strom refuses (an unknown transition) changes nothing on air:
/// the state stays with the old program, so must the picture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_take_leaves_program_alone() {
    let (mut manager, _registry) = build_manager(&build_flow(false));
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");
    hold(&manager, &sink, "fade", 0.0, 'r', 'r');
    let incoming = manager
        .pipeline()
        .by_name(&format!("{BLOCK}:mixer"))
        .expect("mixer")
        .static_pad("sink_1")
        .expect("preview pad");
    let changes = states_during(&incoming, || {
        assert!(manager
            .trigger_transition(BLOCK, 0, 1, "slide_lft", 0)
            .is_err());
    });
    assert!(
        changes.iter().all(|&(alpha, _)| alpha == 0.0),
        "preview went on air for a refused take: {changes:?}"
    );

    // Nor does it lift fade to black: the bar stays refused under it.
    manager.fade_to_black(BLOCK, 0).expect("fade to black");
    assert!(manager
        .trigger_transition(BLOCK, 0, 1, "slide_lft", 0)
        .is_err());
    assert!(
        manager
            .hold_vision_mixer_transition(BLOCK, "fade", 0.3)
            .is_err(),
        "a refused take cancelled fade to black"
    );
    manager.stop().expect("stop");
}

/// A take with a duration to a PiP, while the bar holds an input on
/// program: that input goes off with the take instead of staying on air
/// over the PiP (the morph only moves the pads of both compositions).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_take_to_a_pip_clears_what_the_bar_held() {
    let mut flow = build_flow(false);
    flow.blocks[0]
        .properties
        .insert("num_pips".to_string(), PV::String("1".into()));
    let (mut manager, _registry) = build_manager(&flow);
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");

    // Blue slid half in over red, then a PiP of red alone on preview.
    hold(&manager, &sink, "slide_left", 0.5, 'r', 'b');
    manager
        .apply_vision_mixer_pip_config(
            BLOCK,
            0,
            Some(0),
            Vec::new(),
            strom_types::vision_mixer::PipTransforms::new(),
        )
        .expect("PiP config");
    manager
        .select_vision_mixer_pip_for_preview(BLOCK, 0)
        .expect("PiP to preview");
    manager
        .trigger_transition(BLOCK, 0, 0, "fade", 300)
        .expect("take the PiP");
    std::thread::sleep(std::time::Duration::from_millis(800));
    // `hold` with position 0 and a PiP on program changes nothing; it
    // waits for program to show red on both halves.
    hold(&manager, &sink, "fade", 0.0, 'r', 'r');
    manager.stop().expect("stop");
}

/// A take to a PiP that holds the input the bar slid in: that input goes
/// off before the morph moves it, instead of flashing whole in its PiP
/// place for a frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_take_to_a_pip_with_the_held_input_hides_it_before_moving_it() {
    let mut flow = build_flow(false);
    flow.blocks[0]
        .properties
        .insert("num_pips".to_string(), PV::String("1".into()));
    let (mut manager, _registry) = build_manager(&flow);
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");
    let incoming = manager
        .pipeline()
        .by_name(&format!("{BLOCK}:mixer"))
        .expect("mixer")
        .static_pad("sink_1")
        .expect("preview pad");

    hold(&manager, &sink, "slide_left", 0.5, 'r', 'b');
    let held_x = incoming.property::<i32>("xpos");
    // A PiP of blue alone on preview: blue is in the new composition.
    manager
        .apply_vision_mixer_pip_config(
            BLOCK,
            0,
            Some(1),
            Vec::new(),
            strom_types::vision_mixer::PipTransforms::new(),
        )
        .expect("PiP config");
    manager
        .select_vision_mixer_pip_for_preview(BLOCK, 0)
        .expect("PiP to preview");
    let take = states_during(&incoming, || {
        manager
            .trigger_transition(BLOCK, 0, 0, "fade", 300)
            .expect("take the PiP");
    });
    assert!(
        take.iter()
            .all(|&(alpha, xpos)| alpha < 1.0 || xpos == held_x),
        "blue moved while fully visible: held at {held_x}, {take:?}"
    );
    std::thread::sleep(std::time::Duration::from_millis(800));
    hold(&manager, &sink, "fade", 0.0, 'b', 'b');
    manager.stop().expect("stop");
}

/// Another input chosen for preview while a push is held: on the next bar
/// step the new preview takes its place before the old one goes off, so
/// no frame shows black where the old one was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_preview_under_a_held_bar_replaces_the_old_one_without_a_gap() {
    let mut flow = build_flow(false);
    flow.blocks[0]
        .properties
        .insert("num_inputs".to_string(), PV::UInt(3));
    for (id, ty, props) in [
        (
            "src2",
            "videotestsrc",
            vec![
                ("pattern", PV::String("green".into())),
                ("is-live", PV::Bool(true)),
            ],
        ),
        (
            "caps2",
            "capsfilter",
            vec![(
                "caps",
                PV::String(format!("video/x-raw,width={W},height={H},framerate=30/1")),
            )],
        ),
    ] {
        flow.elements.push(elem(id, ty, props));
    }
    for (from, to) in [
        ("src2:src".to_string(), "caps2:sink".to_string()),
        ("caps2:src".to_string(), format!("{BLOCK}:video_in_2")),
    ] {
        flow.links.push(strom_types::Link { from, to });
    }
    let builder = strom::blocks::builtin::get_builder("builtin.vision_mixer").expect("builder");
    flow.blocks[0].computed_external_pads = builder.get_external_pads(&flow.blocks[0].properties);
    let (mut manager, _registry) = build_manager(&flow);
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");
    let mixer = manager
        .pipeline()
        .by_name(&format!("{BLOCK}:mixer"))
        .expect("mixer");

    hold(&manager, &sink, "push_left", 0.5, 'r', 'b');
    manager
        .select_vision_mixer_preview(BLOCK, 2, 3)
        .expect("green to preview");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut ids = Vec::new();
    let pads = [
        mixer.static_pad("sink_1").expect("old preview"),
        mixer.static_pad("sink_2").expect("new preview"),
    ];
    for pad in &pads {
        let log = seen.clone();
        ids.push(pad.connect_notify(Some("alpha"), move |pad, _| {
            log.lock()
                .unwrap()
                .push((pad.name().to_string(), pad.property::<f64>("alpha")));
        }));
    }
    manager
        .hold_vision_mixer_transition(BLOCK, "push_left", 0.55)
        .expect("hold");
    for (pad, id) in pads.iter().zip(ids) {
        pad.disconnect(id);
    }
    let log = seen.lock().unwrap().clone();
    let old_off = log.iter().position(|(n, a)| n == "sink_1" && *a == 0.0);
    let new_on = log.iter().position(|(n, a)| n == "sink_2" && *a == 1.0);
    assert!(
        matches!((new_on, old_off), (Some(on), Some(off)) if on < off),
        "the old preview went off before the new one came in: {log:?}"
    );
    manager.stop().expect("stop");
}

/// A cut needs no stream time: one taken while the mixer answers no
/// position (here before the pipeline runs) still succeeds, so the state
/// follows the picture it has already put on air.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_does_not_need_the_mixer_position() {
    let (manager, _registry) = build_manager(&build_flow(false));
    manager
        .trigger_transition(BLOCK, 0, 1, "cut", 0)
        .expect("cut without a position");
    let incoming = manager
        .pipeline()
        .by_name(&format!("{BLOCK}:mixer"))
        .expect("mixer")
        .static_pad("sink_1")
        .expect("preview pad");
    assert_eq!(incoming.property::<f64>("alpha"), 1.0);
}

/// The effect changed while the bar is held (push, then fade): the
/// outgoing picture covers its place before the incoming one fades, so
/// no frame shows the incoming one faint over black.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_effect_change_under_the_held_bar_shows_no_black() {
    let (mut manager, _registry) = build_manager(&build_flow(false));
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");
    let mixer = manager
        .pipeline()
        .by_name(&format!("{BLOCK}:mixer"))
        .expect("mixer");
    let outgoing = mixer.static_pad("sink_0").expect("program pad");
    let incoming = mixer.static_pad("sink_1").expect("preview pad");

    hold(&manager, &sink, "push_left", 0.5, 'r', 'b');
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut ids = Vec::new();
    for pad in [&outgoing, &incoming] {
        let (log, a, b) = (seen.clone(), outgoing.downgrade(), incoming.downgrade());
        ids.push(pad.connect_notify(None, move |_, _| {
            if let (Some(a), Some(b)) = (a.upgrade(), b.upgrade()) {
                log.lock()
                    .unwrap()
                    .push((a.property::<i32>("xpos"), b.property::<f64>("alpha")));
            }
        }));
    }
    manager
        .hold_vision_mixer_transition(BLOCK, "fade", 0.55)
        .expect("hold");
    for (pad, id) in [&outgoing, &incoming].into_iter().zip(ids) {
        pad.disconnect(id);
    }
    let states = seen.lock().unwrap().clone();
    assert!(
        states
            .iter()
            .all(|&(old_x, new_alpha)| new_alpha == 1.0 || old_x == 0),
        "the incoming picture faded while the old one was still pushed out: {states:?}"
    );
    manager.stop().expect("stop");
}

/// After a wipe take on the GL mixer the outgoing input's take slot is
/// masked away (an inverted wipe ends fully transparent). The bar back to
/// that input shows it again: here a fade at half mixes the two.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bar_after_a_wipe_shows_the_wiped_input_again() {
    if gstreamer::init().is_err()
        || ["glvideomixerelement", "glshader", "gltestsrc"]
            .iter()
            .any(|e| gstreamer::ElementFactory::find(e).is_none())
    {
        return;
    }
    let mut flow = build_flow(false);
    for (k, v) in [
        ("compositor_preference", PV::String("gpu".into())),
        ("gl_download", PV::Bool(true)),
    ] {
        flow.blocks[0].properties.insert(k.to_string(), v);
    }
    let flow_id = flow.id;
    let (mut manager, _registry) = build_manager(&flow);
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");
    hold(&manager, &sink, "fade", 0.0, 'r', 'r');

    manager
        .trigger_transition(BLOCK, 0, 1, "wipe_left", 400)
        .expect("wipe take");
    std::thread::sleep(std::time::Duration::from_millis(1500));
    // What the API does after the take: blue on program, red on preview.
    let state = strom::blocks::builtin::vision_mixer::overlay::get_overlay_state(&flow_id, BLOCK)
        .expect("mixer state");
    state.set_pgm_input(Some(1));
    state.set_pvw_input(Some(0));
    hold(&manager, &sink, "fade", 0.0, 'b', 'b');
    hold(&manager, &sink, "fade", 0.5, 'm', 'm');
    manager.stop().expect("stop");
}

/// Button 1 changed from slide left to slide up under the held bar: the
/// incoming picture never stands whole at its resting place on the way
/// from one effect's geometry to the other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_effect_under_the_held_bar_never_shows_the_whole_preview() {
    let (mut manager, _registry) = build_manager(&build_flow(false));
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");
    let incoming = manager
        .pipeline()
        .by_name(&format!("{BLOCK}:mixer"))
        .expect("mixer")
        .static_pad("sink_1")
        .expect("preview pad");

    hold(&manager, &sink, "slide_left", 0.5, 'r', 'b');
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    let id = incoming.connect_notify(None, move |pad, _| {
        log.lock().unwrap().push((
            pad.property::<f64>("alpha"),
            pad.property::<i32>("xpos"),
            pad.property::<i32>("ypos"),
        ));
    });
    manager
        .hold_vision_mixer_transition(BLOCK, "slide_up", 0.5)
        .expect("hold");
    incoming.disconnect(id);
    let states = seen.lock().unwrap().clone();
    assert!(
        states
            .iter()
            .all(|&(alpha, x, y)| alpha == 0.0 || (x, y) != (0, 0)),
        "the whole preview went on air: {states:?}"
    );
    manager.stop().expect("stop");
}

/// A take with a duration to a PiP after a held push: program's pad goes
/// back to its place before the bar's preview goes off, so no part of the
/// picture is black at the start of the take.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_take_to_a_pip_after_a_held_push_shows_no_black() {
    let mut flow = build_flow(false);
    flow.blocks[0]
        .properties
        .insert("num_pips".to_string(), PV::String("1".into()));
    let (mut manager, _registry) = build_manager(&flow);
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");
    let mixer = manager
        .pipeline()
        .by_name(&format!("{BLOCK}:mixer"))
        .expect("mixer");
    let outgoing = mixer.static_pad("sink_0").expect("program pad");
    let incoming = mixer.static_pad("sink_1").expect("preview pad");

    hold(&manager, &sink, "push_left", 0.5, 'r', 'b');
    manager
        .apply_vision_mixer_pip_config(
            BLOCK,
            0,
            Some(1),
            Vec::new(),
            strom_types::vision_mixer::PipTransforms::new(),
        )
        .expect("PiP config");
    manager
        .select_vision_mixer_pip_for_preview(BLOCK, 0)
        .expect("PiP to preview");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut ids = Vec::new();
    for pad in [&outgoing, &incoming] {
        let (log, a, b) = (seen.clone(), outgoing.downgrade(), incoming.downgrade());
        ids.push(pad.connect_notify(None, move |_, _| {
            if let (Some(a), Some(b)) = (a.upgrade(), b.upgrade()) {
                log.lock()
                    .unwrap()
                    .push((a.property::<i32>("xpos"), b.property::<f64>("alpha")));
            }
        }));
    }
    manager
        .trigger_transition(BLOCK, 0, 0, "fade", 300)
        .expect("take the PiP");
    for (pad, id) in [&outgoing, &incoming].into_iter().zip(ids) {
        pad.disconnect(id);
    }
    let states = seen.lock().unwrap().clone();
    // While program is still pushed out, the preview must still cover the
    // other half.
    assert!(
        states
            .iter()
            .all(|&(old_x, new_alpha)| old_x == 0 || new_alpha == 1.0),
        "half the picture went black: {states:?}"
    );
    manager.stop().expect("stop");
}

/// Button 1 from fade to a wipe under the held bar: the bar shows both as
/// a fade, so nothing changes on air (no rest in between).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wipe_after_a_fade_under_the_held_bar_changes_nothing() {
    let (mut manager, _registry) = build_manager(&build_flow(false));
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");
    let incoming = manager
        .pipeline()
        .by_name(&format!("{BLOCK}:mixer"))
        .expect("mixer")
        .static_pad("sink_1")
        .expect("preview pad");
    hold(&manager, &sink, "fade", 0.5, 'm', 'm');
    let states = states_during(&incoming, || {
        manager
            .hold_vision_mixer_transition(BLOCK, "wipe_left", 0.5)
            .expect("hold");
    });
    assert!(
        states.iter().all(|&(alpha, _)| alpha == 0.5),
        "the preview left the mix: {states:?}"
    );
    manager.stop().expect("stop");
}

/// A take with a duration to a PiP after a dip held past black: program
/// comes back fully before the bar's preview goes off, so the picture is
/// never black at the start of the take.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_take_to_a_pip_after_a_held_dip_shows_no_black() {
    let mut flow = build_flow(false);
    flow.blocks[0]
        .properties
        .insert("num_pips".to_string(), PV::String("1".into()));
    let (mut manager, _registry) = build_manager(&flow);
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");
    let mixer = manager
        .pipeline()
        .by_name(&format!("{BLOCK}:mixer"))
        .expect("mixer");
    let outgoing = mixer.static_pad("sink_0").expect("program pad");
    let incoming = mixer.static_pad("sink_1").expect("preview pad");

    // Past the black: program off, blue half in.
    hold(&manager, &sink, "dip_to_black", 0.75, '?', '?');
    assert_eq!(outgoing.property::<f64>("alpha"), 0.0);
    manager
        .apply_vision_mixer_pip_config(
            BLOCK,
            0,
            Some(0),
            Vec::new(),
            strom_types::vision_mixer::PipTransforms::new(),
        )
        .expect("PiP config");
    manager
        .select_vision_mixer_pip_for_preview(BLOCK, 0)
        .expect("PiP to preview");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut ids = Vec::new();
    for pad in [&outgoing, &incoming] {
        let (log, a, b) = (seen.clone(), outgoing.downgrade(), incoming.downgrade());
        ids.push(pad.connect_notify(Some("alpha"), move |_, _| {
            if let (Some(a), Some(b)) = (a.upgrade(), b.upgrade()) {
                log.lock()
                    .unwrap()
                    .push((a.property::<f64>("alpha"), b.property::<f64>("alpha")));
            }
        }));
    }
    manager
        .trigger_transition(BLOCK, 0, 0, "fade", 300)
        .expect("take the PiP");
    for (pad, id) in [&outgoing, &incoming].into_iter().zip(ids) {
        pad.disconnect(id);
    }
    let states = seen.lock().unwrap().clone();
    assert!(
        states
            .iter()
            .all(|&(old_alpha, new_alpha)| old_alpha == 1.0 || new_alpha > 0.0),
        "both pads off at once: {states:?}"
    );
    manager.stop().expect("stop");
}

/// A cut to a PiP and from a PiP to an input whose picture sits on a
/// higher pad than the old one: the new picture is on before the old one
/// goes off, so no frame between two writes is black.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_to_and_from_a_pip_shows_no_black() {
    let mut flow = build_flow(false);
    flow.blocks[0]
        .properties
        .insert("num_pips".to_string(), PV::String("1".into()));
    let (mut manager, _registry) = build_manager(&flow);
    manager.start().expect("start pipeline");
    let mixer = manager
        .pipeline()
        .by_name(&format!("{BLOCK}:mixer"))
        .expect("mixer");
    let red = mixer.static_pad("sink_0").expect("red pad");
    let blue = mixer.static_pad("sink_1").expect("blue pad");

    // A cut and the swap after it, as the state layer does; each alpha
    // write logs (red, blue), and some pad must be on in every one.
    let cut = |what: &str| {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut ids = Vec::new();
        for pad in [&red, &blue] {
            let (log, a, b) = (seen.clone(), red.downgrade(), blue.downgrade());
            ids.push(pad.connect_notify(Some("alpha"), move |_, _| {
                if let (Some(a), Some(b)) = (a.upgrade(), b.upgrade()) {
                    log.lock()
                        .unwrap()
                        .push((a.property::<f64>("alpha"), b.property::<f64>("alpha")));
                }
            }));
        }
        let (_, old_pgm, new_pgm, _) = manager
            .trigger_transition(BLOCK, 0, 0, "cut", 0)
            .expect("cut");
        for (pad, id) in [&red, &blue].into_iter().zip(ids) {
            pad.disconnect(id);
        }
        manager
            .update_vision_mixer_after_take(BLOCK, new_pgm, old_pgm)
            .expect("swap");
        let states = seen.lock().unwrap().clone();
        assert!(
            states.iter().all(|&(r, b)| r > 0.0 || b > 0.0),
            "{what}: both pads off at once: {states:?}"
        );
    };
    let pip = |bg: usize| {
        manager
            .apply_vision_mixer_pip_config(
                BLOCK,
                0,
                Some(bg),
                Vec::new(),
                strom_types::vision_mixer::PipTransforms::new(),
            )
            .expect("PiP config");
    };

    // Red on program, a PiP of blue alone on preview.
    pip(1);
    manager
        .select_vision_mixer_pip_for_preview(BLOCK, 0)
        .expect("PiP to preview");
    cut("red to a PiP of blue");
    assert_eq!(blue.property::<f64>("alpha"), 1.0);
    cut("back to red");

    // A PiP of red alone on program, blue on preview.
    pip(0);
    cut("red to a PiP of red");
    manager
        .select_vision_mixer_preview(BLOCK, 1, 2)
        .expect("blue to preview");
    cut("a PiP of red to blue");
    assert_eq!(blue.property::<f64>("alpha"), 1.0);
    manager.stop().expect("stop");
}
