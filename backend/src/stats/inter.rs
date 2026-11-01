//! The receive queue of an Inter Input: the `appsrc` inside `intersrc`,
//! which drops the oldest buffers once it holds `max-time` (leaky
//! downstream).
//!
//! `appsrc` counts its drops itself only from GStreamer 1.28 on (`dropped`).
//! Before that the drops are counted from the `dropped-buffer` element
//! message the inter plugin's consumer posts for each one (gst-utils
//! `StreamProducer`, on `enough-data`). The message only comes when a
//! buffer is dropped, so nothing runs per buffer.

use crate::blocks::BusMessageConnectFn;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use strom_types::stats::InterInputStats;

/// The element data key the message count hangs on.
const KEY: &str = "strom-inter-drops";

/// The element message the inter consumer posts for a dropped buffer.
const DROPPED_MESSAGE: &str = "dropped-buffer";

/// Count the drops of `intersrc`'s queue from the bus, readable with
/// [`inter_input_stats`]. Call once per element, at build.
pub fn count_drops(intersrc: &gst::Element) -> BusMessageConnectFn {
    let drops = Arc::new(AtomicU64::new(0));
    // SAFETY: KEY is private to this module and only ever holds an
    // `Arc<AtomicU64>`; `inter_input_stats` reads it as that type. Called
    // once per element, so the data is never replaced under a reader.
    unsafe { intersrc.set_data(KEY, drops.clone()) };

    // The queue is the bin's child `<intersrc>-appsrc`; matched by name, so
    // the handler holds no reference to the element.
    let appsrc = format!("{}-appsrc", intersrc.name());
    Box::new(move |bus, _flow_id, _events| {
        bus.connect_message(Some("element"), move |_, msg| {
            let ours = msg.src().is_some_and(|src| src.name() == appsrc.as_str());
            let dropped = msg.structure().is_some_and(|s| s.name() == DROPPED_MESSAGE);
            if ours && dropped {
                drops.fetch_add(1, Ordering::Relaxed);
            }
        })
    })
}

/// The receive queue of `intersrc`; `None` without the queue.
pub fn inter_input_stats(intersrc: &gst::Element) -> Option<InterInputStats> {
    let appsrc = intersrc
        .downcast_ref::<gst::Bin>()?
        .iterate_elements()
        .find(|e| e.is::<gstreamer_app::AppSrc>())?;
    let dropped_buffers = match appsrc.find_property("dropped") {
        Some(_) => appsrc.property("dropped"),
        None => counted_drops(intersrc),
    };
    Some(InterInputStats {
        queued_buffers: appsrc.property("current-level-buffers"),
        queued_time_ns: appsrc.property("current-level-time"),
        max_time_ns: appsrc.property::<u64>("max-time"),
        dropped_buffers,
    })
}

/// The drops [`count_drops`] counted on `intersrc`, 0 without a count.
fn counted_drops(intersrc: &gst::Element) -> u64 {
    // SAFETY: see `count_drops`; the data lives as long as the element,
    // which the caller holds.
    unsafe { intersrc.data::<Arc<AtomicU64>>(KEY) }
        .map(|drops| unsafe { drops.as_ref() }.load(Ordering::Relaxed))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventBroadcaster;

    /// Before GStreamer 1.28 the drops come from the bus: each
    /// `dropped-buffer` message of this queue counts, others do not.
    #[test]
    fn drops_are_counted_from_the_bus() {
        gst::init().unwrap();
        let _ = gstrsinter::plugin_register_static();
        let intersrc = gst::ElementFactory::make("intersrc")
            .name("in:intersrc")
            .build()
            .unwrap();
        let pipeline = gst::Pipeline::new();
        pipeline.add(&intersrc).unwrap();
        let bus = pipeline.bus().unwrap();
        bus.add_signal_watch();
        let connect = count_drops(&intersrc);
        connect(&bus, uuid::Uuid::nil(), EventBroadcaster::with_capacity(1));

        let appsrc = intersrc
            .downcast_ref::<gst::Bin>()
            .unwrap()
            .by_name("in:intersrc-appsrc")
            .unwrap();
        let other = gst::ElementFactory::make("identity").build().unwrap();
        pipeline.add(&other).unwrap();
        let post = |from: &gst::Element, name: &str| {
            let msg = gst::message::Element::builder(gst::Structure::new_empty(name))
                .src(from)
                .build();
            from.post_message(msg).unwrap();
        };
        post(&appsrc, DROPPED_MESSAGE);
        post(&appsrc, DROPPED_MESSAGE);
        post(&appsrc, "something-else");
        post(&other, DROPPED_MESSAGE);

        let context = gst::glib::MainContext::default();
        while context.iteration(false) {}
        assert_eq!(counted_drops(&intersrc), 2);
        bus.remove_signal_watch();
    }
}
