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
