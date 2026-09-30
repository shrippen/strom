//! Consumer-side GPU-memory adaptation for video inputs.
//!
//! The convention is that a block emits the memory type it naturally produces
//! and the *consuming* block adapts its own input, so a GPU-to-GPU flow is
//! never forced through a download and re-upload. Most consumers can pick
//! their input front at build time. This module covers the case where they
//! cannot: whether frames arriving on a pad live in GL memory depends on which
//! decoder `decodebin` autoplugged upstream, which is only known once caps are
//! negotiated.
//!
//! [`install_gl_download_bridge`] watches an already-linked source pad and, on
//! the first CAPS event that carries `memory:GLMemory`, splices a `gldownload`
//! between that pad and its peer. Relinking from inside a push-event probe is
//! a supported pattern: `gst_pad_push_event_unchecked` resolves the peer and
//! rechecks pending sticky events *after* running its probes, so the event that
//! triggered the splice lands on the newly inserted element.
//!
//! Only GL memory is bridged. CUDA, NVMM, D3D11 and VA memory are left in
//! place — the consumers that advertise those memory types really do encode
//! them, and downloading would cost a full round trip per frame.
//!
//! [`install_gl_upload_bridge`] is the reverse, for GL consumers: `glupload`
//! takes system, DMABuf and GL memory, but not the CUDA (NVDEC) or VA
//! (AMD/Intel decoders) frames a hardware decoder emits. It splices
//! `cudadownload` (CUDA -> GL copy on the GPU) or `vapostproc` (VA -> DMABuf
//! for zero-copy import; unverified, no VA hardware tested) in front of it.

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{info, warn};

/// The caps feature that marks a buffer as living in GL memory.
const GL_MEMORY_FEATURE: &str = "memory:GLMemory";

/// The caps feature of plain system memory (implicit when absent).
const SYSTEM_MEMORY_FEATURE: &str = "memory:SystemMemory";

/// Hardware-decoder memory `glupload` cannot take, and its bridge element.
const UPLOAD_BRIDGES: [(&str, &str); 2] = [
    ("memory:CUDAMemory", "cudadownload"),
    ("memory:VAMemory", "vapostproc"),
];

/// The element that turns `caps`' frames into something `glupload` takes,
/// or `None` if it takes them as they are.
pub fn upload_bridge_for(caps: &gst::CapsRef) -> Option<&'static str> {
    let structure = caps.structure(0)?;
    if structure.name() != "video/x-raw" {
        return None;
    }
    let features = caps.features(0)?;
    UPLOAD_BRIDGES
        .iter()
        .find(|(feature, _)| features.contains(*feature))
        .map(|(_, bridge)| *bridge)
}

/// Watch `src_pad` (linked towards a `glupload`) and splice the bridge from
/// [`upload_bridge_for`] in front of its peer on the first CAPS event that
/// needs one.
///
/// Upstream only sends that CAPS event if negotiation succeeds, and it asks
/// first: a CAPS query, answered by `glupload`, lists no CUDA or VA memory.
/// A query probe therefore adds the bridgeable memory variants to the
/// answer (and accepts them in ACCEPT_CAPS) until the first CAPS event has
/// been classified; then both probes are removed. Query and event probes
/// fire per negotiation, not per buffer.
pub fn install_gl_upload_bridge(src_pad: &gst::Pad, name_prefix: &str) {
    let name_prefix = name_prefix.to_string();

    let query_probe = src_pad.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, |pad, info| {
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
            gst::QueryViewMut::AcceptCaps(q)
                if !q.result() && upload_bridge_for(q.caps()).is_some() =>
            {
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
        let Some(bridge) = upload_bridge_for(caps_event.caps()) else {
            return gst::PadProbeReturn::Remove;
        };
        if let Err(e) = splice(pad, bridge, &name_prefix) {
            warn!(
                "{}: GPU-memory input could not be bridged to GL ({}), glupload will not negotiate",
                name_prefix, e
            );
        }
        gst::PadProbeReturn::Remove
    });
}

/// `caps` plus a CUDA and a VA memory copy of each system-memory structure
/// (the formats `glupload` can take once bridged), for the bridges whose
/// element exists on this host.
fn with_bridgeable_memory(caps: &gst::Caps) -> gst::Caps {
    let mut out = caps.copy();
    for (feature, bridge) in UPLOAD_BRIDGES {
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
            out.append_structure_full(s.to_owned(), Some(gst::CapsFeatures::new([feature])));
        }
    }
    out
}

/// True when `caps` describe raw video in GL memory.
///
/// Encoded video (`video/x-h264` and friends) is never a candidate: there is
/// nothing to download, and `gldownload` would not even link. Other GPU memory
/// types are not candidates either — see the module docs.
pub fn needs_gl_download(caps: &gst::CapsRef) -> bool {
    let Some(structure) = caps.structure(0) else {
        return false;
    };
    if structure.name() != "video/x-raw" {
        return false;
    }
    caps.features(0)
        .is_some_and(|features| features.contains(GL_MEMORY_FEATURE))
}

/// Watch `src_pad` and insert a `gldownload` in front of its peer if the
/// negotiated caps turn out to carry GL memory.
///
/// The probe is one-shot: it removes itself as soon as the first CAPS event has
/// been classified, whichever way it went, so nothing is left behind on the
/// data path. `name_prefix` names the inserted element and is only used for
/// logging and debugging.
pub fn install_gl_download_bridge(src_pad: &gst::Pad, name_prefix: &str) {
    let name_prefix = name_prefix.to_string();

    // EVENT_DOWNSTREAM, not BLOCK/BUFFER: this fires per event, not per buffer.
    src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(ref event)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };

        if !needs_gl_download(caps_event.caps()) {
            // System memory, another GPU memory type, or already-encoded video:
            // nothing to do, and nothing to keep watching for.
            return gst::PadProbeReturn::Remove;
        }

        // Remove the probe on every path from here on: the splice either
        // succeeds, or it failed for a reason that retrying will not fix.
        if let Err(e) = splice(pad, "gldownload", &name_prefix) {
            warn!(
                "{}: GL-memory input could not be bridged ({}), leaving it in GL memory — a downstream encoder may not accept it",
                name_prefix, e
            );
        }
        gst::PadProbeReturn::Remove
    });
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
    fn foreign_gpu_memory_needs_an_upload_bridge() {
        assert_eq!(
            upload_bridge_for(&caps("video/x-raw(memory:CUDAMemory), format=NV12")),
            Some("cudadownload")
        );
        assert_eq!(
            upload_bridge_for(&caps("video/x-raw(memory:VAMemory), format=NV12")),
            Some("vapostproc")
        );
    }

    /// glupload takes these itself.
    #[test]
    fn uploadable_memory_needs_no_bridge() {
        for c in [
            "video/x-raw, format=NV12",
            "video/x-raw(memory:GLMemory), format=RGBA",
            "video/x-raw(memory:DMABuf), format=DMA_DRM",
            "video/x-h264, stream-format=avc",
        ] {
            assert_eq!(upload_bridge_for(&caps(c)), None, "{}", c);
        }
    }

    /// Run with `cargo test -- --ignored` on a host with NVIDIA CUDA and GL.
    #[test]
    #[ignore = "needs NVIDIA CUDA and hardware GL"]
    fn cuda_frames_reach_glupload() {
        let _ = gst::init();
        // Link queue -> glupload before the CUDA source, as strom's linker
        // does: caps are unknown when the mixer's input chain is linked.
        let make = |factory: &str| gst::ElementFactory::make(factory).build().unwrap();
        let (src, upload, cuda_caps) =
            (make("videotestsrc"), make("cudaupload"), make("capsfilter"));
        let (queue, glupload, convert, gl_caps, sink) = (
            make("queue"),
            make("glupload"),
            make("glcolorconvert"),
            make("capsfilter"),
            make("fakesink"),
        );
        src.set_property("num-buffers", 5i32);
        cuda_caps.set_property("caps", caps("video/x-raw(memory:CUDAMemory)"));
        gl_caps.set_property("caps", caps("video/x-raw(memory:GLMemory),format=RGBA"));

        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([
                &src, &upload, &cuda_caps, &queue, &glupload, &convert, &gl_caps, &sink,
            ])
            .unwrap();
        gst::Element::link_many([&queue, &glupload, &convert, &gl_caps, &sink]).unwrap();
        install_gl_upload_bridge(&queue.static_pad("src").unwrap(), "q");
        gst::Element::link_many([&src, &upload, &cuda_caps]).unwrap();
        // Unchecked, like a link made before the source knows its caps.
        cuda_caps
            .static_pad("src")
            .unwrap()
            .link_full(
                &queue.static_pad("sink").unwrap(),
                gst::PadLinkCheck::empty(),
            )
            .unwrap();

        pipeline.set_state(gst::State::Playing).unwrap();
        let msg = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("pipeline neither finished nor failed");
        pipeline.set_state(gst::State::Null).unwrap();

        assert_eq!(msg.type_(), gst::MessageType::Eos, "{:?}", msg);
    }

    #[test]
    fn gl_memory_raw_video_needs_a_download() {
        assert!(needs_gl_download(&caps(
            "video/x-raw(memory:GLMemory), format=NV12, width=1280, height=720"
        )));
    }

    #[test]
    fn system_memory_raw_video_does_not() {
        assert!(!needs_gl_download(&caps(
            "video/x-raw, format=NV12, width=1280, height=720"
        )));
    }

    /// The consumers that advertise these memory types encode them directly.
    /// Downloading would cost a GPU round trip per frame for nothing.
    #[test]
    fn other_gpu_memory_types_are_left_alone() {
        for feature in [
            "memory:CUDAMemory",
            "memory:NVMM",
            "memory:D3D11Memory",
            "memory:VAMemory",
            "memory:DMABuf",
        ] {
            let c = caps(&format!("video/x-raw({}), format=NV12", feature));
            assert!(
                !needs_gl_download(&c),
                "{} should not be downloaded",
                feature
            );
        }
    }

    /// WHEP Output also takes pre-encoded video. gldownload cannot even link
    /// to it, so it must never be spliced in.
    #[test]
    fn encoded_video_is_not_a_candidate() {
        for c in [
            "video/x-h264, stream-format=avc, alignment=au",
            "video/x-h265",
            "video/x-vp9",
            "video/x-av1",
        ] {
            assert!(
                !needs_gl_download(&caps(c)),
                "{} should be passed through",
                c
            );
        }
    }

    #[test]
    fn audio_and_capsless_caps_are_not_candidates() {
        assert!(!needs_gl_download(&caps("audio/x-raw, rate=48000")));
        assert!(!needs_gl_download(&caps("ANY")));
        assert!(!needs_gl_download(&caps("EMPTY")));
    }
}
