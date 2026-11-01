//! Dynamic channels: channels added to and removed from a running mixer
//! without rebuilding it.
//!
//! A dynamic channel is built from the same parts as a static one (see
//! `channels`), under the same element names, so the channel properties
//! (`chN_fader`, `chN_auxM_level`, …) reach it like a static channel. Its
//! sound comes from an `intersrc` on the channel's source instead of a
//! block pad:
//!
//! ```text
//! intersrc_N → delay_N → convert_N → … → routing_tee_N → to_main → Main
//!                                         (sends) → Solo, aux buses, groups
//! ```
//!
//! **Add**: the elements go into the pipeline, are linked among each other
//! and into the bus mixers, then follow the pipeline's state, sinks first,
//! the `intersrc` last: nothing pushes before everything downstream runs.
//!
//! **Remove**: the elements stop, source first; the pads they hold on the
//! bus mixers are released, then the elements leave the pipeline.

use std::collections::HashMap;

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::mixer::{AddMixerChannelRequest, DynamicChannel, MixerChannels};
use strom_types::PropertyValue;
use tracing::{info, warn};

use super::buses::BusSpec;
use super::channels::*;
use super::dynamic::{link, running_channels};
use super::properties::*;
use super::{MAX_AUX_BUSES, MAX_GROUPS};
use crate::blocks::builtin::start_time::StartTime;

/// Why adding or removing a channel failed.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    /// The mixer has no free channel numbers (`max_channels`).
    #[error("{0}")]
    NotSupported(String),
    /// The request is malformed (number out of range, no source).
    #[error("{0}")]
    Invalid(String),
    /// The number is taken, or a static channel cannot go.
    #[error("{0}")]
    Conflict(String),
    /// No such dynamic channel, or no running mixer.
    #[error("{0}")]
    NotFound(String),
    /// GStreamer refused (element missing, link or state change failed).
    #[error("{0}")]
    Failed(String),
}

/// The channels of a mixer with these block properties.
pub fn channels(properties: &HashMap<String, PropertyValue>) -> MixerChannels {
    let num_static = parse_num_channels(properties);
    let capacity = parse_max_channels(properties, num_static);
    let dynamic = parse_dynamic_channels(properties, num_static, capacity)
        .map(|(channels, _)| channels)
        .unwrap_or_default();
    MixerChannels {
        capacity,
        num_static,
        dynamic,
    }
}

/// The channel number `request` gets on a mixer with these block
/// properties: the requested one or the lowest free one. `built` tells
/// which channels the running pipeline has.
pub fn choose_channel(
    properties: &HashMap<String, PropertyValue>,
    request: &AddMixerChannelRequest,
    built: impl Fn(usize) -> bool,
) -> Result<DynamicChannel, ChannelError> {
    let channels = channels(properties);
    let (first, capacity) = (channels.num_static + 1, channels.capacity);
    if first > capacity {
        return Err(ChannelError::NotSupported(format!(
            "this mixer has no free channels (max_channels {} = num_channels); \
             raise max_channels to add channels at runtime",
            capacity
        )));
    }
    let stored = stored_dynamic_channels(properties)
        .map_err(|e| ChannelError::Conflict(format!("{}; fix or remove it to add channels", e)))?;
    let taken = |ch: usize| stored.iter().any(|c| c.channel == ch) || built(ch);
    let number = match request.channel {
        Some(ch) if ch < first || ch > capacity => {
            return Err(ChannelError::Invalid(format!(
                "channel {} is outside the free channels {}..={}",
                ch, first, capacity
            )))
        }
        Some(ch) if taken(ch) => {
            return Err(ChannelError::Conflict(format!("channel {} is taken", ch)))
        }
        Some(ch) => ch,
        None => (first..=capacity).find(|&ch| !taken(ch)).ok_or_else(|| {
            ChannelError::Conflict(format!("all {} channels are taken", capacity))
        })?,
    };
    let channel = DynamicChannel {
        channel: number,
        source: request.source.clone(),
        max_time_ms: request.max_time_ms,
    };
    strom_types::mixer::validate_dynamic_channels(
        std::slice::from_ref(&channel),
        channels.num_static,
        capacity,
    )
    .map_err(ChannelError::Invalid)?;
    Ok(channel)
}

/// Whether the running mixer `instance_id` has channel `ch` (1-based).
pub fn is_built(pipeline: &gst::Pipeline, instance_id: &str, ch: usize) -> bool {
    running_channels(pipeline, instance_id).contains(&(ch - 1))
}

/// Whether the running mixer `instance_id` has channel `ch` (1-based) as a
/// dynamic channel: one fed by an `intersrc`.
pub fn is_built_dynamic(pipeline: &gst::Pipeline, instance_id: &str, ch: usize) -> bool {
    pipeline
        .by_name(&format!("{}:{}", instance_id, input_src_id(ch - 1)))
        .is_some()
}

/// What the running mixer's channels connect to: its backend, its built
/// aux buses and groups.
fn running_routing(
    pipeline: &gst::Pipeline,
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
) -> (String, Vec<usize>, usize) {
    let built = |what: String| {
        pipeline
            .by_name(&format!("{}:{}", instance_id, what))
            .is_some()
    };
    let aux_buses = (0..MAX_AUX_BUSES)
        .filter(|aux| built(format!("aux{}_mixer", aux)))
        .collect();
    let num_groups = (0..MAX_GROUPS)
        .take_while(|sg| built(format!("group{}_mixer", sg)))
        .count();
    let dsp = match get_string_prop(properties, "dsp_backend", "rust") {
        "lv2" => "lv2",
        _ => "rust",
    };
    (dsp.to_string(), aux_buses, num_groups)
}

/// Build dynamic channel `channel` into the running mixer `instance_id`,
/// whose block properties are `properties`. Returns the new elements, for
/// the flow's element map.
pub fn add_channel(
    pipeline: &gst::Pipeline,
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    channel: &DynamicChannel,
) -> Result<Vec<(String, gst::Element)>, ChannelError> {
    // A bus mixer of the strip (none here, the strip only feeds them) would
    // start now; kept like `add_aux_bus` for one rule.
    let spec = BusSpec {
        start: StartTime::First,
        ..BusSpec::new(instance_id, properties)
    };
    let ch = channel.channel - 1;
    let (dsp_backend, aux_buses, num_groups) = running_routing(pipeline, instance_id, properties);
    let routing = ChannelRouting {
        dsp_backend: &dsp_backend,
        aux_buses: &aux_buses,
        num_groups,
    };
    let failed = |e: crate::blocks::BlockBuildError| ChannelError::Failed(e.to_string());
    let mut elements = Vec::new();
    let mut links = Vec::new();
    channel_inter_input(&spec, ch, channel, &mut elements, &mut links).map_err(failed)?;
    channel_strip(&spec, ch, &routing, properties, &mut elements, &mut links).map_err(failed)?;

    let undo = |e: String| {
        teardown(pipeline, &channel_element_names(&spec, ch, &routing));
        ChannelError::Failed(e)
    };
    for (id, element) in &elements {
        pipeline
            .add(element)
            .map_err(|e| undo(format!("cannot add {}: {}", id, e)))?;
    }
    for (from, to) in &links {
        link(pipeline, from, to).map_err(undo)?;
    }
    // The `intersrc` was built first: in reverse order it starts last, when
    // everything it feeds runs.
    for (id, element) in elements.iter().rev() {
        element
            .sync_state_with_parent()
            .map_err(|e| undo(format!("{} did not follow the pipeline state: {}", id, e)))?;
    }
    // The bus mixers' new pads learn the latency upstream of them.
    if let Err(e) = pipeline.recalculate_latency() {
        warn!(
            "Mixer {}: latency after adding channel {}: {}",
            instance_id, channel.channel, e
        );
    }
    info!(
        "Mixer {}: added channel {} from {}",
        instance_id, channel.channel, channel.source
    );
    Ok(elements)
}

/// Remove dynamic channel `ch` (1-based) from the running mixer
/// `instance_id`. Returns the names of the removed elements.
pub fn remove_channel(
    pipeline: &gst::Pipeline,
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    ch: usize,
) -> Vec<String> {
    let spec = BusSpec::new(instance_id, properties);
    let (dsp_backend, aux_buses, num_groups) = running_routing(pipeline, instance_id, properties);
    let routing = ChannelRouting {
        dsp_backend: &dsp_backend,
        aux_buses: &aux_buses,
        num_groups,
    };
    let names = channel_element_names(&spec, ch - 1, &routing);
    let removed = teardown(pipeline, &names);
    // The latency is left as it is: lowering it while the mixer runs would
    // make every sink skip the difference, an audible jump on Main.
    info!("Mixer {}: removed channel {}", instance_id, ch);
    removed
}

/// The source of dynamic channel `ch` (1-based) of the running mixer.
pub fn channel_source(pipeline: &gst::Pipeline, instance_id: &str, ch: usize) -> Option<String> {
    let src = pipeline.by_name(&format!("{}:{}", instance_id, input_src_id(ch - 1)))?;
    Some(src.property::<String>("producer-name"))
}

/// Take the elements `names` of one channel out of the pipeline: to NULL
/// in order (source first), the pads they hold on elements outside the
/// channel (the bus mixers' request pads) released, then out of the
/// pipeline. Missing elements are skipped, so this also undoes a
/// half-built add. Returns the names of the elements that were there.
fn teardown(pipeline: &gst::Pipeline, names: &[String]) -> Vec<String> {
    let elements: Vec<gst::Element> = names.iter().filter_map(|n| pipeline.by_name(n)).collect();
    for element in &elements {
        if element.set_state(gst::State::Null).is_err() {
            warn!("{}: could not stop", element.name());
        }
    }
    for element in &elements {
        for src in element.src_pads() {
            let Some(peer) = src.peer() else {
                continue;
            };
            let Some(target) = peer.parent_element() else {
                continue;
            };
            if elements.contains(&target) {
                continue;
            }
            let _ = src.unlink(&peer);
            target.release_request_pad(&peer);
        }
    }
    for element in &elements {
        if pipeline.remove(element).is_err() {
            warn!("{}: could not leave the pipeline", element.name());
        }
    }
    elements.iter().map(|e| e.name().to_string()).collect()
}
