//! Show Playback block: sounds preloaded, played, faded and stopped in a running
//! pipeline built by the real builder, and heard at the zone outputs.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom::blocks::builtin::show_playback::{self, sound, ShowPlaybackBuilder, ShowPlaybackKey};
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::element::ElementPadRef;
use strom_types::show_playback::{
    AddSoundRequest, FadeEnd, SoundFadeRequest, SoundRepeat, SoundRoute, SoundStart, SoundState,
    ZoneLevelRequest,
};
use strom_types::PropertyValue;

/// Peak level of the zone outputs' latest buffers, and when one first
/// carried sound.
#[derive(Default)]
struct Ear {
    peak: f32,
    first_sound: Option<Instant>,
    buffers: u64,
    /// Peak of every buffer since the first sound.
    peaks: Vec<f32>,
}

struct Rig {
    pipeline: gst::Pipeline,
    key: ShowPlaybackKey,
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
    rig_with(true)
}

/// [`rig`], with sinks that sync to the clock or not.
fn rig_with(sync: bool) -> Rig {
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
    let built = ShowPlaybackBuilder
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
        let sink = gst_app::AppSink::builder().sync(sync).build();
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
                    ear.buffers += 1;
                    if peak > 0.05 && ear.first_sound.is_none() {
                        ear.first_sound = Some(Instant::now());
                    }
                    if ear.first_sound.is_some() {
                        ear.peaks.push(peak);
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
        key: ShowPlaybackKey {
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
    let block = show_playback::running(&rig.key)?;
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
        repeat: SoundRepeat::Once,
    }
}

/// SND.12: a preloaded sound is heard within the zone's latency of `play`,
/// only in the zone it is routed to.
#[test]
fn a_preloaded_sound_is_heard_at_once_in_its_zone_only() {
    let rig = rig();
    let block = show_playback::running(&rig.key).expect("the block runs");
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
    let block = show_playback::running(&rig.key).unwrap();
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

/// A sound plays to the end of its file, however fast it was decoded,
/// and the zone runs on after it.
#[test]
fn a_sound_plays_to_its_end_and_the_zone_runs_on() {
    let rig = rig();
    let block = show_playback::running(&rig.key).unwrap();
    let mut request = hall_tone(&rig, "tone");
    request.start = SoundStart::Play;
    sound::add(&block, &request).unwrap();
    wait_for("the Hall hears the tone", || {
        rig.ears[0].lock().unwrap().first_sound.is_some()
    });
    let heard = rig.ears[0].lock().unwrap().first_sound.unwrap();

    // The file is 3 s; it is decoded in a few ms.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        state_of(&rig, "tone"),
        Some(SoundState::Playing),
        "gone before its end"
    );
    assert!(
        rig.ears[0].lock().unwrap().peak > 0.05,
        "silent in the middle"
    );
    wait_for("the tone is over", || state_of(&rig, "tone").is_none());
    let played = heard.elapsed();
    assert!(
        played > Duration::from_millis(2700),
        "over after {played:?} of a 3 s file"
    );

    let buffers = rig.ears[0].lock().unwrap().buffers;
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        rig.ears[0].lock().unwrap().buffers > buffers,
        "the zone stopped with the sound"
    );
}

/// A zone keeps time by itself, not by its sink: behind a sink that does
/// not hold it back (as a PipeWire `pulsesink` on the monotonic clock did),
/// a 3 s sound is still playing after 1.5 s. Without its live silence the
/// zone mixed the whole file at once, the sound counted as ended at once,
/// and a stop or panic no longer found it while it was heard.
#[test]
fn a_zone_keeps_time_behind_a_sink_that_does_not_sync() {
    let rig = rig_with(false);
    let block = show_playback::running(&rig.key).unwrap();
    let mut request = hall_tone(&rig, "tone");
    request.start = SoundStart::Play;
    sound::add(&block, &request).unwrap();
    wait_for("the Hall hears the tone", || {
        rig.ears[0].lock().unwrap().first_sound.is_some()
    });
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        state_of(&rig, "tone"),
        Some(SoundState::Playing),
        "a 3 s sound counted as over after 1.5 s"
    );
}

/// GO to heard is mostly the zone pipeline's latency: the zone mixer's
/// own slack, one block, and its live silence. The silence reports its
/// buffer as latency, so at audiotestsrc's default (1024 samples, 21 ms)
/// and the mixer's old 30 ms this rig waited 60 ms; now 45 ms.
#[test]
fn a_zone_waits_little_before_it_plays() {
    let rig = rig_with(true);
    wait_for("the zones run", || rig.ears[0].lock().unwrap().buffers > 0);
    let mut query = gst::query::Latency::new();
    assert!(rig.pipeline.query(&mut query), "latency query");
    let (live, min, _) = query.result();
    assert!(live, "the zones are live");
    assert!(
        min <= gst::ClockTime::from_mseconds(50),
        "zone latency {} ms, budget 50 ms",
        min.mseconds()
    );
}

/// A 1 s tone in the Hall, starting at once, repeated as `repeat` says.
fn repeated(rig: &Rig, id: &str, repeat: SoundRepeat) -> AddSoundRequest {
    let dir = std::path::Path::new(&rig.tone).parent().unwrap().join(id);
    std::fs::create_dir_all(&dir).unwrap();
    AddSoundRequest {
        uri: write_tone(&dir, 1),
        start: SoundStart::Play,
        repeat,
        ..hall_tone(rig, id)
    }
}

/// SND.3: a sound played 3 times runs 3 s without a gap, then ends.
#[test]
fn a_repeated_sound_plays_its_times_without_a_gap() {
    let rig = rig();
    let block = show_playback::running(&rig.key).unwrap();
    sound::add(&block, &repeated(&rig, "rain", SoundRepeat::Times(3))).unwrap();
    wait_for("the Hall hears the rain", || {
        rig.ears[0].lock().unwrap().first_sound.is_some()
    });
    let heard = rig.ears[0].lock().unwrap().first_sound.unwrap();
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(
        state_of(&rig, "rain"),
        Some(SoundState::Playing),
        "ended early"
    );
    wait_for("the rain is over", || state_of(&rig, "rain").is_none());
    let played = heard.elapsed();
    assert!(
        played > Duration::from_millis(2800) && played < Duration::from_millis(3600),
        "3 plays of 1 s took {played:?}"
    );
    let ear = rig.ears[0].lock().unwrap();
    // 10 ms buffers: the first 2.9 s are all tone.
    let quiet = ear.peaks.iter().take(290).filter(|p| **p < 0.05).count();
    assert_eq!(quiet, 0, "{quiet} quiet buffers between the plays");
}

/// A preloaded sound waits through its first seek and still repeats.
#[test]
fn a_preloaded_repeated_sound_plays_twice() {
    let rig = rig();
    let block = show_playback::running(&rig.key).unwrap();
    let request = AddSoundRequest {
        start: SoundStart::Preload,
        ..repeated(&rig, "bell", SoundRepeat::Times(2))
    };
    sound::add(&block, &request).unwrap();
    wait_for("the bell is ready", || {
        state_of(&rig, "bell") == Some(SoundState::Ready)
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        rig.ears[0].lock().unwrap().first_sound.is_none(),
        "heard before play"
    );
    let go = Instant::now();
    sound::play(&block, "bell").unwrap();
    wait_for("the bell is over", || state_of(&rig, "bell").is_none());
    let played = go.elapsed();
    assert!(
        played > Duration::from_millis(1800) && played < Duration::from_millis(2600),
        "2 plays of 1 s took {played:?}"
    );
}

/// A sound repeated forever runs on, and a fade over a repeat's seam
/// still fades.
#[test]
fn a_forever_sound_runs_on_and_fades_across_its_seams() {
    let rig = rig();
    let block = show_playback::running(&rig.key).unwrap();
    sound::add(&block, &repeated(&rig, "wind", SoundRepeat::Forever)).unwrap();
    wait_for("the Hall hears the wind", || {
        rig.ears[0].lock().unwrap().first_sound.is_some()
    });
    std::thread::sleep(Duration::from_millis(2200));
    assert_eq!(state_of(&rig, "wind"), Some(SoundState::Playing));
    assert!(rig.ears[0].lock().unwrap().peak > 0.05);

    // From 2.2 s to 3.4 s, over the seam at 3 s.
    sound::fade(
        &block,
        "wind",
        &SoundFadeRequest {
            gain_db: -60.0,
            duration_ms: 1200,
            end: FadeEnd::Keep,
        },
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(600));
    let half = rig.ears[0].lock().unwrap().peak;
    assert!(
        half > 0.01 && half < 0.45,
        "halfway through the fade: {half}"
    );
    std::thread::sleep(Duration::from_millis(1000));
    assert!(
        rig.ears[0].lock().unwrap().peak < 0.01,
        "still heard after the fade, past the seam"
    );
    assert_eq!(
        state_of(&rig, "wind"),
        Some(SoundState::Playing),
        "keep: it runs on"
    );
}

/// A muted zone is silent; the sound plays on.
#[test]
fn a_muted_zone_is_silent() {
    let rig = rig();
    let block = show_playback::running(&rig.key).unwrap();
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
    let block = show_playback::running(&rig.key).unwrap();
    let mut request = hall_tone(&rig, "x");
    request.routes[0].zone = 7;
    assert!(sound::add(&block, &request).is_err());
    request.routes[0].zone = 1;
    request.routes[0].zone_channel = 5;
    assert!(sound::add(&block, &request).is_err());
}
