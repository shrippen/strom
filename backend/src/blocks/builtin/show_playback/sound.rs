//! One sound of a Show Playback block: its branch in the running pipeline, from
//! the file to the zones it is routed to, and what can be done with it.
//!
//! Every closure on the branch's pads holds the [`Sound`] and the
//! [`ShowPlayback`], which hold weak element references only, so no pad keeps
//! the pipeline alive.

use super::repeat::{self, Repeats, Seam};
use super::{zone_caps, ShowPlayback, ZONE_RATE};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_controller::prelude::*;
use gstreamer_controller::{DirectControlBinding, InterpolationControlSource, InterpolationMode};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use strom_types::show_playback::{
    gain_of_db, s_curve, AddSoundRequest, FadeEnd, ShowPlaybackState, SoundFadeRequest, SoundInfo,
    SoundRoute, SoundStart, SoundState, ZoneLevelRequest, FADE_MAX_MS, MAX_SOUNDS,
};
use tracing::{debug, info, warn};

/// Keyframes of one fade: enough for a smooth S-curve, few enough to be cheap.
const FADE_KEYFRAMES: u32 = 32;

/// Shortest ramp for a level change, so a jump does not click.
pub(super) const MIN_RAMP_MS: u64 = 20;

/// One sound and its branch.
pub struct Sound {
    pub id: String,
    /// Play as soon as the first buffer is there (start `play`, or `play`
    /// while loading).
    play_on_ready: std::sync::atomic::AtomicBool,
    state: Mutex<(SoundState, Option<String>)>,
    /// Every element of the branch, to take it down again.
    elements: Mutex<Vec<glib::WeakRef<gst::Element>>>,
    /// The zone mixer pads it feeds, with their mixer and the control
    /// source of the pad's volume (the cue level).
    mixer_pads: Mutex<Vec<MixerPad>>,
    /// Its src pad holds a preloaded sound; seeks go up from its sink.
    volume: glib::WeakRef<gst::Element>,
    level: Mutex<Level>,
    repeats: Repeats,
    gate: Mutex<Option<gst::PadProbeId>>,
    /// Running time the file's start plays at, once released.
    started_at: Mutex<Option<gst::ClockTime>>,
    /// Zone branches whose end has not reached their mixer yet.
    open_zones: std::sync::atomic::AtomicUsize,
    /// Set while paused.
    pause: Mutex<Option<Pause>>,
}

/// A paused sound or picture: since when, and the blocking probes that
/// hold its branches.
pub(super) struct Pause {
    since: gst::ClockTime,
    probes: Vec<(glib::WeakRef<gst::Pad>, gst::PadProbeId)>,
}

impl Pause {
    /// Hold every buffer on `pads` from the running time `now` on.
    pub(super) fn hold(pads: impl IntoIterator<Item = gst::Pad>, now: gst::ClockTime) -> Self {
        let probes = pads
            .into_iter()
            .filter_map(|pad| {
                let probe = pad.add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_pad, _info| {
                    gst::PadProbeReturn::Ok
                })?;
                Some((pad.downgrade(), probe))
            })
            .collect();
        Self { since: now, probes }
    }

    /// Let the held buffers go, shifted by the time the pause took, so
    /// they play from `now` on where they stopped.
    pub(super) fn end(self, now: gst::ClockTime) {
        let held = now.saturating_sub(self.since).nseconds() as i64;
        for (pad, probe) in self.probes {
            if let Some(pad) = pad.upgrade() {
                pad.set_offset(pad.offset() + held);
                pad.remove_probe(probe);
            }
        }
    }

    /// Let go without a shift (the branch is taken down).
    pub(super) fn drop_probes(self) {
        for (pad, probe) in self.probes {
            if let Some(pad) = pad.upgrade() {
                pad.remove_probe(probe);
            }
        }
    }
}

/// The cue level: where it goes, and the ramp that takes it there, for
/// mixer pads that come later.
struct Level {
    gain: f64,
    ramp: Option<Ramp>,
}

/// A level ramp in the zones' running time.
#[derive(Clone, Copy)]
struct Ramp {
    from: f64,
    to: f64,
    start: gst::ClockTime,
    duration_ms: u64,
}

/// A zone mixer pad a sound feeds.
struct MixerPad {
    mixer: glib::WeakRef<gst::Element>,
    pad: glib::WeakRef<gst::Pad>,
    level: InterpolationControlSource,
}

impl Sound {
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

/// Add `request`'s sound to `block`, replacing one of the same id.
pub fn add(block: &Arc<ShowPlayback>, request: &AddSoundRequest) -> Result<(), String> {
    for route in &request.routes {
        let zone = block
            .zones
            .get(route.zone)
            .ok_or_else(|| format!("zone {} does not exist", route.zone))?;
        if route.zone_channel >= zone.channels {
            return Err(format!(
                "zone {} has {} channel(s), not {}",
                zone.name,
                zone.channels,
                route.zone_channel + 1
            ));
        }
    }
    if let Some(old) = take(block, &request.id) {
        teardown(block, &old);
    }
    if block.sounds.lock().unwrap_or_else(|p| p.into_inner()).len() >= MAX_SOUNDS {
        return Err(format!("{} sounds play already", MAX_SOUNDS));
    }
    let bin = block.pipeline().ok_or("the flow is not running")?;
    let n = block
        .next_branch
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = |part: &str| format!("{}:snd{}_{}", block.block_id, n, part);
    let make = |factory: &str, part: &str| {
        gst::ElementFactory::make(factory)
            .name(name(part))
            .build()
            .map_err(|e| format!("{}: {}", factory, e))
    };

    let uri = crate::blocks::builtin::mediaplayer::normalize_uri(&request.uri, &block.media_path);
    let decoder = make("uridecodebin", "dec")?;
    decoder.set_property("uri", &uri);
    let convert = make("audioconvert", "conv")?;
    let resample = make("audioresample", "res")?;
    let caps = make("capsfilter", "caps")?;
    caps.set_property(
        "caps",
        gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("layout", "interleaved")
            .field("rate", ZONE_RATE)
            .build(),
    );
    let volume = make("volume", "vol")?;
    let tee = make("tee", "tee")?;
    tee.set_property("allow-not-linked", true);

    let sound = Arc::new(Sound {
        id: request.id.clone(),
        play_on_ready: std::sync::atomic::AtomicBool::new(request.start == SoundStart::Play),
        state: Mutex::new((SoundState::Loading, None)),
        elements: Mutex::new(Vec::new()),
        mixer_pads: Mutex::new(Vec::new()),
        volume: volume.downgrade(),
        level: Mutex::new(Level {
            gain: gain_of_db(request.gain_db),
            ramp: None,
        }),
        repeats: Repeats::new(request.repeat),
        gate: Mutex::new(None),
        started_at: Mutex::new(None),
        open_zones: std::sync::atomic::AtomicUsize::new(0),
        pause: Mutex::new(None),
    });
    let chain = [&decoder, &convert, &resample, &caps, &volume, &tee];
    for element in chain {
        sound.keep(element);
    }
    bin.add_many(chain).map_err(|e| e.to_string())?;
    gst::Element::link_many([&convert, &resample, &caps, &volume, &tee])
        .map_err(|e| e.to_string())?;

    // The first buffer waits here until `play`.
    let gate_pad = volume.static_pad("src").ok_or("volume has no src pad")?;
    let gated = (Arc::downgrade(block), Arc::downgrade(&sound));
    let gate = gate_pad.add_probe(
        gst::PadProbeType::BLOCK | gst::PadProbeType::BUFFER,
        move |_pad, _info| {
            let (Some(block), Some(sound)) = (gated.0.upgrade(), gated.1.upgrade()) else {
                return gst::PadProbeReturn::Remove;
            };
            if sound.repeats.first_buffer() {
                // A repeated sound plays from segment seeks (`repeat`); the
                // first buffer waits here, blocking the decoder, until the
                // first seek's flush.
                seek_again(&block, &sound, true);
                return gst::PadProbeReturn::Ok;
            }
            if sound.state().0 == SoundState::Loading {
                become_ready(block, sound);
            }
            gst::PadProbeReturn::Ok
        },
    );
    *sound.gate.lock().unwrap_or_else(|p| p.into_inner()) = gate;

    // The decoder is far ahead of the zones: a sound is over when its end
    // reaches the zone mixers (`feed_zones`), here only if it feeds none.
    // At a seam the next play is asked for; the first seek's flush stays
    // here (nothing downstream has data yet).
    let ending = (Arc::downgrade(block), Arc::downgrade(&sound));
    gate_pad.add_probe(
        gst::PadProbeType::EVENT_DOWNSTREAM | gst::PadProbeType::EVENT_FLUSH,
        move |_pad, info| {
            let Some(gst::PadProbeData::Event(event)) = &info.data else {
                return gst::PadProbeReturn::Ok;
            };
            let (Some(block), Some(sound)) = (ending.0.upgrade(), ending.1.upgrade()) else {
                return gst::PadProbeReturn::Ok;
            };
            match sound.repeats.event(event.type_()) {
                Seam::Swallow => {}
                Seam::Next => seek_again(&block, &sound, false),
                Seam::Pass => {
                    let eos = event.type_() == gst::EventType::Eos;
                    if eos && sound.open_zones.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                        end(block, sound);
                    }
                    return gst::PadProbeReturn::Ok;
                }
            }
            // Handled, not Drop: dropping an event in a push probe makes
            // GStreamer 1.24 unref a NULL event.
            info.data = None;
            gst::PadProbeReturn::Handled
        },
    );

    // The file's channels are known once the decoder has a pad: then the
    // zones can be fed.
    let routes = request.routes.clone();
    let wiring = (
        Arc::downgrade(block),
        Arc::downgrade(&sound),
        convert.downgrade(),
        tee.downgrade(),
    );
    decoder.connect_pad_added(move |_decoder, pad| {
        let (Some(block), Some(sound), Some(convert), Some(tee)) = (
            wiring.0.upgrade(),
            wiring.1.upgrade(),
            wiring.2.upgrade(),
            wiring.3.upgrade(),
        ) else {
            return;
        };
        let Some(caps) = pad.current_caps().or_else(|| Some(pad.query_caps(None))) else {
            return;
        };
        let Some(structure) = caps.structure(0) else {
            return;
        };
        if !structure.name().starts_with("audio/") {
            return;
        }
        let Some(sink) = convert.static_pad("sink") else {
            return;
        };
        if sink.is_linked() {
            // A second audio stream: one plays.
            return;
        }
        let channels = structure.get::<i32>("channels").unwrap_or(2).max(1) as u32;
        if let Err(e) = feed_zones(&block, &sound, &tee, channels, &routes) {
            warn!(
                "Show Playback {}: {} cannot reach its zones: {}",
                block.block_id, sound.id, e
            );
            sound.set_state(SoundState::Failed, Some(e));
            return;
        }
        if let Err(e) = pad.link(&sink) {
            sound.set_state(SoundState::Failed, Some(format!("{:?}", e)));
        }
    });

    block
        .sounds
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(request.id.clone(), sound.clone());
    for element in chain {
        element
            .sync_state_with_parent()
            .map_err(|e| e.to_string())?;
    }
    info!(
        "Show Playback {}: added {} ({})",
        block.block_id, request.id, uri
    );
    Ok(())
}

/// The crosspoints of `routes` as mix matrices, per zone: rows are zone
/// channels, columns file channels. No routes: the file's channels to the
/// same channels of the first zone, a mono file to all of them.
pub(crate) fn matrices(
    routes: &[SoundRoute],
    file_channels: u32,
    zone_channels: &[u32],
) -> BTreeMap<usize, Vec<Vec<f32>>> {
    let default_routes: Vec<SoundRoute>;
    let routes = if routes.is_empty() {
        let first = zone_channels.first().copied().unwrap_or(0);
        let channels = if file_channels == 1 {
            first
        } else {
            file_channels.min(first)
        };
        default_routes = (0..channels)
            .map(|c| SoundRoute {
                channel: c.min(file_channels - 1),
                zone: 0,
                zone_channel: c,
                gain_db: 0.0,
            })
            .collect();
        &default_routes
    } else {
        routes
    };
    let mut out: BTreeMap<usize, Vec<Vec<f32>>> = BTreeMap::new();
    for route in routes {
        let Some(&channels) = zone_channels.get(route.zone) else {
            continue;
        };
        if route.channel >= file_channels || route.zone_channel >= channels {
            continue;
        }
        let matrix = out
            .entry(route.zone)
            .or_insert_with(|| vec![vec![0.0; file_channels as usize]; channels as usize]);
        matrix[route.zone_channel as usize][route.channel as usize] +=
            gain_of_db(route.gain_db) as f32;
    }
    out
}

fn is_eos(info: &gst::PadProbeInfo) -> bool {
    matches!(&info.data, Some(gst::PadProbeData::Event(e)) if e.type_() == gst::EventType::Eos)
}

/// The sound is over: take its branch down.
fn end(block: Arc<ShowPlayback>, sound: Arc<Sound>) {
    sound.set_state(SoundState::Ended, None);
    // Not from a streaming thread: teardown sets the branch to NULL.
    std::thread::spawn(move || {
        if take(&block, &sound.id).is_some_and(|s| Arc::ptr_eq(&s, &sound)) {
            teardown(&block, &sound);
        }
    });
}

/// The first buffer is held at the gate: the sound is ready, and plays at
/// once if it should.
fn become_ready(block: Arc<ShowPlayback>, sound: Arc<Sound>) {
    sound.set_state(SoundState::Ready, None);
    debug!("Show Playback {}: {} is ready", block.block_id, sound.id);
    if sound
        .play_on_ready
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        // Not from inside the probe: removing it waits for it.
        std::thread::spawn(move || {
            if let Err(e) = release(&block, &sound) {
                warn!(
                    "Show Playback {}: {} did not start: {}",
                    block.block_id, sound.id, e
                );
            }
        });
    }
}

/// Seek `sound`'s file back to its start for its next play. A first
/// seek the file refuses leaves the sound playing once.
fn seek_again(block: &Arc<ShowPlayback>, sound: &Arc<Sound>, flush: bool) {
    let Some(pad) = sound.volume.upgrade().and_then(|v| v.static_pad("sink")) else {
        return;
    };
    let (block, weak) = (Arc::downgrade(block), Arc::downgrade(sound));
    repeat::seek_from_start(&pad, sound.repeats.flags(flush), move || {
        let (Some(block), Some(sound)) = (block.upgrade(), weak.upgrade()) else {
            return;
        };
        sound.repeats.refused();
        if flush {
            // Its first buffer still waits at the gate.
            become_ready(block, sound);
        }
    });
}

/// From the tee to every zone the sound is routed to.
fn feed_zones(
    block: &Arc<ShowPlayback>,
    sound: &Arc<Sound>,
    tee: &gst::Element,
    file_channels: u32,
    routes: &[SoundRoute],
) -> Result<(), String> {
    let bin = block.pipeline().ok_or("the flow is not running")?;
    let zone_channels: Vec<u32> = block.zones.iter().map(|z| z.channels).collect();
    let matrices = matrices(routes, file_channels, &zone_channels);
    sound
        .open_zones
        .store(matrices.len(), std::sync::atomic::Ordering::SeqCst);
    for (zone, matrix) in matrices {
        let mixer = block.mixers[zone].upgrade().ok_or("the zone is gone")?;
        let base = format!("{}_z{}", tee.name(), zone);
        let make = |factory: &str, part: &str| {
            gst::ElementFactory::make(factory)
                .name(format!("{}_{}", base, part))
                .build()
                .map_err(|e| format!("{}: {}", factory, e))
        };
        let queue = make("queue", "queue")?;
        let convert = make("audioconvert", "mix")?;
        let rows = matrix
            .iter()
            .map(|row| gst::Array::new(row.iter().map(|v| v.to_send_value())).to_send_value());
        convert.set_property("mix-matrix", gst::Array::new(rows));
        let caps = make("capsfilter", "caps")?;
        caps.set_property("caps", zone_caps(zone_channels[zone]));
        for element in [&queue, &convert, &caps] {
            sound.keep(element);
        }
        bin.add_many([&queue, &convert, &caps])
            .map_err(|e| e.to_string())?;
        gst::Element::link_many([&queue, &convert, &caps]).map_err(|e| e.to_string())?;
        let mixer_pad = mixer
            .request_pad_simple("sink_%u")
            .ok_or("the zone mixer gave no pad")?;
        let caps_src = caps.static_pad("src").ok_or("caps has no src pad")?;
        caps_src.link(&mixer_pad).map_err(|e| format!("{:?}", e))?;
        // The mixer takes the branch's buffers as it plays them, so its end
        // arrives here once the sound is heard. The EOS stays out of the
        // mixer: a mixer whose pads are all at their end ends the zone.
        let ending = (Arc::downgrade(block), Arc::downgrade(sound));
        caps_src.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if !is_eos(info) {
                return gst::PadProbeReturn::Ok;
            }
            if let (Some(block), Some(sound)) = (ending.0.upgrade(), ending.1.upgrade()) {
                let before = sound
                    .open_zones
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                if before == 1 {
                    end(block, sound);
                }
            }
            // Handled, not Drop: dropping a sticky event in a push probe
            // makes GStreamer 1.24 unref a NULL event.
            info.data = None;
            gst::PadProbeReturn::Handled
        });
        let level = attach_level(&mixer_pad, sound)?;
        sound
            .mixer_pads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(MixerPad {
                mixer: mixer.downgrade(),
                pad: mixer_pad.downgrade(),
                level,
            });
        tee.link(&queue).map_err(|e| e.to_string())?;
        for element in [&queue, &convert, &caps] {
            element
                .sync_state_with_parent()
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Play sound `id`: a preloaded one at once, one still loading as soon as
/// it is ready.
pub fn play(block: &Arc<ShowPlayback>, id: &str) -> Result<(), String> {
    let sound = find(block, id)?;
    match sound.state().0 {
        SoundState::Ready => release(block, &sound),
        SoundState::Loading => {
            // The gate releases it once its first buffer is there; it may
            // have got there meanwhile.
            sound
                .play_on_ready
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if sound.state().0 == SoundState::Ready {
                return release(block, &sound);
            }
            Ok(())
        }
        SoundState::Playing | SoundState::Paused => Ok(()),
        SoundState::Ended | SoundState::Failed => Err("the sound has ended".into()),
    }
}

/// Pause sound `id` (RUN.5): it goes silent and keeps its place. The
/// zones hold it on the way into their mixers; what the mixers have
/// taken already (a buffer or two) still plays. A fade runs on in the
/// zones' time meanwhile.
pub fn pause(block: &Arc<ShowPlayback>, id: &str) -> Result<(), String> {
    let sound = find(block, id)?;
    match sound.state().0 {
        SoundState::Playing => {}
        SoundState::Paused => return Ok(()),
        _ => return Err("the sound does not play".into()),
    }
    let now = block.running_time().ok_or("the zones are not running")?;
    let pads: Vec<gst::Pad> = sound
        .mixer_pads
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .filter_map(|p| p.pad.upgrade().and_then(|pad| pad.peer()))
        .collect();
    *sound.pause.lock().unwrap_or_else(|p| p.into_inner()) = Some(Pause::hold(pads, now));
    sound.set_state(SoundState::Paused, None);
    info!("Show Playback {}: {} paused", block.block_id, sound.id);
    Ok(())
}

/// Resume paused sound `id` where it was.
pub fn resume(block: &Arc<ShowPlayback>, id: &str) -> Result<(), String> {
    let sound = find(block, id)?;
    let Some(pause) = sound.pause.lock().unwrap_or_else(|p| p.into_inner()).take() else {
        return match sound.state().0 {
            SoundState::Playing => Ok(()),
            _ => Err("the sound is not paused".into()),
        };
    };
    let now = block.running_time().ok_or("the zones are not running")?;
    sound.set_state(SoundState::Playing, None);
    pause.end(now);
    info!("Show Playback {}: {} resumed", block.block_id, sound.id);
    Ok(())
}

/// Let a ready sound go: its start plays at the zones' running time now.
fn release(block: &ShowPlayback, sound: &Sound) -> Result<(), String> {
    let now = block.running_time().ok_or("the zones are not running")?;
    let volume = sound.volume.upgrade().ok_or("the sound is gone")?;
    let pad = volume.static_pad("src").ok_or("volume has no src pad")?;
    pad.set_offset(now.nseconds() as i64);
    *sound.started_at.lock().unwrap_or_else(|p| p.into_inner()) = Some(now);
    sound.set_state(SoundState::Playing, None);
    if let Some(gate) = sound.gate.lock().unwrap_or_else(|p| p.into_inner()).take() {
        pad.remove_probe(gate);
    }
    info!("Show Playback {}: {} plays", block.block_id, sound.id);
    Ok(())
}

/// Fade sound `id` to a level; with [`FadeEnd::Stop`] it stops at the end.
pub fn fade(block: &Arc<ShowPlayback>, id: &str, request: &SoundFadeRequest) -> Result<(), String> {
    let sound = find(block, id)?;
    let duration = request.duration_ms.min(FADE_MAX_MS);
    fade_sound(block, &sound, gain_of_db(request.gain_db), duration)?;
    if request.end == FadeEnd::Stop {
        stop_after(block, &sound, duration);
    }
    Ok(())
}

/// Stop sound `id`, faded out over `fade_ms`.
pub fn stop(block: &Arc<ShowPlayback>, id: &str, fade_ms: u64) -> Result<(), String> {
    let sound = find(block, id)?;
    let fade_ms = fade_ms.min(FADE_MAX_MS);
    // A paused sound is silent: it goes at once.
    if sound.state().0 == SoundState::Playing && fade_ms > 0 {
        fade_sound(block, &sound, 0.0, fade_ms)?;
        stop_after(block, &sound, fade_ms);
    } else {
        sound.set_state(SoundState::Ended, None);
        if let Some(sound) = take(block, id) {
            teardown(block, &sound);
        }
    }
    Ok(())
}

/// Set zone `index`'s level and mute.
pub fn set_zone(
    block: &Arc<ShowPlayback>,
    index: usize,
    request: &ZoneLevelRequest,
) -> Result<(), String> {
    let volume = block
        .zone_volumes
        .get(index)
        .ok_or_else(|| format!("zone {} does not exist", index))?
        .upgrade()
        .ok_or("the flow is not running")?;
    let current = volume.property::<f64>("volume");
    let gain = request.gain_db.map(gain_of_db).unwrap_or(current);
    let target = if request.mute == Some(true) {
        0.0
    } else {
        gain
    };
    let now = block.running_time().ok_or("the zones are not running")?;
    let duration = request.duration_ms.clamp(MIN_RAMP_MS, FADE_MAX_MS);
    ramp(&volume, current, target, now, duration)
}

/// The block's zones, stages, sounds and pictures.
pub fn state(block: &ShowPlayback) -> ShowPlaybackState {
    let sounds = block.sounds.lock().unwrap_or_else(|p| p.into_inner());
    let mut list: Vec<SoundInfo> = sounds
        .values()
        .map(|s| {
            let (state, error) = s.state();
            SoundInfo {
                id: s.id.clone(),
                state,
                error,
            }
        })
        .collect();
    list.sort_by(|a, b| a.id.cmp(&b.id));
    ShowPlaybackState {
        zones: block.zones.clone(),
        sounds: list,
        stages: block.stages.clone(),
        pictures: super::picture::infos(block),
    }
}

fn find(block: &ShowPlayback, id: &str) -> Result<Arc<Sound>, String> {
    block
        .sounds
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(id)
        .cloned()
        .ok_or_else(|| format!("no sound {}", id))
}

fn take(block: &ShowPlayback, id: &str) -> Option<Arc<Sound>> {
    block
        .sounds
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(id)
}

/// The cue level on a zone mixer pad: a control source on its volume,
/// in the zone's running time, so a fade runs on over the seams of a
/// repeated sound. A ramp already asked for is programmed on it too.
fn attach_level(pad: &gst::Pad, sound: &Sound) -> Result<InterpolationControlSource, String> {
    let level = sound.level.lock().unwrap_or_else(|p| p.into_inner());
    pad.set_property("volume", level.gain);
    let source = InterpolationControlSource::new();
    source.set_mode(InterpolationMode::Linear);
    // Absolute: control values are the volume itself (0..10), not 0..1.
    let binding = DirectControlBinding::new_absolute(pad, "volume", &source);
    pad.add_control_binding(&binding)
        .map_err(|e| e.to_string())?;
    match level.ramp {
        Some(r) => program(&source, r.from, r.to, r.start, r.duration_ms),
        None => {
            source.set(gst::ClockTime::ZERO, level.gain);
        }
    }
    Ok(source)
}

/// Fade a sound's level from where it is to `target` over `duration_ms`,
/// on every zone it feeds.
fn fade_sound(
    block: &ShowPlayback,
    sound: &Sound,
    target: f64,
    duration_ms: u64,
) -> Result<(), String> {
    let now = block.running_time().ok_or("the zones are not running")?;
    let mut level = sound.level.lock().unwrap_or_else(|p| p.into_inner());
    let ramp = Ramp {
        from: level.gain,
        to: target,
        start: now,
        duration_ms: duration_ms.max(MIN_RAMP_MS),
    };
    level.gain = target;
    level.ramp = Some(ramp);
    for pad in sound
        .mixer_pads
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
    {
        program(&pad.level, ramp.from, ramp.to, ramp.start, ramp.duration_ms);
    }
    Ok(())
}

/// A zone level ramp from `from` to `to`, starting at `start` (the zone's
/// running time is its volume's stream time).
fn ramp(
    volume: &gst::Element,
    from: f64,
    to: f64,
    start: gst::ClockTime,
    duration_ms: u64,
) -> Result<(), String> {
    let source = match volume.control_binding("volume") {
        Some(binding) => binding
            .property::<Option<gst::ControlSource>>("control-source")
            .and_then(|cs| cs.downcast::<InterpolationControlSource>().ok())
            .ok_or("the zone level has a foreign binding")?,
        None => {
            let source = InterpolationControlSource::new();
            source.set_mode(InterpolationMode::Linear);
            let binding = DirectControlBinding::new_absolute(volume, "volume", &source);
            volume
                .add_control_binding(&binding)
                .map_err(|e| e.to_string())?;
            source
        }
    };
    program(&source, from, to, start, duration_ms);
    Ok(())
}

/// Replace `source`'s keyframes with an S-curve from `from` at `start` to
/// `to` after `duration_ms`, holding `to` after that.
pub(super) fn program(
    source: &InterpolationControlSource,
    from: f64,
    to: f64,
    start: gst::ClockTime,
    duration_ms: u64,
) {
    source.unset_all();
    let duration_ns = duration_ms * 1_000_000;
    for i in 0..=FADE_KEYFRAMES {
        let t = f64::from(i) / f64::from(FADE_KEYFRAMES);
        let at = start + gst::ClockTime::from_nseconds((duration_ns as f64 * t) as u64);
        source.set(at, from + (to - from) * s_curve(t));
    }
}

/// Stop `sound` once `delay_ms` has passed.
fn stop_after(block: &Arc<ShowPlayback>, sound: &Arc<Sound>, delay_ms: u64) {
    let (block, sound) = (Arc::downgrade(block), Arc::downgrade(sound));
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms + MIN_RAMP_MS));
        let (Some(block), Some(sound)) = (block.upgrade(), sound.upgrade()) else {
            return;
        };
        sound.set_state(SoundState::Ended, None);
        if take(&block, &sound.id).is_some_and(|s| Arc::ptr_eq(&s, &sound)) {
            teardown(&block, &sound);
        }
    });
}

/// Take the branch out of the pipeline and give the mixer pads back.
fn teardown(block: &ShowPlayback, sound: &Sound) {
    if let Some(pause) = sound.pause.lock().unwrap_or_else(|p| p.into_inner()).take() {
        pause.drop_probes();
    }
    for MixerPad { mixer, pad, .. } in sound
        .mixer_pads
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .drain(..)
    {
        if let (Some(mixer), Some(pad)) = (mixer.upgrade(), pad.upgrade()) {
            if let Some(peer) = pad.peer() {
                let _ = peer.unlink(&pad);
            }
            mixer.release_request_pad(&pad);
        }
    }
    let elements: Vec<gst::Element> = sound
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
    debug!("Show Playback {}: {} taken down", block.block_id, sound.id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(channel: u32, zone: usize, zone_channel: u32, gain_db: f64) -> SoundRoute {
        SoundRoute {
            channel,
            zone,
            zone_channel,
            gain_db,
        }
    }

    #[test]
    fn no_routes_put_the_file_on_the_first_zone() {
        let m = matrices(&[], 2, &[2, 4]);
        assert_eq!(m.len(), 1);
        assert_eq!(m[&0], vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
        // Mono into stereo: both channels.
        assert_eq!(matrices(&[], 1, &[2])[&0], vec![vec![1.0], vec![1.0]]);
    }

    #[test]
    fn routes_build_one_matrix_per_zone() {
        // Mono to both sides of zone 0 at -6 dB, and to channel 3 of zone 1.
        let m = matrices(
            &[
                route(0, 0, 0, -6.0),
                route(0, 0, 1, -6.0),
                route(0, 1, 2, 0.0),
            ],
            1,
            &[2, 4],
        );
        let half = gain_of_db(-6.0) as f32;
        assert_eq!(m[&0], vec![vec![half], vec![half]]);
        assert_eq!(m[&1], vec![vec![0.0], vec![0.0], vec![1.0], vec![0.0]]);
    }

    #[test]
    fn impossible_routes_are_left_out() {
        let m = matrices(
            &[
                route(3, 0, 0, 0.0),
                route(0, 5, 0, 0.0),
                route(0, 0, 9, 0.0),
            ],
            2,
            &[2],
        );
        assert!(m.is_empty());
    }
}
