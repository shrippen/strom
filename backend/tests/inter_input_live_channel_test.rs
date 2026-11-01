//! An Inter Input switches channel while its flow plays: the consumer
//! takes the other producer's stream without a restart (lmix moves a
//! running output to another mix's sound this way).

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::element::Link;
use strom_types::{Flow, FlowId, PropertyValue as PV};
use tempfile::NamedTempFile;

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

fn block(id: &str, definition: &str, props: Vec<(&str, PV)>) -> strom_types::BlockInstance {
    strom_types::BlockInstance {
        id: id.to_string(),
        block_definition_id: definition.to_string(),
        name: None,
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    }
}

/// A live tone (`wave`: 0 sine, 4 silence) published on an Inter Output.
fn producer(name: &str, wave: i64) -> Flow {
    let mut flow = Flow::new(name);
    flow.elements.push(elem(
        "tone",
        "audiotestsrc",
        vec![("is-live", PV::Bool(true)), ("wave", PV::Int(wave))],
    ));
    flow.blocks
        .push(block("out", "builtin.inter_output", Vec::new()));
    flow.links.push(Link {
        from: "tone:src".into(),
        to: "out:sink".into(),
    });
    flow
}

fn channel(flow: FlowId) -> String {
    format!("strom_{flow}_out")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inter_input_moves_to_another_channel_while_playing() {
    gst::init().unwrap();
    // `main` registers the inter elements at startup, once.
    static INTER: std::sync::Once = std::sync::Once::new();
    INTER.call_once(|| gstrsinter::plugin_register_static().expect("inter plugin"));

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

    let silent = producer("inter_live_silent", 4);
    let tone = producer("inter_live_tone", 0);
    let (silent_id, tone_id) = (silent.id, tone.id);
    let mut consumer = Flow::new("inter_live_consumer");
    consumer.blocks.push(block(
        "in",
        "builtin.inter_input",
        vec![("channel", PV::String(channel(silent_id)))],
    ));
    consumer.elements.push(elem("sink", "fakesink", Vec::new()));
    consumer.links.push(Link {
        from: "in:src".into(),
        to: "sink:sink".into(),
    });
    let consumer_id = consumer.id;
    for flow in [silent, tone, consumer] {
        let id = flow.id;
        state.upsert_flow(flow).await.unwrap();
        state.start_flow(&id).await.unwrap();
    }

    // Buffers with sound reaching the consumer's sink.
    let loud = Arc::new(AtomicUsize::new(0));
    let total = Arc::new(AtomicUsize::new(0));
    {
        let pipelines = state.pipelines_read().await;
        let sink = pipelines[&consumer_id].pipeline().by_name("sink").unwrap();
        let (loud, total) = (Arc::clone(&loud), Arc::clone(&total));
        sink.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(gst::PadProbeData::Buffer(b)) = &info.data {
                    total.fetch_add(1, Ordering::Relaxed);
                    let map = b.map_readable().unwrap();
                    if map.iter().any(|&byte| byte != 0) {
                        loud.fetch_add(1, Ordering::Relaxed);
                    }
                }
                gst::PadProbeReturn::Ok
            });
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        total.load(Ordering::Relaxed) > 5,
        "the consumer got nothing"
    );
    assert_eq!(
        loud.load(Ordering::Relaxed),
        0,
        "the silent channel had sound"
    );

    let (_, rejected) = state
        .update_block_properties(
            &consumer_id,
            "in",
            HashMap::from([("channel".to_string(), PV::String(channel(tone_id)))]),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(rejected.is_empty(), "channel rejected: {rejected:?}");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        loud.load(Ordering::Relaxed) > 5,
        "the consumer did not take the tone's channel"
    );
    // Stored, so a restart of the flow keeps it.
    let stored = state.get_flow(&consumer_id).await.unwrap();
    let input = stored.blocks.iter().find(|b| b.id == "in").unwrap();
    assert!(
        matches!(input.properties.get("channel"), Some(PV::String(c)) if *c == channel(tone_id)),
        "channel not stored: {:?}",
        input.properties.get("channel")
    );

    for id in [consumer_id, silent_id, tone_id] {
        state.stop_flow(&id).await.unwrap();
    }
}
