//! Dynamic inputs: inputs added to and removed from a running vision mixer
//! without rebuilding it (CPU and CUDA backends).
//!
//! A mixer with `max_inputs` above `num_inputs` reserves compositor pad
//! indices for every slot (see `PadMap`), so a dynamic input gets the same
//! pad names a static input at that index would get, and nothing else moves.
//! Its chain is built by the same functions as a static input's
//! (`pipeline_cpu::append_input_elements` and the `link_*` functions,
//! `preview_out::append_input_branch`), fed by an `intersrc` instead of a
//! flow link:
//!
//! ```text
//! intersrc_i → queue_i → … → tee_i → (branches) → mixer / mv_comp / pvw_comp
//! ```
//!
//! **Add**: the elements go into the pipeline, the compositor pads are
//! requested and set up (hidden, thumbnail in its new tile) before anything
//! is linked, then the elements follow the pipeline's state, sinks first.
//! The other inputs' thumbnails move to the new layout on the next geometry
//! refresh.
//!
//! **Remove**: refused (`Conflict`) while the input is on program or
//! preview, part of a PiP, or a take or fade animates the program pads.
//! Otherwise the slot is freed first (no operation can pick the input any
//! more), the chain is stopped (state `NULL`, source first, so no element
//! pushes into an unlinked pad), the compositor pads are released and the
//! elements leave the pipeline.
//!
//! Dynamic inputs have no audio input and no VU meter. The elements are not
//! entered in the flow's element map: the slot list keeps their names, and
//! nothing keeps a reference to them.

use std::collections::{BTreeMap, HashMap};

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::element::ElementPadRef;
use strom_types::vision_mixer::{
    AddVisionMixerInputRequest, DynamicInput, DynamicInputSource, PadMap,
};
use strom_types::{FlowId, PropertyValue};
use tracing::{info, warn};

use super::builder::{pad_layout, pipeline_cpu, preview_out, tiles_out, InputSpec};
use super::overlay::VisionMixerOverlayState;

/// Buffer time of a dynamic input's `intersrc` when the request names none
/// (the Inter Input block's default).
pub const DEFAULT_MAX_TIME_MS: u64 = 500;

/// The dynamic inputs of one mixer.
#[derive(Default)]
pub(crate) struct DynamicInputs {
    /// What building an input chain needs; `None` on the GL backend, which
    /// has no dynamic inputs yet.
    pub(crate) spec: Option<InputSpec>,
    slots: BTreeMap<usize, Slot>,
    /// Stored inputs that failed to build at the flow's start. They stay
    /// stored (and are tried again at the next start) until their slot
    /// gets an input or they are removed.
    unrestored: BTreeMap<usize, DynamicInput>,
}

struct Slot {
    input: DynamicInput,
    /// Names of the input's elements, in build order (source first).
    elements: Vec<String>,
}

impl DynamicInputs {
    /// The dynamic inputs, ascending by index (what `dynamic_inputs` stores).
    pub(crate) fn inputs(&self) -> Vec<DynamicInput> {
        let mut inputs: Vec<DynamicInput> = self
            .slots
            .values()
            .map(|s| s.input.clone())
            .chain(self.unrestored.values().cloned())
            .collect();
        inputs.sort_by_key(|i| i.index);
        inputs
    }

    /// Stored inputs that failed to build at the flow's start.
    pub(crate) fn unrestored(&self) -> Vec<DynamicInput> {
        self.unrestored.values().cloned().collect()
    }

    /// Keep `input`, which failed to build at the flow's start, stored.
    pub(crate) fn keep_unrestored(&mut self, input: DynamicInput) {
        self.unrestored.insert(input.index, input);
    }

    /// The source of dynamic input `index`, if there is one.
    pub(crate) fn source(&self, index: usize) -> Option<DynamicInputSource> {
        self.slots.get(&index).map(|s| s.input.source.clone())
    }
}

/// Why adding or removing an input failed.
#[derive(Debug, thiserror::Error)]
pub enum DynamicInputError {
    /// The mixer cannot take dynamic inputs (GL backend, no free slots).
    #[error("{0}")]
    NotSupported(String),
    /// The request is malformed (slot out of range, empty channel).
    #[error("{0}")]
    Invalid(String),
    /// The slot is taken, or the input is in use and cannot go now.
    #[error("{0}")]
    Conflict(String),
    /// No such dynamic input.
    #[error("{0}")]
    NotFound(String),
    /// GStreamer refused (element missing, link or state change failed).
    #[error("{0}")]
    Failed(String),
}

/// Add a dynamic input to the running mixer `block_id`. Returns the input
/// as it is stored (with the index it got).
pub(crate) fn add_input(
    pipeline: &gst::Pipeline,
    state: &VisionMixerOverlayState,
    flow_id: FlowId,
    block_id: &str,
    request: AddVisionMixerInputRequest,
) -> Result<DynamicInput, DynamicInputError> {
    let mut dynamic = state
        .dynamic
        .lock()
        .map_err(|_| DynamicInputError::Failed("dynamic input state poisoned".into()))?;
    let spec = dynamic.spec.clone().ok_or_else(|| {
        DynamicInputError::NotSupported(
            "this vision mixer has no dynamic inputs (OpenGL backend)".into(),
        )
    })?;
    let (first, capacity) = (state.num_static, state.capacity);
    if first >= capacity {
        return Err(DynamicInputError::NotSupported(format!(
            "this vision mixer has no free input slots (max_inputs {} = num_inputs); \
             raise max_inputs to add inputs at runtime",
            capacity
        )));
    }
    let index = match request.index {
        Some(i) if i < first || i >= capacity => {
            return Err(DynamicInputError::Invalid(format!(
                "input {} is outside the free slots {}..{}",
                i, first, capacity
            )))
        }
        Some(i) if state.is_active(i) => {
            return Err(DynamicInputError::Conflict(format!(
                "input slot {} is taken",
                i
            )))
        }
        Some(i) => i,
        None => (first..capacity)
            .find(|&i| !state.is_active(i) && !dynamic.unrestored.contains_key(&i))
            .ok_or_else(|| {
                DynamicInputError::Conflict(format!("all {} input slots are taken", capacity))
            })?,
    };
    let input = DynamicInput {
        index,
        source: request.source,
        label: request.label.filter(|l| !l.trim().is_empty()),
    };
    strom_types::vision_mixer::validate_dynamic_inputs(
        std::slice::from_ref(&input),
        first,
        capacity,
    )
    .map_err(DynamicInputError::Invalid)?;

    let mixer = by_name(pipeline, &spec.id("mixer"))?;
    let mv_comp = by_name(pipeline, &spec.id("mv_comp"))?;
    let pvw_comp = spec
        .preview_out
        .then(|| by_name(pipeline, &spec.id("pvw_comp")))
        .transpose()?;

    let (elems, links) = build_chain(&spec, &input).map_err(DynamicInputError::Failed)?;
    let mut inputs = state.active_inputs();
    inputs.push(index);
    inputs.sort_unstable();
    let layout = spec.mv_layout(&inputs);
    let pad_props = pad_layout::dynamic_input_pad_properties(&spec, &layout, index);

    let names: Vec<String> = elems.iter().map(|(id, _)| id.clone()).collect();
    if let Err(e) = assemble(pipeline, &elems, &links, &pad_props) {
        teardown(pipeline, &spec, index, &names);
        return Err(DynamicInputError::Failed(e));
    }

    // Reactive geometry, preview mirror and border underlays, like the
    // static inputs get them at element setup.
    super::geometry::install_caps_probes(flow_id, block_id, &mixer, &mv_comp, &spec.pm, [index]);
    if let Some(pvw_comp) = &pvw_comp {
        preview_out::mirror(
            &mv_comp,
            pvw_comp,
            &spec.pm,
            [index],
            &layout.pvw_rect,
            spec.pgm_w,
            spec.pgm_h,
        );
    }
    if spec.num_pips > 0 {
        super::underlays::watch(state, &mixer, &mv_comp, [index]);
    }

    let label = input
        .label
        .clone()
        .unwrap_or_else(|| format!("In {}", index + 1));
    state.set_label(index, label);
    state.set_active(index, true);
    state.set_layout(layout);

    // Sinks first, so no element pushes into one that is not running yet.
    for (id, element) in elems.iter().rev() {
        if let Err(e) = element.sync_state_with_parent() {
            state.set_active(index, false);
            state.set_layout(spec.mv_layout(&state.active_inputs()));
            teardown(pipeline, &spec, index, &names);
            return Err(DynamicInputError::Failed(format!(
                "{} did not follow the pipeline state: {}",
                id, e
            )));
        }
    }

    dynamic.unrestored.remove(&index);
    dynamic.slots.insert(
        index,
        Slot {
            input: input.clone(),
            elements: names,
        },
    );
    drop(dynamic);
    super::geometry::schedule_relayout(flow_id, block_id, &mixer, &mv_comp);
    super::overlay::trigger_overlay_update(&flow_id, block_id);
    info!(
        "Vision mixer {}: added input {} from {:?}",
        block_id, index, input.source
    );
    Ok(input)
}

/// Remove dynamic input `index` from the running mixer `block_id`.
pub(crate) fn remove_input(
    pipeline: &gst::Pipeline,
    state: &VisionMixerOverlayState,
    flow_id: FlowId,
    block_id: &str,
    index: usize,
) -> Result<DynamicInput, DynamicInputError> {
    let mut dynamic = state
        .dynamic
        .lock()
        .map_err(|_| DynamicInputError::Failed("dynamic input state poisoned".into()))?;
    if index < state.num_static {
        return Err(DynamicInputError::Conflict(format!(
            "input {} is a static input (fed by a flow link) and cannot be removed",
            index
        )));
    }
    let Some(spec) = dynamic.spec.clone() else {
        return Err(DynamicInputError::NotFound(format!(
            "input {} is not a dynamic input",
            index
        )));
    };
    // A stored input that did not build at the start: removing it only
    // takes it off the stored list.
    if let Some(input) = dynamic.unrestored.remove(&index) {
        info!(
            "Vision mixer {}: dropped stored input {} that had not built",
            block_id, index
        );
        return Ok(input);
    }
    if !dynamic.slots.contains_key(&index) {
        return Err(DynamicInputError::NotFound(format!(
            "input {} is not a dynamic input",
            index
        )));
    }
    let mixer = by_name(pipeline, &spec.id("mixer"))?;
    let mv_comp = by_name(pipeline, &spec.id("mv_comp"))?;

    // Free the slot first: from here on no selection or PiP change can
    // pick the input (one that checked it before stores it before the lock
    // is ours). Then check that nothing uses it right now.
    {
        let _selection = state.lock_selection();
        state.set_active(index, false);
        if let Err(e) = check_unused(state, &mixer, &spec.pm, index) {
            state.set_active(index, true);
            return Err(e);
        }
    }
    state.set_layout(spec.mv_layout(&state.active_inputs()));

    let slot = dynamic.slots.remove(&index).expect("checked above");
    teardown(pipeline, &spec, index, &slot.elements);
    state.set_label(index, format!("In {}", index + 1));
    drop(dynamic);

    super::geometry::schedule_relayout(flow_id, block_id, &mixer, &mv_comp);
    super::overlay::trigger_overlay_update(&flow_id, block_id);
    info!("Vision mixer {}: removed input {}", block_id, index);
    Ok(slot.input)
}

/// Refuse to remove `index` while something shows or animates it.
fn check_unused(
    state: &VisionMixerOverlayState,
    mixer: &gst::Element,
    pm: &PadMap,
    index: usize,
) -> Result<(), DynamicInputError> {
    let busy = |what: String| {
        Err(DynamicInputError::Conflict(format!(
            "input {} {}; take it off first",
            index, what
        )))
    };
    if state.pgm_input() == Some(index) {
        return busy("is on program".into());
    }
    if state.pvw_input() == Some(index) {
        return busy("is on preview".into());
    }
    for pip in 0..state.num_pips {
        if state.pip_bg_input(pip) == Some(index)
            || state
                .pip_zones(pip)
                .iter()
                .any(|z| z.sources.contains(&index))
        {
            return busy(format!("is part of PiP {}", pip));
        }
    }
    if let Some(left) = super::geometry::inputs_animation_remaining(mixer, pm) {
        return Err(DynamicInputError::Conflict(format!(
            "a transition is running ({} ms left); remove input {} after it",
            left.as_millis(),
            index
        )));
    }
    Ok(())
}

/// The elements and links of `input`'s chain: its `intersrc`, the chain a
/// static input has, and the links to the compositors.
#[allow(clippy::type_complexity)]
fn build_chain(
    spec: &InputSpec,
    input: &DynamicInput,
) -> Result<
    (
        Vec<(String, gst::Element)>,
        Vec<(ElementPadRef, ElementPadRef)>,
    ),
    String,
> {
    let i = input.index;
    let mut elems = Vec::new();
    let mut links = Vec::new();
    let DynamicInputSource::Inter {
        channel,
        max_time_ms,
    } = &input.source;
    let src_id = spec.id(&format!("intersrc_{}", i));
    let src = gst::ElementFactory::make("intersrc")
        .name(&src_id)
        .property("producer-name", channel)
        .property(
            "max-time",
            max_time_ms
                .unwrap_or(DEFAULT_MAX_TIME_MS)
                .saturating_mul(1_000_000),
        )
        .build()
        .map_err(|e| format!("intersrc: {}", e))?;
    elems.push((src_id.clone(), src));
    links.push((
        ElementPadRef::pad(&src_id, "src"),
        ElementPadRef::pad(spec.id(&format!("queue_{}", i)), "sink"),
    ));

    let err = |e: crate::blocks::BlockBuildError| e.to_string();
    pipeline_cpu::append_input_elements(spec, i, &mut elems, &mut links).map_err(err)?;
    pipeline_cpu::link_dist(spec, i, &mut links);
    pipeline_cpu::link_dist_underlay(spec, i, &mut links);
    pipeline_cpu::link_thumb(spec, i, &mut links);
    pipeline_cpu::link_pvw(spec, i, &mut links);
    for pip in 0..spec.num_pips {
        pipeline_cpu::link_pip(spec, pip, i, &mut links);
    }
    pipeline_cpu::link_mv_pvw_underlay(spec, i, &mut links);
    for pip in 0..spec.num_pips {
        pipeline_cpu::link_mv_pip_underlay(spec, pip, i, &mut links);
    }
    if spec.preview_out {
        preview_out::append_input_branch(spec, i, spec.pvw_out_tee_src(), &mut elems, &mut links)
            .map_err(err)?;
        preview_out::append_input_underlay(spec, i, &mut elems, &mut links).map_err(err)?;
    }
    if let Some(grid) = &spec.tiles {
        tiles_out::append_input_branch(
            spec,
            grid,
            i,
            spec.tiles_out_tee_src(),
            &mut elems,
            &mut links,
        )
        .map_err(err)?;
    }
    Ok((elems, links))
}

/// Put the chain into the pipeline: add the elements, request and set up
/// the compositor pads, link. Nothing runs yet (the elements are in NULL).
fn assemble(
    pipeline: &gst::Pipeline,
    elems: &[(String, gst::Element)],
    links: &[(ElementPadRef, ElementPadRef)],
    pad_props: &[(String, String, HashMap<String, PropertyValue>)],
) -> Result<(), String> {
    let local: HashMap<&str, &gst::Element> =
        elems.iter().map(|(id, e)| (id.as_str(), e)).collect();
    for (id, element) in elems {
        pipeline
            .add(element)
            .map_err(|e| format!("cannot add {}: {}", id, e))?;
    }
    let props: HashMap<(&str, &str), &HashMap<String, PropertyValue>> = pad_props
        .iter()
        .map(|(comp, pad, p)| ((comp.as_str(), pad.as_str()), p))
        .collect();
    for (from, to) in links {
        let src_el = element(pipeline, &local, &from.element_id)?;
        let sink_el = element(pipeline, &local, &to.element_id)?;
        let src_pad = pad(&src_el, from.pad_name.as_deref().unwrap_or("src"))?;
        let sink_name = to.pad_name.as_deref().unwrap_or("sink");
        let sink_pad = pad(&sink_el, sink_name)?;
        if let Some(props) = props.get(&(to.element_id.as_str(), sink_name)) {
            apply_pad_props(&sink_pad, props)?;
        }
        src_pad.link(&sink_pad).map_err(|e| {
            format!(
                "cannot link {}:{} to {}:{}: {:?}",
                from.element_id,
                src_pad.name(),
                to.element_id,
                sink_name,
                e
            )
        })?;
    }
    Ok(())
}

/// Stop and remove the chain of input `index`: elements to NULL (source
/// first), compositor pads released, elements out of the pipeline. Missing
/// elements and pads are skipped, so this also undoes a half-built add.
fn teardown(pipeline: &gst::Pipeline, spec: &InputSpec, index: usize, names: &[String]) {
    let elements: Vec<gst::Element> = names.iter().filter_map(|n| pipeline.by_name(n)).collect();
    crate::gst::underlay::quiesce_underlay_sources(&elements);
    for element in &elements {
        if element.set_state(gst::State::Null).is_err() {
            warn!("{}: could not stop", element.name());
        }
    }
    let pm = spec.pm;
    let mut pads: Vec<(String, usize)> = vec![
        (spec.id("mixer"), pm.dist_input(index)),
        (spec.id("mv_comp"), pm.mv_thumb(index)),
        (spec.id("mv_comp"), pm.mv_pvw(index)),
    ];
    for pip in 0..spec.num_pips {
        pads.push((spec.id("mv_comp"), pm.mv_pip(pip, index)));
    }
    if let Some(base) = pm.dist_underlay_base() {
        pads.push((spec.id("mixer"), base + index));
    }
    if let Some(base) = pm.mv_pvw_underlay_base() {
        pads.push((spec.id("mv_comp"), base + index));
    }
    for pip in 0..spec.num_pips {
        if let Some(base) = pm.mv_pip_underlay_base(pip) {
            pads.push((spec.id("mv_comp"), base + index));
        }
    }
    if spec.preview_out {
        pads.push((spec.id("pvw_comp"), pm.pvw_out_input(index)));
        if let Some(base) = pm.pvw_out_underlay_base() {
            pads.push((spec.id("pvw_comp"), base + index));
        }
    }
    if let Some(c) = spec.tiles.as_ref().and_then(|g| g.cell_of(index)) {
        pads.push((spec.id("tiles_comp"), c));
    }
    for (comp_id, idx) in pads {
        let Some(comp) = pipeline.by_name(&comp_id) else {
            continue;
        };
        let Some(pad) = comp.static_pad(&PadMap::sink(idx)) else {
            continue;
        };
        if let Some(peer) = pad.peer() {
            let _ = peer.unlink(&pad);
        }
        comp.release_request_pad(&pad);
    }
    for element in &elements {
        if pipeline.remove(element).is_err() {
            warn!("{}: could not leave the pipeline", element.name());
        }
    }
}

fn by_name(pipeline: &gst::Pipeline, name: &str) -> Result<gst::Element, DynamicInputError> {
    pipeline
        .by_name(name)
        .ok_or_else(|| DynamicInputError::Failed(format!("{} not found", name)))
}

fn element(
    pipeline: &gst::Pipeline,
    local: &HashMap<&str, &gst::Element>,
    id: &str,
) -> Result<gst::Element, String> {
    local
        .get(id)
        .map(|e| (*e).clone())
        .or_else(|| pipeline.by_name(id))
        .ok_or_else(|| format!("{} not found", id))
}

/// A static pad, or a request pad by its name (`src_3` on a tee, `sink_17`
/// on a compositor).
fn pad(element: &gst::Element, name: &str) -> Result<gst::Pad, String> {
    element
        .static_pad(name)
        .or_else(|| element.request_pad_simple(name))
        .ok_or_else(|| format!("{} has no pad {}", element.name(), name))
}

fn apply_pad_props(pad: &gst::Pad, props: &HashMap<String, PropertyValue>) -> Result<(), String> {
    use crate::gst::pipeline::properties::{checked_property_value, set_checked_value};
    if pad.has_property("qos") {
        pad.set_property("qos", true);
    }
    for (name, value) in props {
        // `sizing-policy` exists from GStreamer 1.24 on; skip like the
        // build-time pad properties do.
        let Some(pspec) = pad.find_property(name) else {
            continue;
        };
        let value = checked_property_value(&pspec, value)
            .map_err(|e| format!("{}:{}: {}", pad.name(), name, e))?;
        set_checked_value(pad, name, &value)
            .map_err(|e| format!("{}:{}: {}", pad.name(), name, e))?;
    }
    Ok(())
}
