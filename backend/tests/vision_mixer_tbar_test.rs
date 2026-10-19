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
