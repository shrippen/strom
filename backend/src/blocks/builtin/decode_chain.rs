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

/// Whether `caps` say the video is progressive.
///
/// No `interlace-mode` field means progressive, as in GStreamer's own video
/// info. No caps at all is not an answer: the caller cannot know yet.
fn is_progressive(caps: Option<&gst::CapsRef>) -> bool {
    let Some(structure) = caps.and_then(|c| c.structure(0)) else {
        return false;
    };
    match structure.get::<&str>("interlace-mode") {
        Ok(mode) => mode == "progressive",
        Err(gst::structure::GetError::FieldNotFound { .. }) => true,
        Err(_) => false,
    }
}

/// Link a decoded raw-video source pad into the block's video output
/// `identity`, deinterlacing only when the stream may need it.
///
/// `deinterlace` runs on the CPU and takes system memory only, so a decoder in
/// front of it (`nvh264dec`) downloads every frame - even in passthrough, for a
/// progressive stream. When `raw_src` already has progressive caps, which a
/// `decodebin` pad has when it is added, the pad goes straight to `identity`
/// and the decoder can keep its frames in GPU memory for the consumer to adapt
/// (see CLAUDE.md, GStreamer Memory Formats). Interlaced caps, and a pad with
/// no caps yet (an explicit decoder before data flows), go through
/// [`link_raw_video_through_deinterlace`] as before.
///
/// A stream that turns interlaced on the same pad later is not deinterlaced; a
/// warning says so, and restarting the flow picks `deinterlace` again.
pub(crate) fn link_raw_video(
    bin: &gst::Bin,
    raw_src: &gst::Pad,
    identity: &gst::Element,
    instance_id: &str,
    pad_label: &str,
) -> Result<(), String> {
    if !is_progressive(raw_src.current_caps().as_deref()) {
        return link_raw_video_through_deinterlace(bin, raw_src, identity, instance_id, pad_label);
    }

    let identity_sink = identity
        .static_pad("sink")
        .ok_or("identity has no sink pad")?;
    // EVENT probe: fires per event, never per buffer. Captures nothing but an
    // owned string.
    let label = format!("{}:{}", instance_id, pad_label);
    identity_sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::PadProbeData::Event(event)) = &info.data {
            if let gst::EventView::Caps(caps_event) = event.view() {
                if !is_progressive(Some(caps_event.caps())) {
                    warn!(
                        "{}: the stream turned interlaced after it started progressive, and is \
                         not deinterlaced. Restart the flow to deinterlace it",
                        label
                    );
                    return gst::PadProbeReturn::Remove;
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
    raw_src
        .link(&identity_sink)
        .map(|_| ())
        .map_err(|e| format!("link pad -> identity: {:?}", e))?;
    debug!(
        "{}: progressive video, linked {} straight to its output (no deinterlace)",
        instance_id, pad_label
    );
    Ok(())
}

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
/// event is delivered to the new peer (same pattern as `gst::memory_bridge`).
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
    let name_prefix = format!("{}:deinterlace_{}", instance_id, pad_label);

    // Interlaced caps known already (a `decodebin` pad): splice the chain
    // right away.
    if let Some(caps) = raw_src.current_caps() {
        if !is_progressive(Some(&caps)) {
            raw_src
                .link(&identity_sink)
                .map_err(|e| format!("link pad -> identity: {:?}", e))?;
            return splice_deinterlace(bin, raw_src, &caps, &name_prefix);
        }
    }

    // Probe before linking, so it sees the first CAPS event.
    let bin_weak = bin.downgrade();
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

    /// A bin with the block's output `identity`, and a source pad carrying
    /// `caps` as a sticky event, the way a `decodebin` pad does when added.
    fn setup(caps: Option<&str>) -> (gst::Bin, gst::Pad, gst::Element) {
        gst::init().unwrap();
        let bin = gst::Bin::new();
        let identity = gst::ElementFactory::make("identity").build().unwrap();
        bin.add(&identity).unwrap();
        let src = gst::Pad::builder(gst::PadDirection::Src)
            .name("src_0")
            .build();
        src.set_active(true).unwrap();
        if let Some(caps) = caps {
            // Sticky events are stored on the pad even while it is unlinked.
            src.push_event(gst::event::StreamStart::new("test"));
            src.push_event(gst::event::Caps::new(&gst::Caps::from_str(caps).unwrap()));
            assert!(src.current_caps().is_some());
        }
        (bin, src, identity)
    }

    fn has_deinterlace(bin: &gst::Bin) -> bool {
        bin.children()
            .iter()
            .any(|e| e.factory().is_some_and(|f| f.name() == "deinterlace"))
    }

    fn peer_element(pad: &gst::Pad) -> Option<gst::Element> {
        pad.peer().and_then(|p| p.parent_element())
    }

    /// The fix: a progressive pad goes straight to the output, so nothing on
    /// the CPU sits between a GPU decoder and the consumer.
    #[test]
    fn progressive_video_skips_deinterlace() {
        for caps in [
            "video/x-raw, format=NV12, width=1280, height=720, interlace-mode=progressive",
            "video/x-raw(memory:CUDAMemory), format=NV12, interlace-mode=progressive",
            // No interlace-mode field means progressive.
            "video/x-raw, format=NV12, width=1280, height=720",
        ] {
            let (bin, src, identity) = setup(Some(caps));
            link_raw_video(&bin, &src, &identity, "test", "src_0").unwrap();
            assert!(!has_deinterlace(&bin), "{}", caps);
            assert_eq!(peer_element(&src).as_ref(), Some(&identity), "{}", caps);
        }
    }

    #[test]
    fn interlaced_video_is_deinterlaced() {
        for mode in ["interleaved", "mixed"] {
            let caps = format!("video/x-raw, format=NV12, interlace-mode={}", mode);
            let (bin, src, identity) = setup(Some(&caps));
            link_raw_video(&bin, &src, &identity, "test", "src_0").unwrap();
            assert!(has_deinterlace(&bin), "{}", mode);
            assert_ne!(peer_element(&src).as_ref(), Some(&identity), "{}", mode);
        }
    }
}
