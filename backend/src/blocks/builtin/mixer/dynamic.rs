//! Dynamic aux buses: buses added to and removed from a running mixer
//! without rebuilding it, and sends that change their tap (before or after
//! the fader) while the mixer runs.
//!
//! A dynamic bus is built from the same parts as a static one (see
//! `buses`), under the same element names, so the bus and send properties
//! (`auxN_fader`, `chM_auxN_level`, …) reach it like a static bus. Its
//! output goes to an `intersink` on the bus's channel instead of a block pad:
//!
//! ```text
//! (pre|post)_fader_tee_M → aux_send_M_N → aux_queue_M_N ─┐  (every channel M)
//!                                                         ├→ auxN_mixer → … → auxN_out_tee → auxN_intersink
//!                                          auxN_out_tee → auxN_afl_volume → auxN_afl_queue → solo_mixer
//! ```
//!
//! **Add**: the elements go into the pipeline and are linked among each
//! other and into Solo, then follow the pipeline's state, sinks first. Only
//! then are the sends linked to the channels' tap tees: a tee hands a flow
//! error of one branch (an element not running yet) back to the channel.
//!
//! **Remove**: the sends leave the channels' tees first (a tee releases a
//! pad only once its push is done), then the elements stop, source first,
//! the Solo pad is released and the elements leave the pipeline.
//!
//! **Tap**: a send's tee pad is released and one on the other tee linked.
//! Both tees run on the channel's streaming thread, so the send misses or
//! repeats at most the buffer in between (the bus mixer drops an overlap).

use std::collections::HashMap;

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::element::ElementPadRef;
use strom_types::mixer::{AddMixerAuxBusRequest, DynamicAuxBus, MixerAuxBuses};
use strom_types::PropertyValue;
use tracing::{info, warn};

use super::buses::*;
pub(crate) use super::properties::stored_dynamic_aux_buses;
use super::properties::{
    parse_dynamic_aux_buses, parse_max_aux_buses, parse_num_aux_buses, parse_num_channels,
};
use crate::blocks::builtin::start_time::StartTime;

/// Why adding or removing a bus, or changing a send's tap, failed.
#[derive(Debug, thiserror::Error)]
pub enum AuxBusError {
    /// The mixer has no free bus numbers (`max_aux_buses`).
    #[error("{0}")]
    NotSupported(String),
    /// The request is malformed (number out of range, empty channel).
    #[error("{0}")]
    Invalid(String),
    /// The number or the channel is taken, or a static bus cannot go.
    #[error("{0}")]
    Conflict(String),
    /// No such dynamic bus, or no running mixer.
    #[error("{0}")]
    NotFound(String),
    /// GStreamer refused (element missing, link or state change failed).
    #[error("{0}")]
    Failed(String),
}

/// The aux buses of a mixer with these block properties.
pub fn aux_buses(properties: &HashMap<String, PropertyValue>) -> MixerAuxBuses {
    let num_static = parse_num_aux_buses(properties);
    let capacity = parse_max_aux_buses(properties, num_static);
    let dynamic = parse_dynamic_aux_buses(properties, num_static, capacity)
        .map(|(buses, _)| buses)
        .unwrap_or_default();
    let unbuilt = stored_dynamic_aux_buses(properties)
        .unwrap_or_default()
        .into_iter()
        .filter(|bus| !dynamic.contains(bus))
        .collect();
    MixerAuxBuses {
        capacity,
        num_static,
        dynamic,
        unbuilt,
    }
}

/// The block property values that come with adding a bus as `request`
/// asks: the tap of every channel's send.
pub fn added_bus_properties(
    properties: &HashMap<String, PropertyValue>,
    aux: usize,
    request: &AddMixerAuxBusRequest,
) -> Vec<(String, PropertyValue)> {
    let Some(pre) = request.pre_fader else {
        return Vec::new();
    };
    (0..parse_num_channels(properties))
        .map(|ch| (send_pre_property(ch, aux - 1), PropertyValue::Bool(pre)))
        .collect()
}

/// The bus number `request` gets on a mixer with these block properties:
/// the requested one or the lowest free one. `running` tells which buses
/// the running pipeline has built.
pub fn choose_aux(
    properties: &HashMap<String, PropertyValue>,
    request: &AddMixerAuxBusRequest,
    built: impl Fn(usize) -> bool,
) -> Result<usize, AuxBusError> {
    let buses = aux_buses(properties);
    let (first, capacity) = (buses.num_static + 1, buses.capacity);
    if first > capacity {
        return Err(AuxBusError::NotSupported(format!(
            "this mixer has no free aux buses (max_aux_buses {} = num_aux_buses); \
             raise max_aux_buses to add buses at runtime",
            capacity
        )));
    }
    // Stored buses that no longer fit (not built) keep their number and
    // channel: they come back when the numbers fit again.
    let stored = stored_dynamic_aux_buses(properties)
        .map_err(|e| AuxBusError::Conflict(format!("{}; fix or remove it to add buses", e)))?;
    if let Some(bus) = stored.iter().find(|b| b.channel == request.channel) {
        return Err(AuxBusError::Conflict(format!(
            "channel {} is taken by aux bus {}",
            request.channel, bus.aux
        )));
    }
    let taken = |aux: usize| stored.iter().any(|b| b.aux == aux) || built(aux);
    let aux = match request.aux {
        Some(aux) if aux < first || aux > capacity => {
            return Err(AuxBusError::Invalid(format!(
                "aux bus {} is outside the free buses {}..={}",
                aux, first, capacity
            )))
        }
        Some(aux) if taken(aux) => {
            return Err(AuxBusError::Conflict(format!("aux bus {} is taken", aux)))
        }
        Some(aux) => aux,
        None => (first..=capacity).find(|&aux| !taken(aux)).ok_or_else(|| {
            AuxBusError::Conflict(format!("all {} aux buses are taken", capacity))
        })?,
    };
    let mut all = buses.dynamic.clone();
    all.push(DynamicAuxBus {
        aux,
        channel: request.channel.clone(),
    });
    strom_types::mixer::validate_dynamic_aux_buses(&all, buses.num_static, capacity).map_err(
        |e| {
            if request.channel.trim().is_empty() {
                AuxBusError::Invalid(e)
            } else {
                AuxBusError::Conflict(e)
            }
        },
    )?;
    Ok(aux)
}

/// Whether the running mixer `instance_id` has built aux bus `aux`
/// (1-based).
pub fn is_built(pipeline: &gst::Pipeline, instance_id: &str, aux: usize) -> bool {
    pipeline
        .by_name(&format!("{}:aux{}_mixer", instance_id, aux - 1))
        .is_some()
}

/// Whether the running mixer `instance_id` has built aux bus `aux`
/// (1-based) as a dynamic bus: one with its output on an inter channel.
pub fn is_built_dynamic(pipeline: &gst::Pipeline, instance_id: &str, aux: usize) -> bool {
    pipeline
        .by_name(&format!("{}:{}", instance_id, aux_intersink_id(aux - 1)))
        .is_some()
}

/// The inter channel dynamic aux bus `aux` (1-based) of the running mixer
/// `instance_id` publishes on.
pub fn bus_channel(pipeline: &gst::Pipeline, instance_id: &str, aux: usize) -> Option<String> {
    let sink = pipeline.by_name(&format!("{}:{}", instance_id, aux_intersink_id(aux - 1)))?;
    Some(sink.property::<String>("producer-name"))
}

/// The channels the running mixer `instance_id` has: the stored
/// `num_channels` may have changed since it was built (it takes effect on
/// the next start), the sends follow the pipeline.
fn running_channels(pipeline: &gst::Pipeline, instance_id: &str) -> usize {
    (0..)
        .take_while(|ch| {
            pipeline
                .by_name(&format!("{}:{}", instance_id, send_tap_tee(*ch, true)))
                .is_some()
        })
        .count()
}

/// Build dynamic aux bus `bus` into the running mixer `instance_id`, whose
/// block properties are `properties` (with the values `added_bus_properties`
/// gave). Returns the new elements, for the flow's element map.
pub fn add_aux_bus(
    pipeline: &gst::Pipeline,
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    bus: &DynamicAuxBus,
) -> Result<Vec<(String, gst::Element)>, AuxBusError> {
    // The bus's mixer starts now: with "first", GStreamer 1.26 and later
    // pick the running time when its task first runs in PLAYING (force-live),
    // 1.24 the running time of the first buffer. `Bin::add` hands the element
    // the pipeline's base time, so the 1.26 race of `make_audiomixer` (a base
    // time not yet distributed) cannot hit. At zero (the block's default) it
    // would first make every buffer since the flow started, and the sends'
    // queues would fill and stall the channels' streaming threads, Main with
    // them, for as long as the flow has run.
    let spec = BusSpec {
        start: StartTime::First,
        ..BusSpec::new(instance_id, properties)
    };
    let aux = bus.aux - 1;
    let failed = |e: crate::blocks::BlockBuildError| AuxBusError::Failed(e.to_string());
    let mut elements = Vec::new();
    let mut links = Vec::new();
    let mut taps = Vec::new();
    aux_master(&spec, aux, properties, &mut elements, &mut links).map_err(failed)?;
    aux_inter_output(&spec, aux, &bus.channel, &mut elements, &mut links).map_err(failed)?;
    for ch in 0..running_channels(pipeline, instance_id) {
        aux_send(&spec, ch, aux, properties, &mut elements, &mut links).map_err(failed)?;
        // `aux_send` puts the link from the channel's tap tee last.
        taps.push(links.pop().expect("aux_send links its tap"));
    }

    let names: Vec<String> = elements.iter().map(|(id, _)| id.clone()).collect();
    let undo = |e: String| {
        teardown(pipeline, &spec, aux, &names);
        AuxBusError::Failed(e)
    };
    for (id, element) in &elements {
        pipeline
            .add(element)
            .map_err(|e| undo(format!("cannot add {}: {}", id, e)))?;
    }
    for (from, to) in &links {
        link(pipeline, from, to).map_err(undo)?;
    }
    // In reverse order of building, sends first; nothing pushes into the
    // bus yet: the sends reach the channels' tees only below.
    for (id, element) in elements.iter().rev() {
        element
            .sync_state_with_parent()
            .map_err(|e| undo(format!("{} did not follow the pipeline state: {}", id, e)))?;
    }
    for (from, to) in &taps {
        link(pipeline, from, to).map_err(undo)?;
    }
    // The new bus mixer and Solo's new pad learn the latency upstream of
    // them, like the buses built with the flow.
    if let Err(e) = pipeline.recalculate_latency() {
        warn!(
            "Mixer {}: latency after adding aux bus {}: {}",
            instance_id, bus.aux, e
        );
    }
    info!(
        "Mixer {}: added aux bus {} on channel {}",
        instance_id, bus.aux, bus.channel
    );
    Ok(elements)
}

/// Remove dynamic aux bus `aux` (1-based) from the running mixer
/// `instance_id`. Returns the names of the removed elements.
pub fn remove_aux_bus(
    pipeline: &gst::Pipeline,
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    aux: usize,
) -> Vec<String> {
    let spec = BusSpec::new(instance_id, properties);
    let names = bus_element_names(&spec, aux - 1, running_channels(pipeline, instance_id));
    teardown(pipeline, &spec, aux - 1, &names);
    // The latency is left as it is: lowering it while the mixer runs would
    // make every sink skip the difference, an audible jump on Main.
    info!("Mixer {}: removed aux bus {}", instance_id, aux);
    names
}

/// Make the send of channel `ch` into aux bus `aux` (both 0-based) of the
/// running mixer `instance_id` tap before the fader (`pre_fader`) or after
/// it. A bus the mixer has not built takes the tap when it is built.
pub fn set_send_tap(
    pipeline: &gst::Pipeline,
    instance_id: &str,
    ch: usize,
    aux: usize,
    pre_fader: bool,
) -> Result<(), String> {
    let id = |what: &str| format!("{}:{}", instance_id, what);
    let Some(send) = pipeline.by_name(&id(&send_id(ch, aux))) else {
        return Ok(());
    };
    let tee_id = id(&send_tap_tee(ch, pre_fader));
    let tee = pipeline
        .by_name(&tee_id)
        .ok_or_else(|| format!("{} not found", tee_id))?;
    let sink = send
        .static_pad("sink")
        .ok_or_else(|| format!("{} has no sink pad", send.name()))?;
    let mut old_tee = None;
    if let Some(old) = sink.peer() {
        if old.parent_element().as_ref() == Some(&tee) {
            return Ok(());
        }
        if let Some(parent) = old.parent_element() {
            parent.release_request_pad(&old);
            old_tee = Some(parent);
        }
    }
    let linked = tee
        .request_pad_simple("src_%u")
        .ok_or_else(|| format!("{} gave no pad", tee_id))
        .and_then(|new| {
            new.link(&sink).map(|_| ()).map_err(|e| {
                tee.release_request_pad(&new);
                format!("cannot link {} to {}: {:?}", tee_id, send.name(), e)
            })
        });
    if linked.is_err() {
        // Back on the tee it had, so the send keeps its sound.
        if let Some(old_tee) = old_tee {
            if let Some(pad) = old_tee.request_pad_simple("src_%u") {
                if pad.link(&sink).is_err() {
                    old_tee.release_request_pad(&pad);
                }
            }
        }
    }
    linked
}

/// The channel and aux bus (both 0-based) of a send's tap property
/// `chM_auxN_pre`.
pub fn send_pre_property_target(name: &str) -> Option<(usize, usize)> {
    let rest = name.strip_prefix("ch")?.strip_suffix("_pre")?;
    let (ch, aux) = rest.split_once("_aux")?;
    let (ch, aux) = (ch.parse::<usize>().ok()?, aux.parse::<usize>().ok()?);
    (ch >= 1 && aux >= 1).then(|| (ch - 1, aux - 1))
}

/// Every element of aux bus `aux` (0-based), sends first, so stopping them
/// in this order stops each source before what it feeds.
fn bus_element_names(spec: &BusSpec, aux: usize, num_channels: usize) -> Vec<String> {
    let mut names = Vec::new();
    for ch in 0..num_channels {
        names.push(spec.id(&send_id(ch, aux)));
        names.push(spec.id(&format!("aux_queue_{}_{}", ch, aux)));
    }
    for what in [
        "mixer",
        "mixer_rate",
        "volume",
        "level",
        "out_tee",
        "afl_volume",
        "afl_queue",
    ] {
        names.push(spec.id(&format!("aux{}_{}", aux, what)));
    }
    names.push(spec.id(&aux_intersink_id(aux)));
    names
}

/// Take the elements `names` of aux bus `aux` (0-based) out of the
/// pipeline: sends off the channels' tees, elements to NULL, Solo's pad
/// released, elements removed. Missing elements and pads are skipped, so
/// this also undoes a half-built add.
fn teardown(pipeline: &gst::Pipeline, spec: &BusSpec, aux: usize, names: &[String]) {
    let elements: Vec<gst::Element> = names.iter().filter_map(|n| pipeline.by_name(n)).collect();
    for element in &elements {
        let is_send = element.name().starts_with(&spec.id("aux_send_"));
        let Some(sink) = is_send.then(|| element.static_pad("sink")).flatten() else {
            continue;
        };
        if let Some(tee_pad) = sink.peer() {
            if let Some(tee) = tee_pad.parent_element() {
                tee.release_request_pad(&tee_pad);
            }
        }
    }
    let solo_pad = pipeline
        .by_name(&spec.id(&format!("aux{}_afl_queue", aux)))
        .and_then(|q| q.static_pad("src"))
        .and_then(|src| src.peer());
    for element in &elements {
        if element.set_state(gst::State::Null).is_err() {
            warn!("{}: could not stop", element.name());
        }
    }
    if let Some(pad) = solo_pad {
        if let Some(solo) = pad.parent_element() {
            if let Some(peer) = pad.peer() {
                let _ = peer.unlink(&pad);
            }
            solo.release_request_pad(&pad);
        }
    }
    for element in &elements {
        if pipeline.remove(element).is_err() {
            warn!("{}: could not leave the pipeline", element.name());
        }
    }
}

/// Link like the pipeline manager does: named pads pad to pad, an element
/// without a pad name by a compatible (or requested) pad.
fn link(pipeline: &gst::Pipeline, from: &ElementPadRef, to: &ElementPadRef) -> Result<(), String> {
    let element = |id: &str| {
        pipeline
            .by_name(id)
            .ok_or_else(|| format!("{} not found", id))
    };
    let (src, sink) = (element(&from.element_id)?, element(&to.element_id)?);
    src.link_pads(from.pad_name.as_deref(), &sink, to.pad_name.as_deref())
        .map_err(|e| format!("cannot link {:?} to {:?}: {}", from, to, e))
}
