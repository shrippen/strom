//! Which of the vision mixer's border underlay pads have a configured zone
//! border, and in which color. See `crate::gst::underlay` for what that
//! decides: a pad with a configured border holds its frame even while it is
//! hidden, so a take that reveals the border shows it on the cut frame; the
//! rest hold no frame and cost the mixers nothing.

use super::overlay::VisionMixerOverlayState;
use crate::gst::underlay;
use gstreamer as gst;
use gstreamer::prelude::*;

/// The border color of input `input` in PiP `pip`, if a zone of that PiP
/// holding the input has a visible border.
fn border_argb(state: &VisionMixerOverlayState, pip: usize, input: usize) -> Option<u32> {
    state
        .pip_zones(pip)
        .iter()
        .filter(|z| z.sources.contains(&input))
        .find_map(|z| z.border.as_ref().and_then(|b| b.resolved()))
        .map(|b| b.argb)
}

/// Border color for an underlay in a region that can show any PiP (PGM or
/// the PVW big display): the PiP shown there now, then `next` (the PiP a
/// take would bring in), then the lowest PiP with a border on the input.
fn shared_region_argb(
    state: &VisionMixerOverlayState,
    shown: Option<usize>,
    next: Option<usize>,
    input: usize,
) -> Option<u32> {
    shown
        .into_iter()
        .chain(next)
        .chain(0..state.num_pips)
        .find_map(|pip| border_argb(state, pip, input))
}

/// The preview output's compositor beside `mv_comp`, if the mixer has one
/// (`builder::preview_out`).
fn preview_compositor(mv_comp: &gst::Element) -> Option<gst::Element> {
    let name = mv_comp.name();
    let block = name.strip_suffix(":mv_comp")?;
    mv_comp
        .parent()?
        .downcast::<gst::Bin>()
        .ok()?
        .by_name(&format!("{}:pvw_comp", block))
}

/// Every underlay pad as (compositor, sink index, configured border color).
fn underlay_pads(
    state: &VisionMixerOverlayState,
    mixer: &gst::Element,
    mv_comp: &gst::Element,
) -> Vec<(gst::Element, usize, Option<u32>)> {
    let mut pads = Vec::new();
    let (pgm_pip, pvw_pip) = (state.pgm_pip(), state.pvw_pip());
    let pvw_comp = preview_compositor(mv_comp);
    for input in 0..state.num_inputs {
        if let Some(base) = state.dist_underlay_base() {
            let argb = shared_region_argb(state, pgm_pip, pvw_pip, input);
            pads.push((mixer.clone(), base + input, argb));
        }
        if let Some(base) = state.mv_pvw_underlay_base() {
            let argb = shared_region_argb(state, pvw_pip, None, input);
            pads.push((mv_comp.clone(), base + input, argb));
            // The preview output shows what the PVW big display shows.
            if let Some(pvw_comp) = &pvw_comp {
                pads.push((pvw_comp.clone(), state.num_inputs + input, argb));
            }
        }
        for pip in 0..state.num_pips {
            if let Some(base) = state.mv_pip_underlay_base(pip) {
                pads.push((
                    mv_comp.clone(),
                    base + input,
                    border_argb(state, pip, input),
                ));
            }
        }
    }
    pads
}

fn sink_pad(compositor: &gst::Element, idx: usize) -> Option<gst::Pad> {
    compositor.static_pad(&format!("sink_{}", idx))
}

/// Register every underlay pad (from the element setup, once linked) and
/// apply the initial zones.
pub(crate) fn watch(state: &VisionMixerOverlayState, mixer: &gst::Element, mv_comp: &gst::Element) {
    for (compositor, idx, _) in underlay_pads(state, mixer, mv_comp) {
        if let Some(pad) = sink_pad(&compositor, idx) {
            underlay::watch_underlay_pad(&pad);
        }
    }
    refresh(state, mixer, mv_comp);
}

/// Re-derive every underlay pad's configured border from the current zones
/// (after a zone or PiP selection change). A newly configured border gets
/// its color and a frame; a removed one stops holding its frame once its
/// pad is hidden.
pub(crate) fn refresh(
    state: &VisionMixerOverlayState,
    mixer: &gst::Element,
    mv_comp: &gst::Element,
) {
    for (compositor, idx, argb) in underlay_pads(state, mixer, mv_comp) {
        let Some(pad) = sink_pad(&compositor, idx) else {
            continue;
        };
        // Color first, so a pad that starts holding gets its frame in it.
        if let Some(argb) = argb {
            underlay::set_underlay_color(&pad, argb);
        }
        underlay::set_underlay_configured(&pad, argb.is_some());
    }
}
