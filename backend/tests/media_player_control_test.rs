//! Regression tests for the Media Player's control calls, driven through a
//! real flow.
//!
//! A burst of playlist jumps and pauses used to hang the player for good
//! (issue #963): a call never returned, and stopping the flow then never
//! returned either.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use strom::blocks::builtin::mediaplayer::{MediaPlayerKey, MEDIA_PLAYER_REGISTRY};
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::{Flow, Link, PropertyValue};
use tempfile::NamedTempFile;

const BLOCK_ID: &str = "player";

fn new_app_state() -> (AppState, [NamedTempFile; 2]) {
    let storage_file = NamedTempFile::new().unwrap();
    let blocks_file = NamedTempFile::new().unwrap();
    let state = AppState::new(
        JsonFileStorage::new(storage_file.path()),
        blocks_file.path(),
        std::env::temp_dir(),
        vec![],
        "all".to_string(),
        vec![],
        false,
        false,
    );
    (state, [storage_file, blocks_file])
}

/// Run the default GLib main context, where the player's internal bus watch
/// (EOS → next file) is dispatched, as Strom does in production.
fn run_glib_main_loop() {
    std::thread::spawn(|| {
        let main_loop = gstreamer::glib::MainLoop::new(None, false);
        main_loop.run();
    });
}

/// A short H.264 clip in MP4, the kind of file the hang was seen with.
/// `x264enc` is in gstreamer1.0-plugins-ugly, `mp4mux` in -good, both
/// installed in CI.
fn write_h264_clip(dir: &std::path::Path, name: &str, frames: u32) -> String {
    write_clip(
        dir,
        name,
        &format!(
            "videotestsrc num-buffers={} ! video/x-raw,width=160,height=90,framerate=25/1 \
             ! x264enc tune=zerolatency key-int-max=10 ! h264parse ! mp4mux ! filesink name=out",
            frames
        ),
    )
}

/// A WAV file with a minute of tone and no video.
fn write_audio_clip(dir: &std::path::Path, name: &str) -> String {
    write_clip(
        dir,
        name,
        "audiotestsrc num-buffers=500 ! audio/x-raw,rate=8000,channels=1 \
         ! wavenc ! filesink name=out",
    )
}

/// Run `launch`, whose `filesink name=out` writes `dir/name`, and return the
/// file's URI.
fn write_clip(dir: &std::path::Path, name: &str, launch: &str) -> String {
    use gstreamer::prelude::*;
    let path = dir.join(name);
    let writer = gstreamer::parse::launch(launch).expect("x264enc and mp4mux are installed in CI");
    writer
        .downcast_ref::<gstreamer::Bin>()
        .unwrap()
        .by_name("out")
        .unwrap()
        .set_property("location", &path);
    writer.set_state(gstreamer::State::Playing).unwrap();
    let msg = writer
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gstreamer::ClockTime::from_seconds(20),
            &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
        )
        .expect("writing the clip finishes");
    assert!(
        matches!(msg.view(), gstreamer::MessageView::Eos(_)),
        "{:?}",
        msg
    );
    writer.set_state(gstreamer::State::Null).unwrap();
    gstreamer::glib::filename_to_uri(&path, None)
        .unwrap()
        .to_string()
}

/// A Media Player with default properties (decoder = decodebin3) and
/// `playlist`, its `video_out` into a fakesink.
fn build_player_flow(name: &str, playlist: &[String]) -> Flow {
    let mut flow = Flow::new(name);
    flow.elements.push(strom_types::Element {
        id: "sink".to_string(),
        element_type: "fakesink".to_string(),
        properties: HashMap::new(),
        position: [300.0, 200.0].into(),
        pad_properties: HashMap::new(),
    });
    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK_ID.to_string(),
        block_definition_id: "builtin.media_player".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "playlist".to_string(),
                PropertyValue::String(serde_json::to_string(playlist).unwrap()),
            );
            p.insert("decode".to_string(), PropertyValue::Bool(true));
            p.insert("num_audio_tracks".to_string(), PropertyValue::UInt(0));
            p
        },
        position: strom_types::block::Position { x: 100.0, y: 200.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow.links.push(Link {
        from: format!("{}:video_out", BLOCK_ID),
        to: "sink:sink".to_string(),
    });
    flow
}

/// A small deterministic PRNG, so a failing sequence can be replayed.
struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// The repro from issue #963: playlist jumps and pauses from one thread, in
/// random order, each issued as soon as the previous returns. This used to
/// deadlock inside uridecodebin3, typically within a few hundred calls: a
/// state change walked into the parsebin urisourcebin's typefind thread was
/// still adding. The flow could then not be stopped either.
///
/// `STROM_STORM_CALLS` and `STROM_STORM_SEED` replay a longer or different
/// sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_storm_of_jumps_and_pauses_never_hangs_the_player() {
    gstreamer::init().unwrap();
    run_glib_main_loop();

    // A deadlocked GStreamer thread can keep the process alive after the test
    // fails, and teardown can block on it. Whatever happens, end the process.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(300));
        eprintln!("media player storm test still running after 300 s, aborting");
        std::process::exit(101);
    });

    let dir = tempfile::tempdir().unwrap();
    let playlist = vec![
        write_h264_clip(dir.path(), "a.mp4", 25),
        write_h264_clip(dir.path(), "b.mp4", 25),
    ];

    let (state, _files) = new_app_state();
    let flow = build_player_flow("media_player_control_storm", &playlist);
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow failed");
    state.start_flow(&flow_id).await.expect("start_flow failed");

    let player = MEDIA_PLAYER_REGISTRY
        .get(&MediaPlayerKey {
            flow_id,
            block_id: BLOCK_ID.to_string(),
        })
        .expect("the Media Player registered");

    let calls: usize = std::env::var("STROM_STORM_CALLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let seed: u64 = std::env::var("STROM_STORM_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let (progress_tx, progress_rx) = mpsc::channel::<usize>();
    let storm_player = std::sync::Arc::clone(&player);
    std::thread::spawn(move || {
        let mut rng = XorShift(seed);
        for i in 0..calls {
            let result = match rng.next() % 3 {
                0 => storm_player.goto(0),
                1 => storm_player.goto(1),
                _ => storm_player.pause(),
            };
            if let Err(e) = result {
                eprintln!("call {} returned an error: {}", i, e);
            }
            if progress_tx.send(i + 1).is_err() {
                return;
            }
        }
    });

    // A call takes milliseconds. Even one whose source never set up would
    // give up waiting for it within 10 s; one that has not returned in 30
    // has deadlocked.
    let mut done = 0;
    while done < calls {
        match progress_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(n) => done = n,
            Err(_) => panic!(
                "a Media Player control call did not return after {} of {} calls \
                 (seed {:#x}): the internal pipeline deadlocked (issue #963)",
                done, calls, seed
            ),
        }
    }
    drop(player);

    // On its own task: a stop that blocks in a state change would otherwise
    // block the timer that is meant to catch it.
    let stopper = state.clone();
    let stop = tokio::spawn(async move { stopper.stop_flow(&flow_id).await });
    let stop = tokio::time::timeout(Duration::from_secs(20), stop).await;
    assert!(
        stop.is_ok(),
        "stop_flow did not return within 20 s after the storm (issue #963)"
    );
    stop.unwrap().unwrap().expect("stop_flow failed");
}

/// Start `flow` and count the buffers that reach its fakesink.
async fn start_counting_frames(
    state: &AppState,
    flow: Flow,
) -> (strom_types::FlowId, Arc<AtomicU64>) {
    use gstreamer::prelude::*;
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow failed");
    state.start_flow(&flow_id).await.expect("start_flow failed");

    let frames = Arc::new(AtomicU64::new(0));
    let pipelines = state.pipelines_read().await;
    let sink = pipelines
        .get(&flow_id)
        .expect("running pipeline")
        .pipeline()
        .by_name("sink")
        .expect("the flow's fakesink");
    let counter = Arc::clone(&frames);
    sink.static_pad("sink")
        .unwrap()
        .add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            gstreamer::PadProbeReturn::Ok
        });
    (flow_id, frames)
}

/// Wait until at least ten frames have arrived since `before`.
async fn wait_for_frames(frames: &AtomicU64, before: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while frames.load(Ordering::Relaxed) < before + 10 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        frames.load(Ordering::Relaxed) >= before + 10,
        "the player never fed its output"
    );
}

/// Frames that arrive in 2 s, from half a second on.
async fn frames_in_two_seconds(frames: &AtomicU64) -> u64 {
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = frames.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_secs(2)).await;
    frames.load(Ordering::Relaxed) - before
}

/// A paused player must keep feeding its output with its last frame. Pausing
/// stops the internal pipeline; without a repeat the main pipeline's video
/// appsrc gets no more buffers, and a live aggregator downstream (a vision
/// mixer input) waits for its deadline on every output frame for as long as
/// the player stays paused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_player_keeps_feeding_its_output() {
    gstreamer::init().unwrap();
    run_glib_main_loop();

    let dir = tempfile::tempdir().unwrap();
    // 20 s at 25 fps: still playing when the test pauses it.
    let playlist = vec![write_h264_clip(dir.path(), "long.mp4", 500)];

    let (state, _files) = new_app_state();
    let flow = build_player_flow("media_player_paused_feeds", &playlist);
    let (flow_id, frames) = start_counting_frames(&state, flow).await;
    let player = player_of(flow_id);
    player.play().expect("play");
    wait_for_frames(&frames, 0).await;

    player.pause().expect("pause");
    let paused = frames_in_two_seconds(&frames).await;
    drop(player);
    state.stop_flow(&flow_id).await.expect("stop_flow failed");

    // 2 s at 25 fps is 50 frames; half of that leaves room for a busy runner.
    assert!(
        paused >= 25,
        "the paused player fed {} frames in 2 s",
        paused
    );
}

/// A player that passes its file through (decode off) sends encoded frames.
/// Repeating one while paused (mostly a delta frame, restamped without DTS)
/// would break the decoder or muxer behind it, so it sends nothing, as
/// before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_passthrough_player_repeats_no_encoded_frame() {
    gstreamer::init().unwrap();
    run_glib_main_loop();

    let dir = tempfile::tempdir().unwrap();
    let playlist = vec![write_h264_clip(dir.path(), "long.mp4", 500)];

    let (state, _files) = new_app_state();
    let mut flow = build_player_flow("media_player_paused_passthrough", &playlist);
    flow.blocks[0]
        .properties
        .insert("decode".to_string(), PropertyValue::Bool(false));
    let (flow_id, frames) = start_counting_frames(&state, flow).await;
    let player = player_of(flow_id);
    player.play().expect("play");
    wait_for_frames(&frames, 0).await;

    player.pause().expect("pause");
    let paused = frames_in_two_seconds(&frames).await;
    drop(player);
    state.stop_flow(&flow_id).await.expect("stop_flow failed");

    assert_eq!(paused, 0, "the paused passthrough player repeated frames");
}

/// A file switch forgets the old file's last frame: paused on a file that
/// has shown no frame (here one without video), the player holds nothing
/// rather than the frame of the file before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_player_paused_on_a_new_file_does_not_show_the_old_one() {
    gstreamer::init().unwrap();
    run_glib_main_loop();

    let dir = tempfile::tempdir().unwrap();
    let playlist = vec![
        write_h264_clip(dir.path(), "long.mp4", 500),
        write_audio_clip(dir.path(), "tone.wav"),
    ];

    let (state, _files) = new_app_state();
    let flow = build_player_flow("media_player_paused_new_file", &playlist);
    let (flow_id, frames) = start_counting_frames(&state, flow).await;
    let player = player_of(flow_id);
    player.play().expect("play");
    wait_for_frames(&frames, 0).await;

    player.goto(1).expect("goto");
    tokio::time::sleep(Duration::from_millis(500)).await;
    player.pause().expect("pause");
    let paused = frames_in_two_seconds(&frames).await;
    drop(player);
    state.stop_flow(&flow_id).await.expect("stop_flow failed");

    assert_eq!(paused, 0, "the paused player showed the old file's frame");
}

/// Going to another file while paused plays it, so the output keeps being
/// fed, now by the new file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_player_switching_files_keeps_feeding_its_output() {
    gstreamer::init().unwrap();
    run_glib_main_loop();

    let dir = tempfile::tempdir().unwrap();
    let playlist = vec![
        write_h264_clip(dir.path(), "a.mp4", 500),
        write_h264_clip(dir.path(), "b.mp4", 500),
    ];

    let (state, _files) = new_app_state();
    let flow = build_player_flow("media_player_paused_switch", &playlist);
    let (flow_id, frames) = start_counting_frames(&state, flow).await;
    let player = player_of(flow_id);
    player.play().expect("play");
    wait_for_frames(&frames, 0).await;
    player.pause().expect("pause");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut fed = Vec::new();
    for step in ["next", "previous", "goto"] {
        match step {
            "next" => player.next(),
            "previous" => player.previous(),
            _ => player.goto(1),
        }
        .expect(step);
        fed.push((step, frames_in_two_seconds(&frames).await));
        player.pause().expect("pause");
    }
    drop(player);
    state.stop_flow(&flow_id).await.expect("stop_flow failed");

    for (step, frames) in fed {
        assert!(
            frames >= 25,
            "{} while paused: the player fed {} frames in 2 s",
            step,
            frames
        );
    }
}

fn player_of(
    flow_id: strom_types::FlowId,
) -> Arc<strom::blocks::builtin::mediaplayer::MediaPlayerState> {
    MEDIA_PLAYER_REGISTRY
        .get(&MediaPlayerKey {
            flow_id,
            block_id: BLOCK_ID.to_string(),
        })
        .expect("the Media Player registered")
}
