//! Shared helpers for decoded-video chains in source blocks.
//!
//! Source blocks that decode H.264 to raw video feed the GL-based vision mixer,
//! which rejects interlaced frames at `glupload`. Interlaced broadcast feeds
//! (e.g. 1080i50) must therefore be deinterlaced in the source.
//!
//! Progressive feeds must not pass a deinterlacer at all: `deinterlace` only
//! handles system memory, so even in passthrough it forces a hardware
//! decoder's GPU frames (CUDA, VA) into system memory — a full download per
//! frame on the hot path.

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, info, warn};

/// Build the highest-ranked H.264 decoder on this host, as `decodebin` would
/// pick it: NVDEC on NVIDIA, VA on AMD/Intel (strom promotes the VA ranks
/// at startup), a software decoder elsewhere. Falls through to the next
/// candidate if one cannot be instantiated (e.g. no device).
#[cfg(feature = "efp")]
pub(crate) fn make_h264_decoder(name: &str) -> Result<gst::Element, String> {
    let caps = gst::Caps::builder("video/x-h264").build();
    let decoders = gst::ElementFactory::factories_with_type(
        gst::ElementFactoryType::DECODER | gst::ElementFactoryType::MEDIA_VIDEO,
        gst::Rank::MARGINAL,
    );
    // Sorted by decreasing rank.
    let candidates = decoders.into_iter().filter(|f| f.can_sink_any_caps(&caps));

    for factory in candidates {
        match factory.create().name(name).build() {
            Ok(decoder) => return Ok(decoder),
            Err(e) => debug!("{}: {} unusable: {}", name, factory.name(), e),
        }
    }
    Err("no usable H.264 decoder".to_string())
}

/// Caps feature of VA-API surfaces (`va` plugin, AMD/Intel).
const VA_MEMORY_FEATURE: &str = "memory:VAMemory";
/// Caps feature of CUDA device memory (`nvcodec`).
const CUDA_MEMORY_FEATURE: &str = "memory:CUDAMemory";
/// Caps feature of GL textures.
const GL_MEMORY_FEATURE: &str = "memory:GLMemory";

/// Link a decoded raw-video source pad into the block's video output `identity`,
/// deinterlacing only when the source is interlaced.
///
/// `raw_src` links straight to `identity`. A one-shot CAPS-event probe then
/// reads the negotiated `interlace-mode`: `interleaved`/`mixed` splice a
/// deinterlace chain in front of `identity` (see [`deinterlace_chain`]);
/// anything else leaves the direct link, so GPU frames stay on the GPU.
///
/// The deinterlacer is forced to `mode=interlaced`: H.264 `mixed` streams
/// report mixed caps without flagging individual buffers as interlaced, so
/// `mode=auto` would silently pass everything through (relabelling to
/// progressive) and leave visible combing.
///
/// Relinking from inside a push-event probe is supported: the triggering
/// event is delivered to the new peer (same pattern as `gst::gl_bridge`).
/// The probe captures only a weak bin reference — no circular-ref leak (see
/// CLAUDE.md). EVENT probes fire infrequently. Because the decision is driven
/// by the CAPS event rather than caps-at-link-time, this works for both
/// decodebin pads (caps known at pad-added) and explicit decoder pads (caps
/// known only once data flows).
///
/// `pad_label` is used only to build unique element names.
pub(crate) fn link_raw_video_through_deinterlace(
    bin: &gst::Bin,
    raw_src: &gst::Pad,
    identity: &gst::Element,
    instance_id: &str,
    pad_label: &str,
) -> Result<(), String> {
    let identity_sink = identity
        .static_pad("sink")
        .ok_or("identity has no sink pad")?;

    // Probe before linking, so it sees the first (possibly sticky) CAPS event.
    let bin_weak = bin.downgrade();
    let name_prefix = format!("{}:deinterlace_{}", instance_id, pad_label);
    raw_src.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(event)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };

        let caps = caps_event.caps();
        let interlace_mode = caps
            .structure(0)
            .and_then(|s| s.get::<String>("interlace-mode").ok());
        if !matches!(
            interlace_mode.as_deref(),
            Some("interleaved") | Some("mixed")
        ) {
            debug!(
                "{}: progressive source (interlace-mode={:?}), no deinterlacer",
                name_prefix, interlace_mode
            );
            return gst::PadProbeReturn::Remove;
        }

        let Some(bin) = bin_weak.upgrade() else {
            return gst::PadProbeReturn::Remove;
        };
        if let Err(e) = splice_deinterlace(&bin, pad, caps, &name_prefix) {
            warn!(
                "{}: interlaced source left interlaced, deinterlacer not inserted: {}",
                name_prefix, e
            );
        }
        gst::PadProbeReturn::Remove
    });

    raw_src
        .link(&identity_sink)
        .map_err(|e| format!("link pad -> identity: {:?}", e))?;
    Ok(())
}

/// One element of a deinterlace chain; `caps` makes it a capsfilter.
type Step = (&'static str, Option<&'static str>);

/// RGBA in CUDA memory: `glcolorconvert` rejects interlaced input, so the
/// conversion for `gldeinterlace` happens in CUDA.
const CUDA_RGBA: &str = "video/x-raw(memory:CUDAMemory),format=RGBA";
/// `cudadownload` copies into GL on the GPU when asked for GL memory.
const GL_RGBA: &str = "video/x-raw(memory:GLMemory),format=RGBA";

/// Deinterlace chain for frames in `caps`' memory.
///
/// VA surfaces stay on the GPU (`vadeinterlace`, unverified: no AMD/Intel
/// test hardware yet). There is no CUDA deinterlacer in GStreamer; CUDA
/// frames go to GL on the GPU and through `gldeinterlace` (greedyh):
/// measured ~0% CPU vs ~24% of a core for download + yadif per 1080i
/// stream. Unlike yadif it keeps the frame rate (no field doubling). GL
/// frames are downloaded for yadif (glcolorconvert rejects interlaced
/// caps). Only interlaced sources get a chain at all.
fn deinterlace_chain(caps: &gst::CapsRef) -> Vec<Step> {
    let has = |feature: &str| caps.features(0).is_some_and(|f| f.contains(feature));
    if has(VA_MEMORY_FEATURE) {
        return vec![("vadeinterlace", None)];
    }
    if has(CUDA_MEMORY_FEATURE) {
        return vec![
            ("cudaconvert", None),
            ("capsfilter", Some(CUDA_RGBA)),
            ("cudadownload", None),
            ("capsfilter", Some(GL_RGBA)),
            ("gldeinterlace", None),
        ];
    }
    if has(GL_MEMORY_FEATURE) {
        return vec![("gldownload", None), ("deinterlace", None)];
    }
    vec![("deinterlace", None)]
}

/// Insert the deinterlace chain between `src_pad` and its current peer.
fn splice_deinterlace(
    bin: &gst::Bin,
    src_pad: &gst::Pad,
    caps: &gst::CapsRef,
    name_prefix: &str,
) -> Result<(), String> {
    let peer = src_pad.peer().ok_or("source pad has no peer")?;

    let mut chain = Vec::new();
    for (i, (factory, filter)) in deinterlace_chain(caps).into_iter().enumerate() {
        let element = gst::ElementFactory::make(factory)
            .name(format!("{}_{}{}", name_prefix, factory, i))
            .build()
            .map_err(|e| format!("{}: {}", factory, e))?;
        if let Some(filter) = filter {
            let filter: gst::Caps = filter
                .parse()
                .map_err(|e| format!("caps {}: {}", filter, e))?;
            element.set_property("caps", filter);
        }
        if element.find_property("mode").is_some() {
            element.set_property_from_str("mode", "interlaced");
        }
        // High-quality motion-adaptive method; the default `linear` softens
        // detail and leaves residual artifacts on broadcast content.
        if factory == "deinterlace" {
            element.set_property_from_str("method", "yadif");
        }
        if factory == "gldeinterlace" {
            element.set_property_from_str("method", "greedyh");
        }
        chain.push(element);
    }

    bin.add_many(&chain)
        .map_err(|e| format!("add deinterlace chain: {}", e))?;
    gst::Element::link_many(&chain).map_err(|e| format!("link deinterlace chain: {}", e))?;

    let first_sink = chain[0].static_pad("sink").ok_or("chain has no sink pad")?;
    let last_src = chain[chain.len() - 1]
        .static_pad("src")
        .ok_or("chain has no src pad")?;

    // Unlink before linking: the peer accepts one upstream link only.
    src_pad
        .unlink(&peer)
        .map_err(|e| format!("unlink from {}: {}", peer.name(), e))?;
    last_src
        .link(&peer)
        .map_err(|e| format!("link chain -> {}: {:?}", peer.name(), e))?;
    src_pad
        .link(&first_sink)
        .map_err(|e| format!("link -> chain: {:?}", e))?;

    // State last, so the chain negotiates against a complete topology.
    for element in &chain {
        element
            .sync_state_with_parent()
            .map_err(|e| format!("sync {}: {}", element.name(), e))?;
    }

    info!(
        "{}: interlaced source, inserted {}",
        name_prefix,
        chain
            .iter()
            .map(|e| e
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default())
            .collect::<Vec<_>>()
            .join(" ! ")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn caps(s: &str) -> gst::Caps {
        let _ = gst::init();
        gst::Caps::from_str(s).expect("valid caps")
    }

    #[test]
    #[cfg(feature = "efp")]
    fn h264_decoder_is_the_highest_ranked() {
        let _ = gst::init();
        let decoder = make_h264_decoder("dec").expect("some H.264 decoder");
        let factory = decoder.factory().unwrap();

        let caps = gst::Caps::builder("video/x-h264").build();
        let best = gst::ElementFactory::factories_with_type(
            gst::ElementFactoryType::DECODER | gst::ElementFactoryType::MEDIA_VIDEO,
            gst::Rank::MARGINAL,
        )
        .into_iter()
        .filter(|f| f.can_sink_any_caps(&caps))
        .map(|f| f.rank())
        .max()
        .unwrap();
        assert_eq!(factory.rank(), best, "picked {}", factory.name());
    }

    #[test]
    fn deinterlacer_follows_the_memory_type() {
        let chain = |s: &str| {
            deinterlace_chain(&caps(s))
                .into_iter()
                .map(|(factory, _)| factory)
                .collect::<Vec<_>>()
        };

        assert_eq!(chain("video/x-raw,format=NV12"), ["deinterlace"]);
        assert_eq!(
            chain("video/x-raw(memory:VAMemory),format=NV12"),
            ["vadeinterlace"]
        );
        assert_eq!(
            chain("video/x-raw(memory:CUDAMemory),format=NV12"),
            [
                "cudaconvert",
                "capsfilter",
                "cudadownload",
                "capsfilter",
                "gldeinterlace"
            ]
        );
        assert_eq!(
            chain("video/x-raw(memory:GLMemory),format=RGBA"),
            ["gldownload", "deinterlace"]
        );
    }

    /// Push `interlace_mode` caps through the helper and report which
    /// elements sit between source and identity afterwards.
    fn chain_after_caps(interlace_mode: &str) -> Vec<String> {
        let _ = gst::init();
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 2i32)
            .build()
            .unwrap();
        let filter = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                caps(&format!(
                    "video/x-raw,format=I420,width=64,height=64,interlace-mode={}",
                    interlace_mode
                )),
            )
            .build()
            .unwrap();
        let identity = gst::ElementFactory::make("identity").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        pipeline
            .add_many([&src, &filter, &identity, &sink])
            .unwrap();
        src.link(&filter).unwrap();
        identity.link(&sink).unwrap();

        let raw_src = filter.static_pad("src").unwrap();
        link_raw_video_through_deinterlace(
            pipeline.upcast_ref(),
            &raw_src,
            &identity,
            "test",
            "v0",
        )
        .unwrap();

        pipeline.set_state(gst::State::Playing).unwrap();
        let bus = pipeline.bus().unwrap();
        let _ = bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(5),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        let upstream_of_identity = identity
            .static_pad("sink")
            .unwrap()
            .peer()
            .and_then(|p| p.parent_element())
            .and_then(|e| e.factory())
            .map(|f| f.name().to_string())
            .unwrap_or_default();
        pipeline.set_state(gst::State::Null).unwrap();
        vec![upstream_of_identity]
    }

    #[test]
    fn progressive_source_links_directly() {
        assert_eq!(chain_after_caps("progressive"), ["capsfilter"]);
    }

    #[test]
    fn interlaced_source_gets_a_deinterlacer() {
        assert_eq!(chain_after_caps("interleaved"), ["deinterlace"]);
        assert_eq!(chain_after_caps("mixed"), ["deinterlace"]);
    }
}
