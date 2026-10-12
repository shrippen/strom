//! Still frame while the player is paused or at the end of its playlist.
//!
//! Pausing stops the internal pipeline, so the main pipeline's video appsrc
//! gets no more buffers. A live aggregator further down (a vision mixer
//! input, also across `inter`) then counts the pad as active but starving
//! and waits for its deadline on every output frame: a paused clip anywhere
//! on the mixer added ~30 ms to program (lmix: 86 instead of 56 ms on a CUDA
//! mixer). Holding repeats the last video frame at its frame rate with the
//! main pipeline's running time, like a paused player in a hardware mixer.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::debug;

/// Frame interval when the caps carry no frame rate.
const DEFAULT_INTERVAL: Duration = Duration::from_millis(40);

/// Last video sample the bridge pushed, and the repeat that holds it.
#[derive(Default)]
pub struct StillFrame {
    last: Mutex<Option<gst::Sample>>,
    /// Bumped by every hold and release; a repeat runs while it matches.
    generation: AtomicU64,
}

impl StillFrame {
    /// Remember `sample` as the frame to hold.
    pub fn store(&self, sample: &gst::Sample) {
        if let Ok(mut last) = self.last.lock() {
            *last = Some(sample.clone());
        }
    }

    /// Repeat the last frame into `appsrc` until [`release`](Self::release).
    pub fn hold(
        self: &Arc<Self>,
        appsrc: &gst_app::AppSrc,
        main: gst::glib::WeakRef<gst::Pipeline>,
    ) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let this = Arc::downgrade(self);
        let appsrc = appsrc.downgrade();
        let spawned = std::thread::Builder::new()
            .name("player-still".into())
            .spawn(move || loop {
                let Some(this) = this.upgrade() else { break };
                if this.generation.load(Ordering::SeqCst) != generation {
                    break;
                }
                let (Some(appsrc), Some(main)) = (appsrc.upgrade(), main.upgrade()) else {
                    break;
                };
                let sample = this.last.lock().ok().and_then(|last| last.clone());
                drop(this);
                let interval = sample
                    .as_ref()
                    .and_then(|s| s.caps().and_then(frame_interval))
                    .unwrap_or(DEFAULT_INTERVAL);
                if let (Some(sample), Some(now)) = (sample, running_time(&main)) {
                    if let Some(repeat) = restamp(&sample, now, interval) {
                        // FLUSHING while the main pipeline (re)starts is
                        // transient; the next round tries again.
                        let _ = appsrc.push_sample(&repeat);
                    }
                }
                std::thread::sleep(interval);
            });
        if let Err(e) = spawned {
            debug!("Media Player: no still-frame thread: {}", e);
        }
    }

    /// Stop repeating (play, file switch).
    pub fn release(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

fn running_time(pipeline: &gst::Pipeline) -> Option<gst::ClockTime> {
    let clock = pipeline.clock()?;
    let base_time = pipeline.base_time()?;
    Some(clock.time().saturating_sub(base_time))
}

fn frame_interval(caps: &gst::CapsRef) -> Option<Duration> {
    let fps = caps.structure(0)?.get::<gst::Fraction>("framerate").ok()?;
    if fps.numer() <= 0 || fps.denom() <= 0 {
        return None;
    }
    Some(Duration::from_nanos(
        1_000_000_000u64 * fps.denom() as u64 / fps.numer() as u64,
    ))
}

/// `sample` again, stamped at running time `now` for one frame.
fn restamp(sample: &gst::Sample, now: gst::ClockTime, interval: Duration) -> Option<gst::Sample> {
    let mut buffer = sample.buffer()?.copy();
    {
        let buffer = buffer.get_mut()?;
        buffer.set_pts(now);
        buffer.set_dts(gst::ClockTime::NONE);
        buffer.set_duration(gst::ClockTime::from_nseconds(interval.as_nanos() as u64));
    }
    let caps = sample.caps_owned();
    let mut builder = gst::Sample::builder().buffer(&buffer);
    if let Some(caps) = &caps {
        builder = builder.caps(caps);
    }
    Some(builder.build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_interval_from_caps() {
        gst::init().unwrap();
        let caps = gst::Caps::builder("video/x-raw")
            .field("framerate", gst::Fraction::new(30, 1))
            .build();
        assert_eq!(
            frame_interval(&caps),
            Some(Duration::from_nanos(33_333_333))
        );
        let still = gst::Caps::builder("video/x-raw")
            .field("framerate", gst::Fraction::new(0, 1))
            .build();
        assert_eq!(frame_interval(&still), None);
    }

    /// A held frame keeps arriving with fresh timestamps until released.
    #[test]
    fn hold_repeats_until_release() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let appsrc = gst_app::AppSrc::builder()
            .format(gst::Format::Time)
            .is_live(true)
            .build();
        let sink = gst_app::AppSink::builder().sync(false).build();
        pipeline
            .add_many([
                appsrc.upcast_ref::<gst::Element>(),
                sink.upcast_ref::<gst::Element>(),
            ])
            .unwrap();
        appsrc.link(&sink).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "GRAY8")
            .field("width", 2)
            .field("height", 2)
            .field("framerate", gst::Fraction::new(50, 1))
            .build();
        let sample = gst::Sample::builder()
            .buffer(&gst::Buffer::from_slice([0u8; 4]))
            .caps(&caps)
            .build();
        let still = Arc::new(StillFrame::default());
        still.store(&sample);
        still.hold(&appsrc, pipeline.downgrade());

        let mut last_pts = None;
        for _ in 0..3 {
            let got = sink
                .try_pull_sample(gst::ClockTime::from_seconds(2))
                .expect("held frame repeats");
            let pts = got.buffer().unwrap().pts();
            assert!(pts > last_pts, "{:?} after {:?}", pts, last_pts);
            last_pts = pts;
        }

        still.release();
        std::thread::sleep(Duration::from_millis(100));
        while sink.try_pull_sample(gst::ClockTime::ZERO).is_some() {}
        assert!(
            sink.try_pull_sample(gst::ClockTime::from_mseconds(200))
                .is_none(),
            "released frame stops"
        );
        pipeline.set_state(gst::State::Null).unwrap();
    }
}
