//! The vision mixer's preview output (`pvw_out`, `enable_preview_out`):
//! at program size, what the multiview's PVW shows. Runs on the CPU
//! backend (CI); the CUDA variant is ignored like the other CUDA tests.

use gstreamer::prelude::*;
use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::vision_mixer::{
    NormRect, Zone, ZoneBorder, ENABLE_PREVIEW_OUT_PROPERTY, PREVIEW_OUTPUT_PAD,
};
use strom_types::{Flow, PropertyValue as PV};
use tempfile::NamedTempFile;

const W: usize = 320;
const H: usize = 180;
const BLOCK: &str = "vm";
/// Input colours: program, preview, the third input.
const PATTERNS: [&str; 3] = ["red", "green", "blue"];

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

/// Three coloured live inputs into a mixer with one PiP, its preview
/// output into an RGBA appsink.
fn build_flow(backend: &str) -> Flow {
    let mut flow = Flow::new(format!("vm_preview_out_{}", backend));
    let props: HashMap<String, PV> = [
        ("compositor_preference", PV::String(backend.into())),
        ("num_inputs", PV::UInt(3)),
        ("num_pips", PV::UInt(1)),
        ("pgm_resolution", PV::String(format!("{}x{}", W, H))),
        (
            "multiview_resolution",
            PV::String(format!("{}x{}", 2 * W, 2 * H)),
        ),
        ("initial_pgm_input", PV::UInt(0)),
        ("initial_pvw_input", PV::UInt(1)),
        (ENABLE_PREVIEW_OUT_PROPERTY, PV::Bool(true)),
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
    for (i, pattern) in PATTERNS.iter().enumerate() {
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
            to: format!("{BLOCK}:video_in_{i}"),
        });
    }
    if backend == "cuda" {
        flow.elements.push(elem("down", "cudadownload", vec![]));
    } else if backend == "gpu" {
        flow.elements.push(elem("down", "gldownload", vec![]));
    } else {
        flow.elements.push(elem("down", "identity", vec![]));
    }
    flow.elements.push(elem("conv", "videoconvert", vec![]));
    flow.elements.push(elem(
        "rgba",
        "capsfilter",
        vec![("caps", PV::String("video/x-raw,format=RGBA".into()))],
    ));
    flow.elements.push(elem(
        "pvwsink",
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
        (
            format!("{BLOCK}:{PREVIEW_OUTPUT_PAD}"),
            "down:sink".to_string(),
        ),
        ("down:src".into(), "conv:sink".into()),
        ("conv:src".into(), "rgba:sink".into()),
        ("rgba:src".into(), "pvwsink:sink".into()),
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

/// The colour (index into `PATTERNS`) most of a band of an RGBA frame
/// shows, if one covers more than 80 % of it.
fn band_color(sample: &gstreamer::Sample, x0: usize, x1: usize) -> Option<usize> {
    use gstreamer_video::{VideoFrameRef, VideoInfo};
    let info = VideoInfo::from_caps(sample.caps().expect("caps")).expect("video info");
    assert_eq!(
        (info.width() as usize, info.height() as usize),
        (W, H),
        "program size"
    );
    let buffer = sample.buffer().expect("buffer");
    let frame = VideoFrameRef::from_buffer_ref_readable(buffer, &info).expect("map frame");
    let stride = info.stride()[0] as usize;
    let data = frame.plane_data(0).expect("plane 0");
    let mut counts = [0u64; 3];
    let mut n = 0u64;
    for y in (H / 4)..(3 * H / 4) {
        let row = &data[y * stride..];
        for x in x0..x1 {
            let (r, g, b) = (row[x * 4], row[x * 4 + 1], row[x * 4 + 2]);
            let hot = |v: u8| v > 150;
            let cold = |v: u8| v < 80;
            if hot(r) && cold(g) && cold(b) {
                counts[0] += 1;
            } else if cold(r) && hot(g) && cold(b) {
                counts[1] += 1;
            } else if cold(r) && cold(g) && hot(b) {
                counts[2] += 1;
            }
            n += 1;
        }
    }
    (0..3).find(|&c| counts[c] * 10 > n * 8)
}

/// Fraction of yellow pixels in a rectangle of an RGBA frame.
fn yellow_fraction(sample: &gstreamer::Sample, xs: (usize, usize), ys: (usize, usize)) -> f64 {
    use gstreamer_video::{VideoFrameRef, VideoInfo};
    let info = VideoInfo::from_caps(sample.caps().expect("caps")).expect("video info");
    let buffer = sample.buffer().expect("buffer");
    let frame = VideoFrameRef::from_buffer_ref_readable(buffer, &info).expect("map frame");
    let stride = info.stride()[0] as usize;
    let data = frame.plane_data(0).expect("plane 0");
    let (mut yellow, mut n) = (0u64, 0u64);
    for y in ys.0..ys.1 {
        for x in xs.0..xs.1 {
            let o = y * stride + x * 4;
            if data[o] > 200 && data[o + 1] > 200 && data[o + 2] < 80 {
                yellow += 1;
            }
            n += 1;
        }
    }
    yellow as f64 / n as f64
}

/// Pull preview frames until the left and right halves show `left` and
/// `right` (indices into `PATTERNS`).
fn wait_for(sink: &gstreamer_app::AppSink, left: usize, right: usize, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut last = (None, None);
    while std::time::Instant::now() < deadline {
        let Some(sample) = sink.try_pull_sample(gstreamer::ClockTime::from_mseconds(500)) else {
            continue;
        };
        last = (band_color(&sample, 0, W / 2), band_color(&sample, W / 2, W));
        if last == (Some(left), Some(right)) {
            return;
        }
    }
    panic!(
        "{what}: preview output never showed {} | {} within 20 s, last {:?}",
        PATTERNS[left], PATTERNS[right], last
    );
}

fn preview_follows_the_mixer(backend: &str) {
    let (mut manager, _registry) = build_manager(&build_flow(backend));
    manager.start().expect("start pipeline");
    let sink = manager
        .pipeline()
        .by_name("pvwsink")
        .expect("pvwsink")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink");

    // At program size, the preview input alone (not the program's red).
    wait_for(&sink, 1, 1, "initial preview");

    manager
        .select_vision_mixer_preview(BLOCK, 2, 3)
        .expect("preview input 2");
    wait_for(&sink, 2, 2, "another input on preview");

    // A PiP on preview: input 0 as background, input 2 in the right half.
    let zone = Zone {
        rect: Some(NormRect {
            x: 0.5,
            y: 0.0,
            w: 0.5,
            h: 1.0,
        }),
        capacity: None,
        sources: vec![2],
        border: Some(ZoneBorder {
            color: "#FFFF00".into(),
            width: 8.0,
        }),
    };
    manager
        .apply_vision_mixer_pip_config(BLOCK, 0, Some(0), vec![zone], Default::default())
        .expect("PiP config");
    manager
        .select_vision_mixer_pip_for_preview(BLOCK, 0)
        .expect("PiP on preview");
    wait_for(&sink, 0, 2, "PiP on preview");
    // The zone's border: yellow just above the box (input 2 fits into the
    // right half as 160x90 at y 45).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut yellow = 0.0;
    while std::time::Instant::now() < deadline && yellow < 0.8 {
        if let Some(sample) = sink.try_pull_sample(gstreamer::ClockTime::from_mseconds(500)) {
            yellow = yellow_fraction(&sample, (W / 2 + 20, W - 20), (39, 44));
        }
    }
    assert!(
        yellow >= 0.8,
        "zone border on the preview output: {yellow:.2} yellow"
    );

    // The mirror's handlers hold the preview pads weakly: nothing keeps
    // the pipeline alive after stop.
    drop(sink);
    let pipeline = manager.pipeline_weak();
    manager.stop().expect("stop");
    drop(manager);
    assert!(
        pipeline.upgrade().is_none(),
        "pipeline still alive after drop: a reference cycle"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preview_output_shows_what_preview_shows() {
    preview_follows_the_mixer("cpu");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs NVIDIA CUDA (cudaupload, cudacompositor)"]
async fn preview_output_shows_what_preview_shows_on_cuda() {
    preview_follows_the_mixer("cuda");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a GL context (glvideomixer)"]
async fn preview_output_shows_what_preview_shows_on_gl() {
    preview_follows_the_mixer("gpu");
}
