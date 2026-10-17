//! Repeated plays of a sound or video from segment seeks, without a gap.
//!
//! ```text
//! first buffer at the gate ──► seek FLUSH|SEGMENT (it waits for the flush)
//! SEGMENT_DONE             ──► seek SEGMENT, no flush: the new segment
//!                              continues the running time
//! last play                ──► plain seek: it ends with EOS
//! ```
//!
//! The first seek's flush stays at the gate: nothing has passed it yet.

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use strom_types::show_playback::SoundRepeat;
use tracing::warn;

/// Where the plays of one file are.
pub(super) struct Repeats {
    /// Plays still to come after this one; `u32::MAX`: forever.
    plays_left: AtomicU32,
    /// The first buffer is still to come and starts the first seek.
    first_seek: AtomicBool,
    /// The first seek's flush is still to come.
    seeking: AtomicBool,
}

/// What the gate's event probe does with an event.
pub(super) enum Seam {
    /// Not ours: on downstream.
    Pass,
    /// The first seek's flush: kept at the gate.
    Swallow,
    /// A play ended: kept at the gate, and the next one is to be sought.
    Next,
}

impl Repeats {
    pub(super) fn new(repeat: SoundRepeat) -> Self {
        let plays_left = match repeat {
            SoundRepeat::Once => 0,
            SoundRepeat::Times(n) => n.saturating_sub(1),
            SoundRepeat::Forever => u32::MAX,
        };
        Self {
            plays_left: AtomicU32::new(plays_left),
            first_seek: AtomicBool::new(plays_left > 0),
            seeking: AtomicBool::new(false),
        }
    }

    /// For the gate's buffer probe: whether this buffer starts the first
    /// seek (then it stays blocked until the seek's flush).
    pub(super) fn first_buffer(&self) -> bool {
        if self.first_seek.swap(false, SeqCst) {
            self.seeking.store(true, SeqCst);
            true
        } else {
            false
        }
    }

    /// For the gate's event probe (downstream and flush events).
    pub(super) fn event(&self, kind: gst::EventType) -> Seam {
        let seeking = self.seeking.load(SeqCst);
        match kind {
            gst::EventType::FlushStart if seeking => Seam::Swallow,
            gst::EventType::FlushStop if seeking => {
                self.seeking.store(false, SeqCst);
                Seam::Swallow
            }
            gst::EventType::SegmentDone => {
                let left = self.plays_left.load(SeqCst);
                if left != u32::MAX {
                    self.plays_left.store(left.saturating_sub(1), SeqCst);
                }
                Seam::Next
            }
            _ => Seam::Pass,
        }
    }

    /// The next seek's flags: a segment seek while more plays are to come
    /// after it, so its end is a seam; a plain one for the last play.
    pub(super) fn flags(&self, flush: bool) -> gst::SeekFlags {
        let mut flags = gst::SeekFlags::ACCURATE;
        if flush {
            flags |= gst::SeekFlags::FLUSH;
        }
        if self.plays_left.load(SeqCst) > 0 {
            flags |= gst::SeekFlags::SEGMENT;
        }
        flags
    }

    /// Play once whatever was asked (a still image repeats itself).
    pub(super) fn once(&self) {
        self.plays_left.store(0, SeqCst);
        self.first_seek.store(false, SeqCst);
    }

    /// The file refused a seek: it plays once more and ends there.
    pub(super) fn refused(&self) {
        self.plays_left.store(0, SeqCst);
        self.seeking.store(false, SeqCst);
    }
}

/// Seek with `flags` from `pad` (a sink pad of the branch) back to the
/// file's start, on another thread: a seek from a streaming thread would
/// wait for itself. `refused` runs when the file refuses.
pub(super) fn seek_from_start<F>(pad: &gst::Pad, flags: gst::SeekFlags, refused: F)
where
    F: FnOnce() + Send + 'static,
{
    let pad: glib::WeakRef<gst::Pad> = pad.downgrade();
    std::thread::spawn(move || {
        let Some(pad) = pad.upgrade() else {
            return;
        };
        let seek = gst::event::Seek::new(
            1.0,
            flags,
            gst::SeekType::Set,
            gst::ClockTime::ZERO,
            gst::SeekType::None,
            gst::ClockTime::NONE,
        );
        if !pad.push_event(seek) {
            warn!(
                "Show Playback: {} refused a seek, so it does not repeat",
                pad.name()
            );
            refused();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plays_count_down_to_a_plain_last_seek() {
        let thrice = Repeats::new(SoundRepeat::Times(3));
        assert!(thrice.first_buffer());
        assert!(!thrice.first_buffer(), "one first seek");
        assert!(thrice.flags(true).contains(gst::SeekFlags::SEGMENT));
        assert!(matches!(
            thrice.event(gst::EventType::FlushStart),
            Seam::Swallow
        ));
        assert!(matches!(
            thrice.event(gst::EventType::FlushStop),
            Seam::Swallow
        ));
        assert!(
            matches!(thrice.event(gst::EventType::FlushStop), Seam::Pass),
            "later flushes pass"
        );
        assert!(matches!(
            thrice.event(gst::EventType::SegmentDone),
            Seam::Next
        ));
        assert!(
            thrice.flags(false).contains(gst::SeekFlags::SEGMENT),
            "a third play to come"
        );
        thrice.event(gst::EventType::SegmentDone);
        assert!(
            !thrice.flags(false).contains(gst::SeekFlags::SEGMENT),
            "the last: plain"
        );

        let once = Repeats::new(SoundRepeat::Once);
        assert!(!once.first_buffer(), "a single play needs no seek");
        let forever = Repeats::new(SoundRepeat::Forever);
        forever.event(gst::EventType::SegmentDone);
        assert!(forever.flags(false).contains(gst::SeekFlags::SEGMENT));
    }
}
