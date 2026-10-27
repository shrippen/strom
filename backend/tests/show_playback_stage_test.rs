//! Show Playback stages: pictures preloaded, shown, faded, dimmed and
//! stopped in a running pipeline built by the real builder, and seen at
//! the stage output.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom::blocks::builtin::show_playback::{self, picture, ShowPlaybackBuilder, ShowPlaybackKey};
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::element::ElementPadRef;
use strom_types::show_playback::{
    AddPictureRequest, PictureEnd, PictureFadeRequest, PictureFit, SoundRepeat, SoundStart,
    SoundState, StageLevelRequest,
};
use strom_types::PropertyValue;

const W: u32 = 160;
const H: u32 = 90;

struct Rig {
    pipeline: gst::Pipeline,
    key: ShowPlaybackKey,
    /// RGBA of the stage's latest frame.
    frame: Arc<Mutex<Vec<u8>>>,
    dir: tempfile::TempDir,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Write `pipeline`'s output to `file` in the rig's folder.
fn write(dir: &std::path::Path, file: &str, pipeline: &str) -> String {
    let path = dir.join(file);
    let writer = gst::parse::launch(&format!("{pipeline} ! filesink name=out")).unwrap();
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
        .expect("the file is written");
    writer.set_state(gst::State::Null).unwrap();
    path.to_string_lossy().into_owned()
}

/// A red still image, square, smaller than the stage.
fn red_image(dir: &std::path::Path) -> String {
    write(
        dir,
        "red.png",
        "videotestsrc pattern=red num-buffers=1 ! video/x-raw,width=64,height=64 ! pngenc",
    )
}

/// A green video of `frames` frames at 30 fps.
fn green_video(dir: &std::path::Path, frames: u32) -> String {
    write(
        dir,
        "green.avi",
        &format!(
            "videotestsrc pattern=green num-buffers={frames} \
             ! video/x-raw,width=64,height=36,framerate=30/1 ! jpegenc ! avimux"
        ),
    )
}

/// One stage into an appsink keeping its latest frame.
fn rig() -> Rig {
    gst::init().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let flow_id = uuid::Uuid::new_v4();
    let props: HashMap<String, PropertyValue> = [
        (
            "stages".to_string(),
            PropertyValue::String(format!(r#"[{{"name":"Back","width":{W},"height":{H}}}]"#)),
        ),
        ("framerate".to_string(), PropertyValue::String("30".into())),
        (
            "_flow_id".to_string(),
            PropertyValue::String(flow_id.to_string()),
        ),
    ]
    .into();
    let built = ShowPlaybackBuilder
        .build(
            "show",
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
    // The zone is there too; it goes nowhere here.
    let nowhere = gst::ElementFactory::make("fakesink").build().unwrap();
    pipeline.add(&nowhere).unwrap();
    by_id["show:zone_out_0"].link(&nowhere).unwrap();

    let frame = Arc::new(Mutex::new(Vec::new()));
    let seen = frame.clone();
    let sink = gst_app::AppSink::builder()
        .caps(
            &gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .build(),
        )
        .sync(true)
        .build();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                *seen.lock().unwrap() = map.to_vec();
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    let convert = gst::ElementFactory::make("videoconvert").build().unwrap();
    pipeline.add_many([&convert, sink.upcast_ref()]).unwrap();
    by_id["show:stage_out_0"].link(&convert).unwrap();
    convert.link(&sink).unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let _ = pipeline.state(gst::ClockTime::from_seconds(5));
    Rig {
        pipeline,
        key: ShowPlaybackKey {
            flow_id,
            block_id: "show".into(),
        },
        frame,
        dir,
    }
}

/// RGB at (`x`, `y`) of the latest frame.
fn pixel(rig: &Rig, x: u32, y: u32) -> [u8; 3] {
    let frame = rig.frame.lock().unwrap();
    let at = ((y * W + x) * 4) as usize;
    match frame.get(at..at + 3) {
        Some(p) => [p[0], p[1], p[2]],
        None => [0, 0, 0],
    }
}

fn center(rig: &Rig) -> [u8; 3] {
    pixel(rig, W / 2, H / 2)
}

fn is_black(p: [u8; 3]) -> bool {
    p.iter().all(|c| *c < 24)
}

fn is_red(p: [u8; 3]) -> bool {
    p[0] > 200 && p[1] < 40 && p[2] < 40
}

fn is_green(p: [u8; 3]) -> bool {
    p[1] > 200 && p[0] < 40 && p[2] < 40
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
    picture::infos(&block)
        .into_iter()
        .find(|p| p.id == id)
        .map(|p| p.state)
}

fn request(id: &str, uri: String) -> AddPictureRequest {
    AddPictureRequest {
        id: id.into(),
        uri,
        stage: 0,
        layer: 1,
        opacity: 1.0,
        fit: PictureFit::Fit,
        end: PictureEnd::Hold,
        start: SoundStart::Preload,
        repeat: SoundRepeat::Once,
    }
}

/// VIS.8: black without pictures. A preloaded picture shows within a
/// frame or two of `play`, fitted (bars at the sides of a square picture).
#[test]
fn a_preloaded_picture_shows_at_once_fitted_on_black() {
    let rig = rig();
    let block = show_playback::running(&rig.key).expect("the block runs");
    wait_for("the stage runs", || !rig.frame.lock().unwrap().is_empty());
    assert!(is_black(center(&rig)), "not black: {:?}", center(&rig));

    picture::add(&block, &request("back", red_image(rig.dir.path()))).unwrap();
    wait_for("the picture is ready", || {
        state_of(&rig, "back") == Some(SoundState::Ready)
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(is_black(center(&rig)), "shown before play");

    let go = Instant::now();
    picture::play(&block, "back").unwrap();
    wait_for("the stage shows red", || is_red(center(&rig)));
    let latency = go.elapsed();
    println!("play to red: {latency:?}");
    assert!(
        latency < Duration::from_millis(200),
        "seen after {latency:?}"
    );
    assert!(
        is_black(pixel(&rig, 4, H / 2)),
        "a square picture fitted on a wide stage leaves bars: {:?}",
        pixel(&rig, 4, H / 2)
    );
}

/// `stretch` and `fill` cover the stage.
#[test]
fn stretched_and_filled_pictures_cover_the_stage() {
    for fit in [PictureFit::Stretch, PictureFit::Fill] {
        let rig = rig();
        let block = show_playback::running(&rig.key).unwrap();
        let mut req = request("back", red_image(rig.dir.path()));
        req.fit = fit;
        req.start = SoundStart::Play;
        picture::add(&block, &req).unwrap();
        wait_for("the stage shows red", || is_red(center(&rig)));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            is_red(pixel(&rig, 4, H / 2)),
            "{fit:?}: the side is not covered"
        );
    }
}

/// VIS.4: a fade to clear with stop takes the picture off; VIS.7: the
/// dimmer darkens the stage whatever shows.
#[test]
fn fades_and_the_dimmer() {
    let rig = rig();
    let block = show_playback::running(&rig.key).unwrap();
    let mut req = request("back", red_image(rig.dir.path()));
    req.start = SoundStart::Play;
    picture::add(&block, &req).unwrap();
    wait_for("the stage shows red", || is_red(center(&rig)));

    picture::set_stage(
        &block,
        0,
        &StageLevelRequest {
            brightness: 0.0,
            duration_ms: 100,
        },
    )
    .unwrap();
    wait_for("the dimmer is down", || is_black(center(&rig)));
    picture::set_stage(
        &block,
        0,
        &StageLevelRequest {
            brightness: 1.0,
            duration_ms: 0,
        },
    )
    .unwrap();
    wait_for("the dimmer is up", || is_red(center(&rig)));

    picture::fade(
        &block,
        "back",
        &PictureFadeRequest {
            opacity: 0.0,
            duration_ms: 1000,
            end: strom_types::show_playback::FadeEnd::Stop,
        },
    )
    .unwrap();
    // Halfway, and the stage's latency later: the output lags the clock.
    std::thread::sleep(Duration::from_millis(600));
    let half = center(&rig);
    assert!(
        !is_red(half) && !is_black(half),
        "halfway through the fade: {half:?}"
    );
    wait_for("the picture is gone", || state_of(&rig, "back").is_none());
    assert!(is_black(center(&rig)));
}

/// VIS.2: a video holds its last frame, or leaves the stage at its end.
#[test]
fn a_video_holds_or_leaves_at_its_end() {
    for end in [PictureEnd::Hold, PictureEnd::Stop] {
        let rig = rig();
        let block = show_playback::running(&rig.key).unwrap();
        let mut req = request("clip", green_video(rig.dir.path(), 15));
        req.end = end;
        req.start = SoundStart::Play;
        picture::add(&block, &req).unwrap();
        wait_for("the stage shows green", || is_green(center(&rig)));
        // Half a second of video, then a second more.
        std::thread::sleep(Duration::from_millis(1500));
        match end {
            PictureEnd::Hold => {
                assert!(is_green(center(&rig)), "the last frame went");
                assert_eq!(state_of(&rig, "clip"), Some(SoundState::Playing));
            }
            PictureEnd::Stop => {
                assert!(is_black(center(&rig)), "still shown: {:?}", center(&rig));
                assert_eq!(state_of(&rig, "clip"), None);
            }
        }
    }
}

/// VIS.2: a video played 3 times shows for 3 of its lengths, then leaves.
#[test]
fn a_repeated_video_plays_its_times_then_leaves() {
    let rig = rig();
    let block = show_playback::running(&rig.key).unwrap();
    let mut req = request("clip", green_video(rig.dir.path(), 15));
    req.end = PictureEnd::Stop;
    req.start = SoundStart::Play;
    req.repeat = SoundRepeat::Times(3);
    picture::add(&block, &req).unwrap();
    wait_for("the stage shows green", || is_green(center(&rig)));
    let shown = Instant::now();
    // Half a second of video: once alone it would be gone by now.
    std::thread::sleep(Duration::from_millis(1100));
    assert!(is_green(center(&rig)), "gone after its first play");
    assert_eq!(state_of(&rig, "clip"), Some(SoundState::Playing));
    wait_for("the clip is gone", || state_of(&rig, "clip").is_none());
    let played = shown.elapsed();
    assert!(
        played > Duration::from_millis(1300) && played < Duration::from_millis(2100),
        "3 plays of 0.5 s took {played:?}"
    );
}

/// RUN.5: a paused video holds its frame on the stage and plays on from
/// there when resumed: half a second of video paused for a second leaves
/// after about one and a half.
#[test]
fn a_paused_video_holds_its_frame_and_resumes() {
    let rig = rig();
    let block = show_playback::running(&rig.key).unwrap();
    let mut req = request("clip", green_video(rig.dir.path(), 15));
    req.end = PictureEnd::Stop;
    req.start = SoundStart::Play;
    picture::add(&block, &req).unwrap();
    wait_for("the stage shows green", || is_green(center(&rig)));
    let shown = Instant::now();
    std::thread::sleep(Duration::from_millis(200));
    picture::pause(&block, "clip").unwrap();
    assert_eq!(state_of(&rig, "clip"), Some(SoundState::Paused));
    std::thread::sleep(Duration::from_millis(1000));
    assert!(is_green(center(&rig)), "the paused frame went");
    assert_eq!(state_of(&rig, "clip"), Some(SoundState::Paused));

    picture::resume(&block, "clip").unwrap();
    assert_eq!(state_of(&rig, "clip"), Some(SoundState::Playing));
    wait_for("the clip is gone", || state_of(&rig, "clip").is_none());
    let played = shown.elapsed();
    assert!(
        played > Duration::from_millis(1300),
        "0.5 s of video paused for 1 s was gone after {played:?}"
    );
}

/// A pipeline with pictures on its stage is freed when it stops: the
/// branches hold no strong reference to it (CLAUDE.md, closures).
#[test]
fn a_stopped_stage_frees_its_pipeline() {
    let rig = rig();
    let block = show_playback::running(&rig.key).unwrap();
    let mut still = request("back", red_image(rig.dir.path()));
    still.start = SoundStart::Play;
    picture::add(&block, &still).unwrap();
    let mut clip = request("clip", green_video(rig.dir.path(), 60));
    clip.start = SoundStart::Play;
    clip.layer = 2;
    picture::add(&block, &clip).unwrap();
    wait_for("the stage shows green", || is_green(center(&rig)));
    drop(block);

    let pipeline = rig.pipeline.downgrade();
    drop(rig);
    wait_for("the pipeline is freed", || pipeline.upgrade().is_none());
}
