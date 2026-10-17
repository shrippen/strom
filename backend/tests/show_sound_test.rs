//! Show Sound block: sounds preloaded, played, faded and stopped in a running
//! pipeline built by the real builder, and heard at the zone outputs.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom::blocks::builtin::show_sound::{self, sound, ShowSoundBuilder, ShowSoundKey};
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::element::ElementPadRef;
use strom_types::show_sound::{
    AddSoundRequest, SoundRoute, SoundStart, SoundState, ZoneLevelRequest,
};
use strom_types::PropertyValue;

/// Peak level of the zone outputs' latest buffers, and when one first
/// carried sound.
#[derive(Default)]
struct Ear {
    peak: f32,
    first_sound: Option<Instant>,
}

struct Rig {
    pipeline: gst::Pipeline,
    key: ShowSoundKey,
    ears: Vec<Arc<Mutex<Ear>>>,
    _dir: tempfile::TempDir,
    tone: String,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// A 1 kHz mono tone of `seconds` as a WAV file.
fn write_tone(dir: &std::path::Path, seconds: u32) -> String {
    let path = dir.join("tone.wav");
    let writer = gst::parse::launch(&format!(
        "audiotestsrc wave=sine freq=1000 volume=0.5 num-buffers={} samplesperbuffer=480 \
         ! audio/x-raw,format=S16LE,rate=48000,channels=1 ! wavenc ! filesink name=out",
        seconds * 100
    ))
    .unwrap();
    writer
        .downcast_ref::<gst::Bin>()
        .unwrap()
        .by_name("out")
        .unwrap()
        .set_property("location", &path);
    writer.set_state(gst::State::Playing).unwrap();
    writer
        .bus()
        .unwrap()
        .timed_pop_filtered(gst::ClockTime::from_seconds(10), &[gst::MessageType::Eos])
        .expect("the tone is written");
    writer.set_state(gst::State::Null).unwrap();
    path.to_string_lossy().into_owned()
}

/// Two stereo zones, each into an appsink that measures its level.
fn rig() -> Rig {
    gst::init().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let tone = write_tone(dir.path(), 3);
    let flow_id = uuid::Uuid::new_v4();
    let props: HashMap<String, PropertyValue> = [
        (
            "zones".to_string(),
            PropertyValue::String(
                r#"[{"name":"Hall","channels":2},{"name":"Foyer","channels":2}]"#.into(),
            ),
        ),
        (
            "_flow_id".to_string(),
            PropertyValue::String(flow_id.to_string()),
        ),
    ]
    .into();
    let built = ShowSoundBuilder
        .build(
            "snd",
            &props,
            &BlockBuildContext::new(Vec::new(), "all".to_string()),
        )
        .unwrap();
    let pipeline = gst::Pipeline::new();
    let by_id: HashMap<String, gst::Element> = built.elements.iter().cloned().collect();
    for (_, element) in &built.elements {
        pipeline.add(element).unwrap();
    }
    for (from, to) in &built.internal_links {
        let (
            ElementPadRef {
                element_id: f,
                pad_name: fp,
            },
            ElementPadRef {
                element_id: t,
                pad_name: tp,
            },
        ) = (from, to);
        by_id[f]
            .link_pads(fp.as_deref(), &by_id[t], tp.as_deref())
            .unwrap();
    }
    let mut ears = Vec::new();
    for zone in 0..2 {
        let ear = Arc::new(Mutex::new(Ear::default()));
        let sink = gst_app::AppSink::builder().sync(true).build();
        let heard = ear.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let peak = map
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|b| f32::from_le_bytes(*b).abs())
                        .fold(0.0, f32::max);
                    let mut ear = heard.lock().unwrap();
                    ear.peak = peak;
                    if peak > 0.05 && ear.first_sound.is_none() {
                        ear.first_sound = Some(Instant::now());
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline.add(&sink).unwrap();
        by_id[&format!("snd:zone_out_{zone}")].link(&sink).unwrap();
        ears.push(ear);
    }
    pipeline.set_state(gst::State::Playing).unwrap();
    let _ = pipeline.state(gst::ClockTime::from_seconds(5));
    Rig {
        pipeline,
        key: ShowSoundKey {
            flow_id,
            block_id: "snd".into(),
        },
        ears,
        _dir: dir,
        tone,
    }
}

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn state_of(rig: &Rig, id: &str) -> Option<SoundState> {
    let block = show_sound::running(&rig.key)?;
    sound::state(&block)
        .sounds
        .into_iter()
        .find(|s| s.id == id)
        .map(|s| s.state)
}

/// The mono tone on both channels of the Hall, preloaded.
fn hall_tone(rig: &Rig, id: &str) -> AddSoundRequest {
    AddSoundRequest {
        id: id.into(),
        uri: rig.tone.clone(),
        gain_db: 0.0,
        routes: vec![
            SoundRoute {
                channel: 0,
                zone: 0,
                zone_channel: 0,
                gain_db: 0.0,
            },
            SoundRoute {
                channel: 0,
                zone: 0,
                zone_channel: 1,
                gain_db: 0.0,
            },
        ],
        start: SoundStart::Preload,
    }
}

/// SND.12: a preloaded sound is heard within the zone's latency of `play`,
/// only in the zone it is routed to.
#[test]
fn a_preloaded_sound_is_heard_at_once_in_its_zone_only() {
    let rig = rig();
    let block = show_sound::running(&rig.key).expect("the block runs");
    sound::add(&block, &hall_tone(&rig, "door")).unwrap();
    wait_for("the sound is ready", || {
        state_of(&rig, "door") == Some(SoundState::Ready)
    });
    // Ready but held: nothing heard yet.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        rig.ears[0].lock().unwrap().first_sound.is_none(),
        "heard before play"
    );

    let go = Instant::now();
    sound::play(&block, "door").unwrap();
    wait_for("the Hall hears the sound", || {
        rig.ears[0].lock().unwrap().first_sound.is_some()
    });
    let latency = rig.ears[0].lock().unwrap().first_sound.unwrap() - go;
    println!("play to first sample: {latency:?}");
    assert!(
        latency < Duration::from_millis(150),
        "heard after {latency:?}"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        rig.ears[1].lock().unwrap().first_sound.is_none(),
        "the Foyer heard it"
    );
}

/// A stop with a fade silences the sound and takes its branch down.
#[test]
fn a_stopped_sound_fades_out_and_leaves_the_pipeline() {
    let rig = rig();
    let block = show_sound::running(&rig.key).unwrap();
    let before = rig.pipeline.children().len();
    let mut request = hall_tone(&rig, "music");
    request.start = SoundStart::Play;
    sound::add(&block, &request).unwrap();
    wait_for("the Hall hears the music", || {
        rig.ears[0].lock().unwrap().first_sound.is_some()
    });
    sound::stop(&block, "music", 200).unwrap();
    wait_for("the music is gone", || state_of(&rig, "music").is_none());
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        rig.ears[0].lock().unwrap().peak < 0.01,
        "still heard after the stop"
    );
    assert_eq!(
        rig.pipeline.children().len(),
        before,
        "the branch stayed in the pipeline"
    );
}

/// A muted zone is silent; the sound plays on.
#[test]
fn a_muted_zone_is_silent() {
    let rig = rig();
    let block = show_sound::running(&rig.key).unwrap();
    let mut request = hall_tone(&rig, "rain");
    request.start = SoundStart::Play;
    sound::add(&block, &request).unwrap();
    wait_for("the Hall hears the rain", || {
        rig.ears[0].lock().unwrap().first_sound.is_some()
    });
    sound::set_zone(
        &block,
        0,
        &ZoneLevelRequest {
            mute: Some(true),
            duration_ms: 50,
            ..Default::default()
        },
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        rig.ears[0].lock().unwrap().peak < 0.001,
        "the muted zone is heard"
    );
    assert_eq!(state_of(&rig, "rain"), Some(SoundState::Playing));
}

#[test]
fn a_route_to_a_missing_zone_is_refused() {
    let rig = rig();
    let block = show_sound::running(&rig.key).unwrap();
    let mut request = hall_tone(&rig, "x");
    request.routes[0].zone = 7;
    assert!(sound::add(&block, &request).is_err());
    request.routes[0].zone = 1;
    request.routes[0].zone_channel = 5;
    assert!(sound::add(&block, &request).is_err());
}
