//! The preview output (`pvw_out`): a compositor at program size that shows
//! what the multiview's PVW rectangle shows.
//!
//! ```text
//!   tee_i.src_{3+P} → queue_to_pvw_out_i → [videocrop_pvw_out_i] → pvw_comp.sink_i
//!   pvw_comp → … → queue_pvw_out
//! ```
//!
//! Every preview operation (an input or a PiP on preview, a take, a crop, a
//! fade) places the multiview's PVW pads inside `pvw_rect`. The preview
//! compositor has one pad per input, fed from the input's tee, and
//! [`mirror`] copies each change of a multiview PVW pad to its twin, scaled
//! from `pvw_rect` to the full frame. So no operation has to know about it.
//! Zone borders: with PiPs, each input also has a border underlay chain to
//! `pvw_comp.sink_{N+i}`, mirrored from the multiview's PVW underlay pad;
//! its colour and hold come from `underlays` like every other underlay.

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::element::ElementPadRef;
use tracing::{debug, info};

use super::super::elements::{self, CompositorBackend};
use super::super::layout::Rect;
use super::PipelineParams;
use crate::blocks::BlockBuildError;

/// Pad geometry, scaled from `pvw_rect` to the output.
const GEOMETRY: [&str; 4] = ["xpos", "ypos", "width", "height"];

/// Pad properties copied as they are.
const COPIED: [&str; 7] = [
    "alpha",
    "zorder",
    "sizing-policy",
    "crop-left",
    "crop-right",
    "crop-top",
    "crop-bottom",
];

/// `videocrop` properties (CPU and CUDA crop with an element upstream of
/// the pad).
const CROP: [&str; 4] = ["left", "right", "top", "bottom"];

/// Build the preview compositor, its output chain and one branch per input
/// from `tee_{i}.src_{tee_src}`. Returns the compositor.
pub(super) fn append(
    p: &PipelineParams,
    tee_src: usize,
    elems: &mut Vec<(String, gst::Element)>,
    links: &mut Vec<(ElementPadRef, ElementPadRef)>,
) -> Result<gst::Element, BlockBuildError> {
    let pvw_comp =
        elements::make_dist_compositor(p.backend, p.latency_ms, p.min_upstream_ms, p.start_time)?;
    let comp_id = p.id("pvw_comp");
    pvw_comp.set_property("name", &comp_id);
    elems.push((comp_id.clone(), pvw_comp.clone()));

    for i in 0..p.num_inputs {
        let q_id = p.id(&format!("queue_to_pvw_out_{}", i));
        elems.push((q_id.clone(), elements::make_queue(&q_id)?));
        links.push((
            ElementPadRef::pad(p.id(&format!("tee_{}", i)), format!("src_{}", tee_src)),
            ElementPadRef::pad(&q_id, "sink"),
        ));
        let last = if p.backend == CompositorBackend::OpenGL {
            // GL crops with pad properties, which the mirror copies.
            q_id
        } else {
            let crop_id = p.id(&format!("videocrop_pvw_out_{}", i));
            elems.push((
                crop_id.clone(),
                elements::make_element("videocrop", &crop_id)?,
            ));
            links.push((
                ElementPadRef::pad(&q_id, "src"),
                ElementPadRef::pad(&crop_id, "sink"),
            ));
            crop_id
        };
        links.push((
            ElementPadRef::pad(&last, "src"),
            ElementPadRef::pad(&comp_id, format!("sink_{}", i)),
        ));
    }

    if p.num_pips > 0 {
        append_underlays(p, &comp_id, elems, links)?;
    }

    let q_out_id = p.id("queue_pvw_out");
    elems.push((q_out_id.clone(), elements::make_queue(&q_out_id)?));
    let cf_id = p.id("capsfilter_pvw");
    let mut chain = vec![comp_id];
    match p.backend {
        CompositorBackend::OpenGL => {
            let q_post_id = p.id("queue_post_pvw");
            elems.push((q_post_id.clone(), elements::make_queue(&q_post_id)?));
            chain.push(q_post_id);
            let caps = if p.gl_download {
                let dl_id = p.id("gldownload_pvw");
                elems.push((dl_id.clone(), elements::make_element("gldownload", &dl_id)?));
                chain.push(dl_id);
                p.pgm_caps()
            } else {
                p.pgm_caps_glmem()
            };
            elems.push((cf_id.clone(), capsfilter(&cf_id, &caps)?));
            chain.push(cf_id);
        }
        CompositorBackend::Software | CompositorBackend::Cuda => {
            elems.push((cf_id.clone(), capsfilter(&cf_id, &p.pgm_caps())?));
            chain.push(cf_id);
            if p.backend == CompositorBackend::Cuda && p.gl_download {
                let dl_id = p.id("cudadownload_pvw");
                elems.push((
                    dl_id.clone(),
                    elements::make_element("cudadownload", &dl_id)?,
                ));
                chain.push(dl_id);
            }
        }
    }
    chain.push(q_out_id);
    for pair in chain.windows(2) {
        links.push((
            ElementPadRef::pad(&pair[0], "src"),
            ElementPadRef::pad(&pair[1], "sink"),
        ));
    }
    info!(
        "Vision mixer {}: preview output at {}x{}",
        p.instance_id, p.pgm_w, p.pgm_h
    );
    Ok(pvw_comp)
}

/// One border underlay chain per input into `pvw_comp.sink_{N+i}`, built
/// like the multiview's (see the backend builders).
fn append_underlays(
    p: &PipelineParams,
    comp_id: &str,
    elems: &mut Vec<(String, gst::Element)>,
    links: &mut Vec<(ElementPadRef, ElementPadRef)>,
) -> Result<(), BlockBuildError> {
    let (live, caps) = p.underlay_source();
    let caps: gst::Caps = caps
        .parse()
        .map_err(|e| BlockBuildError::ElementCreation(format!("underlay caps: {}", e)))?;
    for i in 0..p.num_inputs {
        let name = |part: &str| p.id(&format!("underlay_pvw_out_{}_{}", i, part));
        let src = crate::gst::underlay::make_underlay_src(&name("src"), live)
            .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", name("src"), e)))?;
        let cf = capsfilter(&name("caps"), &caps)?;
        let mut chain = vec![name("src"), name("caps")];
        elems.push((name("src"), src));
        let upload = match p.backend {
            CompositorBackend::OpenGL => {
                if let Some(pad) = cf.static_pad("src") {
                    super::pipeline_gpu::keep_own_memory(&pad);
                }
                Some("glupload")
            }
            CompositorBackend::Cuda => Some(super::pipeline_cpu::CUDA_UPLOAD),
            CompositorBackend::Software => None,
        };
        elems.push((name("caps"), cf));
        if let Some(factory) = upload {
            elems.push((
                name("upload"),
                elements::make_element(factory, &name("upload"))?,
            ));
            chain.push(name("upload"));
        }
        for pair in chain.windows(2) {
            links.push((
                ElementPadRef::pad(&pair[0], "src"),
                ElementPadRef::pad(&pair[1], "sink"),
            ));
        }
        links.push((
            ElementPadRef::pad(chain.last().expect("chain"), "src"),
            ElementPadRef::pad(comp_id, format!("sink_{}", p.num_inputs + i)),
        ));
    }
    Ok(())
}

fn capsfilter(id: &str, caps: &gst::Caps) -> Result<gst::Element, BlockBuildError> {
    gst::ElementFactory::make("capsfilter")
        .name(id)
        .property("caps", caps)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", id, e)))
}

/// Where a multiview PVW pad's rectangle lands on the preview output.
#[derive(Clone, Copy, Debug)]
struct Scale {
    x: f64,
    y: f64,
    sx: f64,
    sy: f64,
}

impl Scale {
    fn new(pvw_rect: &Rect, out_w: u32, out_h: u32) -> Self {
        Self {
            x: pvw_rect.x,
            y: pvw_rect.y,
            sx: f64::from(out_w) / pvw_rect.w.max(1.0),
            sy: f64::from(out_h) / pvw_rect.h.max(1.0),
        }
    }

    /// `value` of `property` on the multiview pad, on the preview pad.
    fn apply(&self, property: &str, value: i32) -> Option<i32> {
        let v = f64::from(value);
        let scaled = match property {
            "xpos" => (v - self.x) * self.sx,
            "ypos" => (v - self.y) * self.sy,
            "width" => v * self.sx,
            "height" => v * self.sy,
            _ => return None,
        };
        Some(scaled.round() as i32)
    }
}

/// Copy `property` of the multiview pad `from` to the preview pad `to`.
fn copy_pad_property(from: &gst::Pad, to: &gst::Pad, property: &str, scale: Scale) {
    if to.find_property(property).is_none() {
        return;
    }
    if GEOMETRY.contains(&property) {
        if let Some(v) = scale.apply(property, from.property::<i32>(property)) {
            to.set_property(property, v);
        }
    } else if COPIED.contains(&property) {
        to.set_property_from_value(property, &from.property_value(property));
    }
}

/// The `videocrop` right upstream of `pad`, if any.
fn upstream_crop(pad: &gst::Pad) -> Option<gst::Element> {
    let element = pad.peer()?.parent_element()?;
    (element.factory()?.name() == "videocrop").then_some(element)
}

/// Make the preview compositor follow the multiview's PVW pads (from the
/// element setup, once the request pads exist): each input's PVW pad, and
/// its border underlay pad from `mv_underlay_base` on. `pvw_rect` is the
/// multiview's PVW rectangle, `out_w`×`out_h` the preview output's size.
pub(super) fn mirror(
    mv_comp: &gst::Element,
    pvw_comp: &gst::Element,
    num_inputs: usize,
    mv_underlay_base: Option<usize>,
    pvw_rect: &Rect,
    out_w: u32,
    out_h: u32,
) {
    let scale = Scale::new(pvw_rect, out_w, out_h);
    let inputs = (0..num_inputs).map(|i| (num_inputs + 1 + i, i));
    let underlays = mv_underlay_base
        .into_iter()
        .flat_map(|base| (0..num_inputs).map(move |i| (base + i, num_inputs + i)));
    for (mv_idx, pvw_idx) in inputs.chain(underlays) {
        let (Some(from), Some(to)) = (
            mv_comp.static_pad(&format!("sink_{}", mv_idx)),
            pvw_comp.static_pad(&format!("sink_{}", pvw_idx)),
        ) else {
            debug!("preview output: no pad pair {} -> {}", mv_idx, pvw_idx);
            continue;
        };
        for property in GEOMETRY.iter().chain(COPIED.iter()) {
            copy_pad_property(&from, &to, property, scale);
        }
        // Weak: the multiview pad owns this handler, the preview pad must
        // not be kept alive by it.
        let to_weak = to.downgrade();
        from.connect_notify(None, move |from, pspec| {
            if let Some(to) = to_weak.upgrade() {
                copy_pad_property(from, &to, pspec.name(), scale);
            }
        });

        let (Some(crop_from), Some(crop_to)) = (upstream_crop(&from), upstream_crop(&to)) else {
            continue;
        };
        for property in CROP {
            crop_to.set_property_from_value(property, &crop_from.property_value(property));
        }
        let crop_weak = crop_to.downgrade();
        crop_from.connect_notify(None, move |crop_from, pspec| {
            let Some(crop_to) = crop_weak.upgrade() else {
                return;
            };
            if CROP.contains(&pspec.name()) {
                crop_to
                    .set_property_from_value(pspec.name(), &crop_from.property_value(pspec.name()));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pvw_rect_maps_onto_the_whole_output() {
        let rect = Rect {
            x: 960.0,
            y: 30.0,
            w: 960.0,
            h: 540.0,
        };
        let scale = Scale::new(&rect, 1920, 1080);
        assert_eq!(scale.apply("xpos", 960), Some(0));
        assert_eq!(scale.apply("ypos", 30), Some(0));
        assert_eq!(scale.apply("width", 960), Some(1920));
        assert_eq!(scale.apply("height", 540), Some(1080));
        // A PiP insert in the right half of the rect.
        assert_eq!(scale.apply("xpos", 1440), Some(960));
        assert_eq!(scale.apply("alpha", 1), None);
    }
}
