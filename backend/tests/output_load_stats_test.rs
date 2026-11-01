//! Block statistics of what loads an output: the Inter Input's receive
//! queue (frames waiting, frames dropped) and the Video Encoder's encode
//! time.
//!
//! Both build the real block through its builder, run it in a pipeline and
//! read the statistics through `StatsCollector`, as
//! `GET /api/flows/{id}/rtp-stats` does. They count, they do not time: the
//! frames encoded, the frames queued and dropped.

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::Duration;
use strom::blocks::builtin::inter::InterInputBuilder;
use strom::blocks::builtin::videoenc::VideoEncBuilder;
use strom::blocks::{BlockBuildContext, BlockBuildResult, BlockBuilder};
use strom::stats::StatsCollector;
use strom::whip_session_manager::WhipSessionManager;
use strom_types::stats::{FlowStats, InterInputStats, VideoEncoderStats};
use strom_types::{BlockDefinition, Flow, PropertyValue};

const FRAMES: u64 = 30;
const TIMEOUT: Duration = Duration::from_secs(20);

/// The inter elements are linked into Strom and registered at startup, so a
/// test binary registers them too.
fn register_inter() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        gstrsinter::plugin_register_static().expect("register the inter plugin");
    });
}

fn context() -> BlockBuildContext {
    BlockBuildContext::new(Vec::new(), "all".to_string())
}

/// The block's elements in `pipeline`, linked inside, by id.
fn install(pipeline: &gst::Pipeline, built: &BlockBuildResult) -> HashMap<String, gst::Element> {
    let by_id: HashMap<String, gst::Element> = built.elements.iter().cloned().collect();
    for element in by_id.values() {
        pipeline.add(element).unwrap();
    }
    for (from, to) in &built.internal_links {
        by_id[&from.element_id]
            .link_pads(
                from.pad_name.as_deref(),
                &by_id[&to.element_id],
                to.pad_name.as_deref(),
            )
            .expect("internal link");
    }
    by_id
}

/// The elements behind block `id`'s first external input and output.
fn ends(
    definition: BlockDefinition,
    id: &str,
    by_id: &HashMap<String, gst::Element>,
) -> (Option<gst::Element>, Option<gst::Element>) {
    let pads = definition.external_pads;
    let element = |pad: Option<&strom_types::block::ExternalPad>| {
        pad.map(|p| by_id[&format!("{id}:{}", p.internal_element_id)].clone())
    };
    (element(pads.inputs.first()), element(pads.outputs.first()))
}

fn definition(blocks: Vec<BlockDefinition>, id: &str) -> BlockDefinition {
    blocks
        .into_iter()
        .find(|b| b.id == id)
        .expect("block definition")
}

/// A flow of one block `id` of `definition`, as the collector reads it.
fn flow_of(id: &str, definition: &str) -> Flow {
    serde_json::from_str(&format!(
        r#"{{
            "id": "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0",
            "name": "output load",
            "blocks": [{{
                "id": "{id}",
                "block_definition_id": "{definition}",
                "properties": {{}},
                "position": {{"x": 0.0, "y": 0.0}}
            }}]
        }}"#
    ))
    .expect("flow fixture")
}

fn collect(pipeline: &gst::Pipeline, flow: &Flow) -> FlowStats {
    StatsCollector::collect_flow_stats(pipeline, flow, &WhipSessionManager::new())
}

fn wait_for_eos(pipeline: &gst::Pipeline) {
    let bus = pipeline.bus().expect("bus");
    let msg = bus
        .timed_pop_filtered(
            gst::ClockTime::from_nseconds(TIMEOUT.as_nanos() as u64),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("EOS within the timeout");
    if let gst::MessageView::Error(e) = msg.view() {
        panic!("pipeline error: {} ({:?})", e.error(), e.debug());
    }
}

/// Every frame through the software encoder counts, with its time.
#[test]
fn video_encoder_reports_each_frame_it_encoded() {
    gst::init().unwrap();
    if !common::require_or_skip(&common::missing_elements(&[
        "videotestsrc",
        "x264enc",
        "h264parse",
        "fakesink",
    ])) {
        return;
    }
    // The encoder builder reads the process-global video convert mode.
    strom::gpu::detect_gpu_capabilities();
    let properties = HashMap::from([
        ("codec".to_string(), PropertyValue::String("h264".into())),
        (
            "encoder_preference".to_string(),
            PropertyValue::String("software".into()),
        ),
    ]);
    let built = VideoEncBuilder
        .build("enc", &properties, &context())
        .expect("build the encoder block");

    let pipeline = gst::Pipeline::new();
    let elements = install(&pipeline, &built);
    let src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", FRAMES as i32)
        .build()
        .unwrap();
    let sink = gst::ElementFactory::make("fakesink").build().unwrap();
    pipeline.add_many([&src, &sink]).unwrap();
    let blocks = strom::blocks::builtin::videoenc::get_blocks();
    let (input, output) = ends(definition(blocks, "builtin.videoenc"), "enc", &elements);
    src.link(&input.unwrap()).unwrap();
    output.unwrap().link(&sink).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();
    wait_for_eos(&pipeline);
    let stats = collect(&pipeline, &flow_of("enc", "builtin.videoenc"));
    pipeline.set_state(gst::State::Null).unwrap();

    let block = &stats.block_stats[0];
    let encoder = VideoEncoderStats::from_statistics(&block.stats).expect("encoder stats");
    assert_eq!(encoder.frames, FRAMES, "every frame timed once");
    assert!(encoder.encode_time_ns > 0, "the frames took time");
}

/// A consumer that does not take its frames (paused, so its live source
/// never pushes) keeps what fits in `max_time` and drops the rest: queued
/// and dropped add up to what was published.
#[test]
fn inter_input_reports_queued_and_dropped_frames() {
    gst::init().unwrap();
    register_inter();
    if !common::require_or_skip(&common::missing_elements(&[
        "videotestsrc",
        "intersink",
        "fakesink",
    ])) {
        return;
    }
    const CHANNEL: &str = "output-load-stats-test";
    const MAX_TIME_MS: u64 = 200;

    let properties = HashMap::from([
        ("channel".to_string(), PropertyValue::String(CHANNEL.into())),
        (
            "max_time".to_string(),
            PropertyValue::String(MAX_TIME_MS.to_string()),
        ),
    ]);
    let built = InterInputBuilder
        .build("in", &properties, &context())
        .expect("build the inter input block");
    let consumer = gst::Pipeline::new();
    let elements = install(&consumer, &built);
    let blocks = strom::blocks::builtin::inter::get_blocks();
    let (_, output) = ends(definition(blocks, "builtin.inter_input"), "in", &elements);
    let sink = gst::ElementFactory::make("fakesink").build().unwrap();
    consumer.add(&sink).unwrap();
    output.unwrap().link(&sink).unwrap();
    // Subscribed from PAUSED on; a live source pushes only in PLAYING.
    let _ = consumer.set_state(gst::State::Paused);

    // 30 fps frames, published as fast as they are made.
    let producer = gst::parse::launch(&format!(
        "videotestsrc num-buffers={FRAMES} ! video/x-raw,framerate=30/1 \
         ! intersink producer-name={CHANNEL} sync=false"
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    producer.set_state(gst::State::Playing).unwrap();
    wait_for_eos(&producer);

    let stats = collect(&consumer, &flow_of("in", "builtin.inter_input"));
    producer.set_state(gst::State::Null).unwrap();
    consumer.set_state(gst::State::Null).unwrap();

    let block = &stats.block_stats[0];
    let queue = InterInputStats::from_statistics(&block.stats).expect("queue stats");
    assert_eq!(queue.max_time_ns, MAX_TIME_MS * 1_000_000);
    assert!(queue.queued_buffers > 0, "frames wait: {queue:?}");
    assert!(queue.dropped_buffers > 0, "the queue overflowed: {queue:?}");
    assert!(queue.queued_time_ns <= queue.max_time_ns, "{queue:?}");
    assert_eq!(
        queue.queued_buffers + queue.dropped_buffers,
        FRAMES,
        "{queue:?}"
    );
}
