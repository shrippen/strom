//! How long a video encoder takes per frame.
//!
//! Two buffer probes note when a frame enters the encoder (sink pad) and when
//! the encoded frame with the same running time leaves it (src pad), and add the time
//! between to a sum. A client reads the sum twice and takes the encode time
//! per frame, or the share of the wall time the encoder was busy: at 100 %
//! it no longer keeps up with real time.
//!
//! The probes fire for every frame, so they only touch atomics: no lock, no
//! allocation, no system call beyond `Instant::now()`.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use strom_types::stats::VideoEncoderStats;

/// Frames that can be inside the encoder at once and still be timed. A
/// zero-latency encoder holds one; a frame pushed out of its slot by 16
/// newer ones before it leaves goes untimed.
const SLOTS: usize = 16;

/// A slot without a frame (`GST_CLOCK_TIME_NONE` is no running time a
/// frame has).
const EMPTY: u64 = u64::MAX;

/// The element data key the timing hangs on.
const KEY: &str = "strom-encode-timing";

/// A frame inside the encoder: its running time and when it went in.
struct Slot {
    running: AtomicU64,
    entered_ns: AtomicU64,
}

/// The running sums of one encoder.
struct EncodeTiming {
    epoch: Instant,
    slots: [Slot; SLOTS],
    next: AtomicUsize,
    frames: AtomicU64,
    busy_ns: AtomicU64,
}

impl EncodeTiming {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
            slots: std::array::from_fn(|_| Slot {
                running: AtomicU64::new(EMPTY),
                entered_ns: AtomicU64::new(0),
            }),
            next: AtomicUsize::new(0),
            frames: AtomicU64::new(0),
            busy_ns: AtomicU64::new(0),
        }
    }

    fn now_ns(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    /// A frame at `running` time went in. The slot is emptied first, so a reader
    /// never pairs the new frame with the old time.
    fn entered(&self, running: u64) {
        let slot = &self.slots[self.next.fetch_add(1, Ordering::Relaxed) % SLOTS];
        slot.running.store(EMPTY, Ordering::Release);
        slot.entered_ns.store(self.now_ns(), Ordering::Release);
        slot.running.store(running, Ordering::Release);
    }

    /// The frame at `running` time came out: its time counts once. A slot the sink
    /// side reused in between is left alone (the claim fails).
    fn left(&self, running: u64) {
        let now = self.now_ns();
        let Some(slot) = self
            .slots
            .iter()
            .find(|s| s.running.load(Ordering::Acquire) == running)
        else {
            return;
        };
        let entered = slot.entered_ns.load(Ordering::Acquire);
        if slot
            .running
            .compare_exchange(running, EMPTY, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        self.busy_ns
            .fetch_add(now.saturating_sub(entered), Ordering::Relaxed);
        self.frames.fetch_add(1, Ordering::Relaxed);
    }

    /// Forget the frames inside: after a flush or a new segment the running
    /// time may start again lower, and an old slot would pair with a new
    /// frame of the same running time (one huge encode time in the sum).
    fn clear(&self) {
        for slot in &self.slots {
            slot.running.store(EMPTY, Ordering::Release);
        }
    }

    fn stats(&self) -> VideoEncoderStats {
        VideoEncoderStats {
            frames: self.frames.load(Ordering::Relaxed),
            encode_time_ns: self.busy_ns.load(Ordering::Relaxed),
            hardware: false,
        }
    }
}

/// A pad's current TIME segment, kept for the buffer probe: the running
/// time of a PTS is `pts - start + base` (forward, rate 1). The encoder
/// moves its output segment (x264enc starts it 1000 h later to leave room
/// for negative DTS), so frames are matched in running time, not PTS.
struct PadSegment {
    /// `start + offset` of the segment; EMPTY for one that cannot be mapped.
    start: AtomicU64,
    base: AtomicU64,
}

impl PadSegment {
    fn new() -> Self {
        Self {
            start: AtomicU64::new(EMPTY),
            base: AtomicU64::new(0),
        }
    }

    /// Keep `event`'s segment, if it is one.
    fn update(&self, event: &gst::EventRef) {
        let gst::EventView::Segment(segment) = event.view() else {
            return;
        };
        let start = segment
            .segment()
            .downcast_ref::<gst::ClockTime>()
            .filter(|s| s.rate() == 1.0)
            .and_then(|s| Some((s.start()? + s.offset()?, s.base()?)));
        let Some((start, base)) = start else {
            self.start.store(EMPTY, Ordering::Release);
            return;
        };
        self.base.store(base.nseconds(), Ordering::Release);
        self.start.store(start.nseconds(), Ordering::Release);
    }

    /// The running time of `buffer`, if it has one.
    fn running_time(&self, buffer: &gst::BufferRef) -> Option<u64> {
        let start = self.start.load(Ordering::Acquire);
        if start == EMPTY {
            return None;
        }
        let since = buffer.pts()?.nseconds().checked_sub(start)?;
        Some(since + self.base.load(Ordering::Acquire))
    }
}

/// Call `frame` with the running time of each buffer through `pad`.
fn watch_pad(pad: &gst::Pad, frame: impl Fn(u64) + Send + Sync + 'static) {
    let segment = PadSegment::new();
    let mask = gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM;
    pad.add_probe(mask, move |_, info| {
        match &info.data {
            Some(gst::PadProbeData::Buffer(buffer)) => {
                if let Some(running) = segment.running_time(buffer) {
                    frame(running);
                }
            }
            Some(gst::PadProbeData::Event(event)) => segment.update(event),
            _ => {}
        }
        gst::PadProbeReturn::Ok
    });
}

/// Time the frames `encoder` encodes, readable with [`encoder_stats`]. An
/// encoder without static `sink` and `src` pads is left alone.
pub fn time_encoder(encoder: &gst::Element) {
    let (Some(sink), Some(src)) = (encoder.static_pad("sink"), encoder.static_pad("src")) else {
        return;
    };
    let timing = Arc::new(EncodeTiming::new());

    let entering = timing.clone();
    watch_pad(&sink, move |running| entering.entered(running));
    let clearing = timing.clone();
    sink.add_probe(
        gst::PadProbeType::EVENT_DOWNSTREAM | gst::PadProbeType::EVENT_FLUSH,
        move |_, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data {
                if matches!(
                    event.type_(),
                    gst::EventType::Segment | gst::EventType::FlushStop
                ) {
                    clearing.clear();
                }
            }
            gst::PadProbeReturn::Ok
        },
    );
    let leaving = timing.clone();
    watch_pad(&src, move |running| leaving.left(running));

    // SAFETY: KEY is private to this module and only ever holds an
    // `Arc<EncodeTiming>`; `encoder_stats` reads it as that type. Call this
    // only once per element: a second call would replace (and free) the data
    // while a reader in `encoder_stats` may still hold the old pointer.
    unsafe { encoder.set_data(KEY, timing) };
}

/// What `encoder` encoded since it started, if [`time_encoder`] watches it.
pub fn encoder_stats(encoder: &gst::Element) -> Option<VideoEncoderStats> {
    // SAFETY: see `time_encoder`; the data lives as long as the element,
    // which the caller holds.
    let timing = unsafe { encoder.data::<Arc<EncodeTiming>>(KEY)? };
    let hardware = encoder
        .factory()
        .is_some_and(|f| crate::gst::software_decoder::is_hardware(&f));
    Some(VideoEncoderStats {
        hardware,
        ..unsafe { timing.as_ref() }.stats()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_counts_once_by_its_running_time() {
        let timing = EncodeTiming::new();
        timing.entered(1_000);
        timing.entered(2_000);
        timing.left(1_000);
        timing.left(1_000); // a second buffer with the same running time
        timing.left(3_000); // never went in
        assert_eq!(timing.stats().frames, 1);
        timing.left(2_000);
        assert_eq!(timing.stats().frames, 2);
    }

    #[test]
    fn a_frame_pushed_out_of_its_slot_goes_untimed() {
        let timing = EncodeTiming::new();
        for running in 0..=SLOTS as u64 {
            timing.entered(running);
        }
        timing.left(0);
        timing.left(SLOTS as u64);
        assert_eq!(timing.stats().frames, 1);
    }

    #[test]
    fn a_cleared_ring_pairs_nothing_old() {
        let timing = EncodeTiming::new();
        timing.entered(1_000);
        timing.clear();
        timing.left(1_000);
        assert_eq!(timing.stats().frames, 0);
        timing.entered(1_000);
        timing.left(1_000);
        assert_eq!(timing.stats().frames, 1);
    }

    #[test]
    fn the_time_inside_adds_up() {
        let timing = EncodeTiming::new();
        timing.entered(1_000);
        std::thread::sleep(std::time::Duration::from_millis(5));
        timing.left(1_000);
        assert!(timing.stats().encode_time_ns >= 5_000_000);
    }
}
