//! Guard for the mixer's `sample_rate` property.
//!
//! A live mixer that times out before its first input arrives fixates the
//! default rate (44.1 kHz). An input at 48 kHz that arrives later then fails
//! with not-negotiated, because the channel strip only converts the format.
//! With `sample_rate` set, every bus is pinned to that rate and inputs are
//! resampled, so a late input at any rate is mixed.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::blocks::builtin::mixer::MixerBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::element::ElementPadRef;
use strom_types::PropertyValue;

const REQUIRED: &[&str] = &[
    "audiomixer",
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "capsfilter",
    "valve",
    "tee",
    "queue",
    "volume",
    "level",
    "audiopanorama",
    "appsink",
];

fn plugins_available() -> bool {
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|e| gst::ElementFactory::find(e).is_none())
        .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        strom_types::env::var_opt("STROM_REQUIRE_GST_PLUGINS").is_none(),
        "STROM_REQUIRE_GST_PLUGINS is set but these elements are missing: {}",
        missing.join(", ")
    );
    false
}

fn link(by_id: &HashMap<String, gst::Element>, from: &ElementPadRef, to: &ElementPadRef) {
    let src = &by_id[&from.element_id];
    let sink = &by_id[&to.element_id];
    src.link_pads(from.pad_name.as_deref(), sink, to.pad_name.as_deref())
        .unwrap_or_else(|e| panic!("link {:?} -> {:?}: {:?}", from, to, e));
}

/// Mixer with one channel; its input opens only after `hold`, so the mixer
/// has negotiated on its own by then. Returns the rate of the main output
/// once the input flows, or the first error.
fn run(sample_rate: Option<u64>, hold: Duration) -> Result<i32, String> {
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert("num_channels".to_string(), PropertyValue::UInt(1));
    if let Some(rate) = sample_rate {
        props.insert("sample_rate".to_string(), PropertyValue::UInt(rate));
    }
    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = MixerBuilder
        .build("mix", &props, &ctx)
        .expect("mixer block builds");

    let pipeline = gst::Pipeline::new();
    let mut by_id: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        by_id.insert(id.clone(), element.clone());
    }
    for (from, to) in &built.internal_links {
        link(&by_id, from, to);
    }

    let src = gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .build()
        .unwrap();
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("rate", 48_000i32)
                .field("channels", 2i32)
                .build(),
        )
        .build()
        .unwrap();
    let valve = gst::ElementFactory::make("valve")
        .property("drop", true)
        .build()
        .unwrap();
    let sink = gst_app::AppSink::builder().sync(false).build();
    pipeline
        .add_many([&src, &caps, &valve, sink.upcast_ref()])
        .unwrap();
    gst::Element::link_many([&src, &caps, &valve]).unwrap();
    // Into channel 1 as a flow links it: the block's external pad.
    let input = &MixerBuilder
        .get_external_pads(&props)
        .expect("mixer has pads")
        .inputs[0];
    valve
        .link_pads(
            Some("src"),
            &by_id[&format!("mix:{}", input.internal_element_id)],
            Some(input.internal_pad_name.as_str()),
        )
        .unwrap();
    by_id["mix:main_out_tee"]
        .link_pads(
            Some("src_%u"),
            sink.upcast_ref::<gst::Element>(),
            Some("sink"),
        )
        .unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let error = |deadline: Instant| -> Option<String> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let timeout = gst::ClockTime::from_nseconds(left.as_nanos() as u64);
            let msg = bus.timed_pop_filtered(timeout, &[gst::MessageType::Error])?;
            if let gst::MessageView::Error(e) = msg.view() {
                return Some(format!("{} ({:?})", e.error(), e.debug()));
            }
        }
    };

    let result = (|| {
        if let Some(e) = error(Instant::now() + hold) {
            return Err(e);
        }
        valve.set_property("drop", false);
        if let Some(e) = error(Instant::now() + Duration::from_millis(1500)) {
            return Err(e);
        }
        let sample = sink
            .try_pull_sample(gst::ClockTime::from_seconds(2))
            .ok_or("no output")?;
        let rate = sample
            .caps()
            .and_then(|c| c.structure(0).and_then(|s| s.get::<i32>("rate").ok()))
            .ok_or("output without rate")?;
        Ok(rate)
    })();
    pipeline.set_state(gst::State::Null).unwrap();
    result
}

fn init() -> bool {
    gst::init().unwrap();
    let _ = gst_plugins_lsp::plugin_register_static();
    plugins_available()
}

#[test]
fn late_input_at_another_rate_is_mixed_at_the_set_rate() {
    if !init() {
        return;
    }
    assert_eq!(run(Some(48_000), Duration::from_secs(1)), Ok(48_000));
}

#[test]
fn set_rate_resamples_inputs() {
    if !init() {
        return;
    }
    assert_eq!(run(Some(44_100), Duration::from_millis(0)), Ok(44_100));
}
