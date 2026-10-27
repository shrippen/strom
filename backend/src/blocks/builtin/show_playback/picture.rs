//! One picture of a Show Playback block: its branch from the file to a
//! pad of its stage's compositor, and what can be done with it.
//!
//! ```text
//! uridecodebin -> videoconvert [-> aspectratiocrop] [-> imagefreeze]
//!     -> gate (identity) -> stage_mixer_S pad (zorder, alpha, size)
//! ```
//!
//! The compositor scales: its pad is as large as the stage, with the
//! aspect kept (`fit`) or not (`stretch`, and `fill` after a crop to the
//! stage's aspect). A still image is repeated by `imagefreeze`; a video
//! holds its last frame (`repeat-after-eos`) or leaves at its end.
//!
//! Like a sound, a preloaded picture waits at the gate with its first
//! frame until `play`, which shifts it to the stage's running time.

use super::repeat::{self, Repeats, Seam};
use super::sound::{program, Pause, MIN_RAMP_MS};
use super::ShowPlayback;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_controller::prelude::*;
use gstreamer_controller::{DirectControlBinding, InterpolationControlSource, InterpolationMode};
use std::sync::{Arc, Mutex};
use strom_types::show_playback::{
    AddPictureRequest, FadeEnd, PictureEnd, PictureFadeRequest, PictureFit, SoundInfo, SoundStart,
    SoundState, StageLevelRequest, FADE_MAX_MS, MAX_LAYER, MAX_PICTURES,
};
use tracing::{debug, info, warn};

/// One picture and its branch.
pub struct Picture {
    pub id: String,
    play_on_ready: std::sync::atomic::AtomicBool,
    state: Mutex<(SoundState, Option<String>)>,
    elements: Mutex<Vec<glib::WeakRef<gst::Element>>>,
    /// Its stage's compositor and the pad it feeds.
    mixer: glib::WeakRef<gst::Element>,
    mixer_pad: glib::WeakRef<gst::Pad>,
    /// Holds a preloaded picture; its src pad is shifted on `play`.
    gate: glib::WeakRef<gst::Element>,
    gate_probe: Mutex<Option<gst::PadProbeId>>,
    opacity: Mutex<Opacity>,
    repeats: Repeats,
    /// Set while paused.
    pause: Mutex<Option<Pause>>,
}

/// The pad's alpha and the control source its fades program.
struct Opacity {
    value: f64,
    source: InterpolationControlSource,
}

impl Picture {
    fn set_state(&self, state: SoundState, error: Option<String>) {
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = (state, error);
    }

    fn state(&self) -> (SoundState, Option<String>) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn keep(&self, element: &gst::Element) {
        self.elements
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(element.downgrade());
    }
}

/// Show `request`'s picture, replacing one of the same id.
pub fn add(block: &Arc<ShowPlayback>, request: &AddPictureRequest) -> Result<(), String> {
    let stage = block
        .stages
        .get(request.stage)
        .ok_or_else(|| format!("stage {} does not exist", request.stage))?
        .clone();
    if request.layer > MAX_LAYER {
        return Err(format!("layers are 0 to {}", MAX_LAYER));
    }
    if let Some(old) = take(block, &request.id) {
        teardown(block, &old);
    }
    if block
        .pictures
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .len()
        >= MAX_PICTURES
    {
        return Err(format!("{} pictures show already", MAX_PICTURES));
    }
    let bin = block.pipeline().ok_or("the flow is not running")?;
    let mixer = block.compositors[request.stage]
        .upgrade()
        .ok_or("the stage is gone")?;
    let n = block
        .next_branch
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = |part: &str| format!("{}:pic{}_{}", block.block_id, n, part);
    let make = |factory: &str, part: &str| {
        gst::ElementFactory::make(factory)
            .name(name(part))
            .build()
            .map_err(|e| format!("{}: {}", factory, e))
    };

    let uri = crate::blocks::builtin::mediaplayer::normalize_uri(&request.uri, &block.media_path);
    let decoder = make("uridecodebin", "dec")?;
    decoder.set_property("uri", &uri);
    let gate = make("identity", "gate")?;
    bin.add_many([&decoder, &gate]).map_err(|e| e.to_string())?;

    // Pictures sit above the dimmer's black only by zorder: the dimmer is
    // always on top (`STAGE_DIMMER_ZORDER`).
    let pad = mixer
        .request_pad_simple("sink_%u")
        .ok_or("the stage gave no pad")?;
    pad.set_property("zorder", request.layer);
    pad.set_property("width", stage.width as i32);
    pad.set_property("height", stage.height as i32);
    pad.set_property_from_str(
        "sizing-policy",
        match request.fit {
            PictureFit::Fit => "keep-aspect-ratio",
            PictureFit::Fill | PictureFit::Stretch => "none",
        },
    );
    pad.set_property("repeat-after-eos", request.end == PictureEnd::Hold);
    let opacity = request.opacity.clamp(0.0, 1.0);
    let source = InterpolationControlSource::new();
    source.set_mode(InterpolationMode::Linear);
    let binding = DirectControlBinding::new_absolute(&pad, "alpha", &source);
    pad.add_control_binding(&binding)
        .map_err(|e| e.to_string())?;
    source.set(gst::ClockTime::ZERO, opacity);
    let gate_src = gate.static_pad("src").ok_or("gate has no src pad")?;
    gate_src.link(&pad).map_err(|e| format!("{:?}", e))?;

    let picture = Arc::new(Picture {
        id: request.id.clone(),
        play_on_ready: std::sync::atomic::AtomicBool::new(request.start == SoundStart::Play),
        state: Mutex::new((SoundState::Loading, None)),
        elements: Mutex::new(Vec::new()),
        mixer: mixer.downgrade(),
        mixer_pad: pad.downgrade(),
        gate: gate.downgrade(),
        gate_probe: Mutex::new(None),
        opacity: Mutex::new(Opacity {
            value: opacity,
            source,
        }),
        repeats: Repeats::new(request.repeat),
        pause: Mutex::new(None),
    });
    picture.keep(&decoder);
    picture.keep(&gate);

    // The first frame waits here until `play`.
    let gated = (Arc::downgrade(block), Arc::downgrade(&picture));
    let probe = gate_src.add_probe(
        gst::PadProbeType::BLOCK | gst::PadProbeType::BUFFER,
        move |_pad, _info| {
            let (Some(block), Some(picture)) = (gated.0.upgrade(), gated.1.upgrade()) else {
                return gst::PadProbeReturn::Remove;
            };
            if picture.repeats.first_buffer() {
                // A repeated video plays from segment seeks (`repeat`);
                // its first frame waits here until the first seek's flush.
                seek_again(&block, &picture, true);
                return gst::PadProbeReturn::Ok;
            }
            if picture.state().0 == SoundState::Loading {
                become_ready(block, picture);
            }
            gst::PadProbeReturn::Ok
        },
    );
    *picture.gate_probe.lock().unwrap_or_else(|p| p.into_inner()) = probe;

    // A video's seams, and its end: it holds its last frame, or leaves
    // the stage.
    let leaves = request.end == PictureEnd::Stop;
    let ending = (Arc::downgrade(block), Arc::downgrade(&picture));
    gate_src.add_probe(
        gst::PadProbeType::EVENT_DOWNSTREAM | gst::PadProbeType::EVENT_FLUSH,
        move |_pad, info| {
            let Some(gst::PadProbeData::Event(event)) = &info.data else {
                return gst::PadProbeReturn::Ok;
            };
            let (Some(block), Some(picture)) = (ending.0.upgrade(), ending.1.upgrade()) else {
                return gst::PadProbeReturn::Ok;
            };
            match picture.repeats.event(event.type_()) {
                Seam::Swallow => {}
                Seam::Next => seek_again(&block, &picture, false),
                Seam::Pass => {
                    if leaves && event.type_() == gst::EventType::Eos {
                        end(block, picture);
                    }
                    // On to the compositor: the stage's live black keeps
                    // it going.
                    return gst::PadProbeReturn::Ok;
                }
            }
            // Handled, not Drop: dropping an event in a push probe makes
            // GStreamer 1.24 unref a NULL event.
            info.data = None;
            gst::PadProbeReturn::Handled
        },
    );

    // What to put between decoder and gate is known from the decoded caps:
    // a still image needs repeating, `fill` a crop to the stage's aspect.
    let wiring = (
        Arc::downgrade(block),
        Arc::downgrade(&picture),
        gate.downgrade(),
    );
    let fit = request.fit;
    let aspect = gst::Fraction::new(stage.width as i32, stage.height as i32);
    decoder.connect_pad_added(move |_decoder, pad| {
        let (Some(block), Some(picture), Some(gate)) =
            (wiring.0.upgrade(), wiring.1.upgrade(), wiring.2.upgrade())
        else {
            return;
        };
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        let Some(structure) = caps.structure(0) else {
            return;
        };
        let Some(bin) = block.pipeline() else {
            return;
        };
        if !structure.name().starts_with("video/") {
            // Sound of a video is not played here (zones take sounds).
            if let Err(e) = discard(&bin, &picture, pad) {
                warn!("Show Playback {}: {}", block.block_id, e);
            }
            return;
        }
        let Some(gate_sink) = gate.static_pad("sink") else {
            return;
        };
        if gate_sink.is_linked() {
            // A second video stream: one shows.
            return;
        }
        let still = structure
            .get::<gst::Fraction>("framerate")
            .is_ok_and(|rate| rate.numer() == 0);
        if still {
            picture.repeats.once();
        }
        if let Err(e) = wire(&bin, &picture, pad, &gate_sink, still, fit, aspect) {
            warn!(
                "Show Playback {}: {} cannot reach its stage: {}",
                block.block_id, picture.id, e
            );
            picture.set_state(SoundState::Failed, Some(e));
        }
    });

    block
        .pictures
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(request.id.clone(), picture.clone());
    for element in [&gate, &decoder] {
        element
            .sync_state_with_parent()
            .map_err(|e| e.to_string())?;
    }
    info!(
        "Show Playback {}: added picture {} ({}) on stage {}",
        block.block_id, request.id, uri, stage.name
    );
    Ok(())
}

/// Decoder pad -> convert [-> crop] [-> freeze] -> gate.
fn wire(
    bin: &gst::Bin,
    picture: &Picture,
    pad: &gst::Pad,
    gate_sink: &gst::Pad,
    still: bool,
    fit: PictureFit,
    aspect: gst::Fraction,
) -> Result<(), String> {
    let base = pad
        .parent_element()
        .map(|e| e.name().replace("_dec", ""))
        .unwrap_or_default();
    let make = |factory: &str, part: &str| {
        gst::ElementFactory::make(factory)
            .name(format!("{}_{}", base, part))
            .build()
            .map_err(|e| format!("{}: {}", factory, e))
    };
    let mut chain = vec![make("videoconvert", "conv")?];
    if fit == PictureFit::Fill {
        let crop = make("aspectratiocrop", "crop")?;
        crop.set_property("aspect-ratio", aspect);
        chain.push(crop);
    }
    if still {
        // Not live: its frames count from 0 like a video's, and `play`
        // shifts them to the stage's time. The stage paces it.
        chain.push(make("imagefreeze", "freeze")?);
    }
    for element in &chain {
        picture.keep(element);
    }
    bin.add_many(&chain).map_err(|e| e.to_string())?;
    gst::Element::link_many(&chain).map_err(|e| e.to_string())?;
    chain
        .last()
        .and_then(|e| e.static_pad("src"))
        .ok_or("no src pad")?
        .link(gate_sink)
        .map_err(|e| format!("{:?}", e))?;
    for element in &chain {
        element
            .sync_state_with_parent()
            .map_err(|e| e.to_string())?;
    }
    pad.link(&chain[0].static_pad("sink").ok_or("no sink pad")?)
        .map_err(|e| format!("{:?}", e))?;
    Ok(())
}

/// A stream the stage does not show, into a fakesink.
fn discard(bin: &gst::Bin, picture: &Picture, pad: &gst::Pad) -> Result<(), String> {
    let sink = gst::ElementFactory::make("fakesink")
        .property("async", false)
        .property("sync", false)
        .build()
        .map_err(|e| e.to_string())?;
    picture.keep(&sink);
    bin.add(&sink).map_err(|e| e.to_string())?;
    sink.sync_state_with_parent().map_err(|e| e.to_string())?;
    pad.link(&sink.static_pad("sink").ok_or("no sink pad")?)
        .map_err(|e| format!("{:?}", e))?;
    Ok(())
}

/// Seek `picture`'s video back to its start for its next play. A first
/// seek the file refuses leaves it playing once.
fn seek_again(block: &Arc<ShowPlayback>, picture: &Arc<Picture>, flush: bool) {
    let Some(pad) = picture.gate.upgrade().and_then(|g| g.static_pad("sink")) else {
        return;
    };
    let (block, weak) = (Arc::downgrade(block), Arc::downgrade(picture));
    repeat::seek_from_start(&pad, picture.repeats.flags(flush), move || {
        let (Some(block), Some(picture)) = (block.upgrade(), weak.upgrade()) else {
            return;
        };
        picture.repeats.refused();
        if flush {
            // Its first frame still waits at the gate.
            become_ready(block, picture);
        }
    });
}

/// The first frame is held at the gate: the picture is ready, and shows
/// at once if it should.
fn become_ready(block: Arc<ShowPlayback>, picture: Arc<Picture>) {
    picture.set_state(SoundState::Ready, None);
    debug!("Show Playback {}: {} is ready", block.block_id, picture.id);
    if picture
        .play_on_ready
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        // Not from inside the probe: removing it waits for it.
        std::thread::spawn(move || {
            if let Err(e) = release(&block, &picture) {
                warn!(
                    "Show Playback {}: {} did not start: {}",
                    block.block_id, picture.id, e
                );
            }
        });
    }
}

/// Show picture `id`: a preloaded one at once, one still loading as soon
/// as its first frame is there.
pub fn play(block: &Arc<ShowPlayback>, id: &str) -> Result<(), String> {
    let picture = find(block, id)?;
    match picture.state().0 {
        SoundState::Ready => release(block, &picture),
        SoundState::Loading => {
            picture
                .play_on_ready
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if picture.state().0 == SoundState::Ready {
                return release(block, &picture);
            }
            Ok(())
        }
        SoundState::Playing | SoundState::Paused => Ok(()),
        SoundState::Ended | SoundState::Failed => Err("the picture has ended".into()),
    }
}

/// Pause picture `id` (RUN.5): its frame stays on the stage, a video
/// holds its place.
pub fn pause(block: &Arc<ShowPlayback>, id: &str) -> Result<(), String> {
    let picture = find(block, id)?;
    match picture.state().0 {
        SoundState::Playing => {}
        SoundState::Paused => return Ok(()),
        _ => return Err("the picture does not show".into()),
    }
    let now = block.running_time().ok_or("the stage is not running")?;
    let gate = picture.gate.upgrade().ok_or("the picture is gone")?;
    let pad = gate.static_pad("src").ok_or("gate has no src pad")?;
    *picture.pause.lock().unwrap_or_else(|p| p.into_inner()) = Some(Pause::hold([pad], now));
    picture.set_state(SoundState::Paused, None);
    info!("Show Playback {}: {} paused", block.block_id, picture.id);
    Ok(())
}

/// Resume paused picture `id` where it was.
pub fn resume(block: &Arc<ShowPlayback>, id: &str) -> Result<(), String> {
    let picture = find(block, id)?;
    let Some(pause) = picture
        .pause
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
    else {
        return match picture.state().0 {
            SoundState::Playing => Ok(()),
            _ => Err("the picture is not paused".into()),
        };
    };
    let now = block.running_time().ok_or("the stage is not running")?;
    picture.set_state(SoundState::Playing, None);
    pause.end(now);
    info!("Show Playback {}: {} resumed", block.block_id, picture.id);
    Ok(())
}

/// Let a ready picture go: its start shows at the stage's running time now.
fn release(block: &ShowPlayback, picture: &Picture) -> Result<(), String> {
    let now = block.running_time().ok_or("the stage is not running")?;
    let gate = picture.gate.upgrade().ok_or("the picture is gone")?;
    let pad = gate.static_pad("src").ok_or("gate has no src pad")?;
    pad.set_offset(now.nseconds() as i64);
    picture.set_state(SoundState::Playing, None);
    if let Some(probe) = picture
        .gate_probe
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
    {
        pad.remove_probe(probe);
    }
    info!("Show Playback {}: {} shows", block.block_id, picture.id);
    Ok(())
}

/// Fade picture `id`'s opacity.
pub fn fade(
    block: &Arc<ShowPlayback>,
    id: &str,
    request: &PictureFadeRequest,
) -> Result<(), String> {
    let picture = find(block, id)?;
    let duration = request.duration_ms.min(FADE_MAX_MS);
    fade_to(block, &picture, request.opacity.clamp(0.0, 1.0), duration)?;
    if request.end == FadeEnd::Stop {
        stop_after(block, &picture, duration);
    }
    Ok(())
}

/// Take picture `id` off its stage, faded out over `fade_ms`.
pub fn stop(block: &Arc<ShowPlayback>, id: &str, fade_ms: u64) -> Result<(), String> {
    let picture = find(block, id)?;
    let fade_ms = fade_ms.min(FADE_MAX_MS);
    // A paused picture fades out too: its frame stays on the stage.
    if matches!(picture.state().0, SoundState::Playing | SoundState::Paused) && fade_ms > 0 {
        fade_to(block, &picture, 0.0, fade_ms)?;
        stop_after(block, &picture, fade_ms);
    } else {
        picture.set_state(SoundState::Ended, None);
        if let Some(picture) = take(block, id) {
            teardown(block, &picture);
        }
    }
    Ok(())
}

/// Stage `index`'s brightness: the dimmer's black on top of it.
pub fn set_stage(
    block: &Arc<ShowPlayback>,
    index: usize,
    request: &StageLevelRequest,
) -> Result<(), String> {
    let mixer = block
        .compositors
        .get(index)
        .ok_or_else(|| format!("stage {} does not exist", index))?
        .upgrade()
        .ok_or("the flow is not running")?;
    let pad = mixer
        .static_pad(super::STAGE_DIMMER_PAD)
        .ok_or("the stage has no dimmer")?;
    let now = block.running_time().ok_or("the stage is not running")?;
    let source = match pad.control_binding("alpha") {
        Some(binding) => binding
            .property::<Option<gst::ControlSource>>("control-source")
            .and_then(|cs| cs.downcast::<InterpolationControlSource>().ok())
            .ok_or("the dimmer has a foreign binding")?,
        None => {
            let source = InterpolationControlSource::new();
            source.set_mode(InterpolationMode::Linear);
            let binding = DirectControlBinding::new_absolute(&pad, "alpha", &source);
            pad.add_control_binding(&binding)
                .map_err(|e| e.to_string())?;
            source
        }
    };
    let from = pad.property::<f64>("alpha");
    let to = 1.0 - request.brightness.clamp(0.0, 1.0);
    let duration = request.duration_ms.clamp(MIN_RAMP_MS, FADE_MAX_MS);
    program(&source, from, to, now, duration);
    Ok(())
}

/// The pictures, sorted by id.
pub fn infos(block: &ShowPlayback) -> Vec<SoundInfo> {
    let pictures = block.pictures.lock().unwrap_or_else(|p| p.into_inner());
    let mut list: Vec<SoundInfo> = pictures
        .values()
        .map(|p| {
            let (state, error) = p.state();
            SoundInfo {
                id: p.id.clone(),
                state,
                error,
            }
        })
        .collect();
    list.sort_by(|a, b| a.id.cmp(&b.id));
    list
}

fn fade_to(
    block: &ShowPlayback,
    picture: &Picture,
    target: f64,
    duration_ms: u64,
) -> Result<(), String> {
    let now = block.running_time().ok_or("the stage is not running")?;
    let mut opacity = picture.opacity.lock().unwrap_or_else(|p| p.into_inner());
    let from = opacity.value;
    opacity.value = target;
    // The compositor reads its pads' values at the stage's running time.
    program(
        &opacity.source,
        from,
        target,
        now,
        duration_ms.max(MIN_RAMP_MS),
    );
    Ok(())
}

fn find(block: &ShowPlayback, id: &str) -> Result<Arc<Picture>, String> {
    block
        .pictures
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(id)
        .cloned()
        .ok_or_else(|| format!("no picture {}", id))
}

fn take(block: &ShowPlayback, id: &str) -> Option<Arc<Picture>> {
    block
        .pictures
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(id)
}

/// The picture is over: take its branch down.
fn end(block: Arc<ShowPlayback>, picture: Arc<Picture>) {
    picture.set_state(SoundState::Ended, None);
    std::thread::spawn(move || {
        if take(&block, &picture.id).is_some_and(|p| Arc::ptr_eq(&p, &picture)) {
            teardown(&block, &picture);
        }
    });
}

fn stop_after(block: &Arc<ShowPlayback>, picture: &Arc<Picture>, delay_ms: u64) {
    let (block, picture) = (Arc::downgrade(block), Arc::downgrade(picture));
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms + MIN_RAMP_MS));
        if let (Some(block), Some(picture)) = (block.upgrade(), picture.upgrade()) {
            picture.set_state(SoundState::Ended, None);
            if take(&block, &picture.id).is_some_and(|p| Arc::ptr_eq(&p, &picture)) {
                teardown(&block, &picture);
            }
        }
    });
}

/// Take the branch out of the pipeline and give the stage pad back.
fn teardown(block: &ShowPlayback, picture: &Picture) {
    if let Some(pause) = picture
        .pause
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
    {
        pause.drop_probes();
    }
    if let (Some(mixer), Some(pad)) = (picture.mixer.upgrade(), picture.mixer_pad.upgrade()) {
        if let Some(peer) = pad.peer() {
            let _ = peer.unlink(&pad);
        }
        mixer.release_request_pad(&pad);
    }
    let elements: Vec<gst::Element> = picture
        .elements
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .drain(..)
        .filter_map(|e| e.upgrade())
        .collect();
    for element in &elements {
        let _ = element.set_state(gst::State::Null);
    }
    if let Some(bin) = block.pipeline() {
        for element in &elements {
            let _ = bin.remove(element);
        }
    }
    debug!(
        "Show Playback {}: {} taken down",
        block.block_id, picture.id
    );
}
