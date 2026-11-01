//! Downloads GPU memory in front of a system-memory consumer whose input is
//! only known at runtime: the CPU vision mixer's `videoconvert`, fed by
//! whatever decoder `decodebin` autoplugged upstream.
//!
//! [`install_memory_bridge`] watches an already-linked source pad and, on
//! the first CAPS event in CUDA, VA or GL memory, splices the matching
//! download between that pad and its peer. Relinking from inside a
//! push-event probe is a supported pattern: `gst_pad_push_event_unchecked`
//! resolves the peer and rechecks pending sticky events *after* running its
//! probes, so the event that triggered the splice lands on the newly
//! inserted element.
//!
//! GL consumers adapt their inputs with [`crate::gst::gl_input_front`].

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{info, warn};

/// The caps feature of plain system memory (implicit when absent).
const SYSTEM_MEMORY_FEATURE: &str = "memory:SystemMemory";

/// GPU memory a system-memory consumer cannot take, and its download.
const DOWNLOAD_BRIDGES: [(&str, &str); 3] = [
    ("memory:CUDAMemory", "cudadownload"),
    ("memory:VAMemory", "vapostproc"),
    (crate::gst::video_adapt::GL_MEMORY_FEATURE, "gldownload"),
];

/// The element that turns `caps`' frames into system memory, or `None` if
/// they are there already.
fn bridge_for(caps: &gst::CapsRef) -> Option<&'static str> {
    let structure = caps.structure(0)?;
    if structure.name() != "video/x-raw" {
        return None;
    }
    let features = caps.features(0)?;
    DOWNLOAD_BRIDGES
        .iter()
        .find(|(feature, _)| features.contains(*feature))
        .map(|(_, bridge)| *bridge)
}

/// Watch `src_pad` (linked towards a system-memory consumer) and splice the
/// download from
/// [`bridge_for`] in front of its peer on the first CAPS event that needs
/// one.
///
/// Upstream only sends that CAPS event if negotiation succeeds, and it asks
/// first: a CAPS query, answered by the consumer, lists no foreign memory.
/// A query probe therefore adds the bridgeable memory variants to the
/// answer (and accepts them in ACCEPT_CAPS) until the first CAPS event has
/// been classified; then both probes are removed. Query and event probes
/// fire per negotiation, not per buffer.
pub fn install_memory_bridge(src_pad: &gst::Pad, name_prefix: &str) {
    let name_prefix = name_prefix.to_string();

    let query_probe = src_pad.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, move |pad, info| {
        // Only after the peer answered (PULL); PUSH is the way down.
        if !info.mask.contains(gst::PadProbeType::PULL) {
            return gst::PadProbeReturn::Ok;
        }
        let Some(query) = info.query_mut() else {
            return gst::PadProbeReturn::Ok;
        };
        match query.view_mut() {
            gst::QueryViewMut::Caps(q) => {
                // Ask the peer unfiltered: with a CUDA filter its answer is
                // already empty. Querying the peer pad directly does not
                // re-enter this probe.
                let Some(peer) = pad.peer() else {
                    return gst::PadProbeReturn::Ok;
                };
                let answer = with_bridgeable_memory(&peer.query_caps(None));
                let answer = match q.filter_owned() {
                    Some(filter) => {
                        filter.intersect_with_mode(&answer, gst::CapsIntersectMode::First)
                    }
                    None => answer,
                };
                q.set_result(&answer);
            }
            gst::QueryViewMut::AcceptCaps(q) if !q.result() && bridge_for(q.caps()).is_some() => {
                q.set_result(true);
            }
            _ => {}
        }
        gst::PadProbeReturn::Ok
    });
    let query_probe = std::sync::Mutex::new(query_probe);

    // EVENT_DOWNSTREAM, not BLOCK/BUFFER: this fires per event, not per buffer.
    src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(ref event)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };

        if let Some(id) = query_probe.lock().ok().and_then(|mut id| id.take()) {
            pad.remove_probe(id);
        }
        let Some(bridge) = bridge_for(caps_event.caps()) else {
            return gst::PadProbeReturn::Remove;
        };
        if let Err(e) = splice(pad, bridge, &name_prefix) {
            warn!(
                "{}: GPU-memory input could not be downloaded ({}), it will not negotiate",
                name_prefix, e
            );
        }
        gst::PadProbeReturn::Remove
    });
}

/// `caps` plus a copy of each system-memory structure (the formats the
/// consumer takes once downloaded) per downloadable memory type, for the bridges
/// whose element exists on this host.
fn with_bridgeable_memory(caps: &gst::Caps) -> gst::Caps {
    let mut out = caps.copy();
    for (feature, bridge) in &DOWNLOAD_BRIDGES {
        if gst::ElementFactory::find(bridge).is_none() {
            continue;
        }
        let out = out.make_mut();
        for (s, features) in caps.iter_with_features() {
            let gpu_memory = features
                .iter()
                .any(|f| f.starts_with("memory:") && f != SYSTEM_MEMORY_FEATURE);
            if features.is_any() || gpu_memory {
                continue;
            }
            out.append_structure_full(s.to_owned(), Some(gst::CapsFeatures::new([*feature])));
        }
    }
    out
}

/// Insert a `factory` element between `src_pad` and its current peer.
fn splice(src_pad: &gst::Pad, factory: &str, name_prefix: &str) -> Result<(), String> {
    let peer = src_pad
        .peer()
        .ok_or_else(|| "source pad has no peer".to_string())?;

    // Strong references stay local to this function — never captured in a
    // closure, so no reference cycle can outlive the pipeline.
    let bin = src_pad
        .parent_element()
        .and_then(|element| element.parent())
        .and_then(|parent| parent.downcast::<gst::Bin>().ok())
        .ok_or_else(|| "source pad's element has no parent bin".to_string())?;

    let name = format!("{}_{}", name_prefix, factory);
    let element = gst::ElementFactory::make(factory)
        .name(&name)
        .build()
        .map_err(|e| format!("{} could not be created: {}", factory, e))?;

    bin.add(&element)
        .map_err(|e| format!("{} could not be added to {}: {}", factory, bin.name(), e))?;

    let sink = element
        .static_pad("sink")
        .ok_or_else(|| format!("{} has no sink pad", factory))?;
    let src = element
        .static_pad("src")
        .ok_or_else(|| format!("{} has no src pad", factory))?;

    // Unlink before linking: the peer accepts one upstream link only.
    src_pad.unlink(&peer).map_err(|e| {
        format!(
            "could not unlink {} from {}: {}",
            src_pad.name(),
            peer.name(),
            e
        )
    })?;
    src.link(&peer)
        .map_err(|e| format!("could not link {} to {}: {}", factory, peer.name(), e))?;
    src_pad
        .link(&sink)
        .map_err(|e| format!("could not link {} to {}: {}", src_pad.name(), factory, e))?;

    // State last, so the element negotiates against a complete topology.
    element
        .sync_state_with_parent()
        .map_err(|e| format!("{} could not reach the pipeline state: {}", factory, e))?;

    info!(
        "{}: input memory needs {}, inserted {} in front of {}",
        name_prefix,
        factory,
        name,
        peer.name()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn caps(s: &str) -> gst::Caps {
        // Caps parsing needs the type system registered.
        let _ = gst::init();
        gst::Caps::from_str(s).expect("valid caps")
    }

    #[test]
    fn every_gpu_memory_is_downloaded() {
        let bridge = |c: &str| bridge_for(&caps(c));

        assert_eq!(
            bridge("video/x-raw(memory:CUDAMemory), format=NV12"),
            Some("cudadownload")
        );
        assert_eq!(
            bridge("video/x-raw(memory:VAMemory), format=NV12"),
            Some("vapostproc")
        );
        assert_eq!(
            bridge("video/x-raw(memory:GLMemory), format=RGBA"),
            Some("gldownload")
        );
        assert_eq!(bridge("video/x-raw, format=NV12"), None);
        assert_eq!(bridge("video/x-h264, stream-format=avc"), None);
    }
}
