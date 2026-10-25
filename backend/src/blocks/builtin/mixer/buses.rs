//! The parts of an aux bus: its master (mixer, fader, meter, output tee,
//! AFL tap into Solo), the send of each channel into it, and the output of
//! a dynamic bus. The builder puts them together with the flow, and
//! `dynamic` builds the same parts into a running mixer.

use std::collections::HashMap;

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::{element::ElementPadRef, PropertyValue};

use super::elements::{make_audiomixer, make_rate_pin};
use super::properties::*;
use super::METER_INTERVAL_NS;
use crate::blocks::builtin::start_time::{parse_start_time, StartTime};
use crate::blocks::BlockBuildError;
use strom_types::mixer::{DEFAULT_LATENCY_MS, DEFAULT_MIN_UPSTREAM_LATENCY_MS};

pub(super) type Elements = Vec<(String, gst::Element)>;
pub(super) type Links = Vec<(ElementPadRef, ElementPadRef)>;

/// What building a bus needs from the block: its instance id and the
/// settings every bus mixer of the block shares.
#[derive(Clone, Debug)]
pub(super) struct BusSpec {
    pub(super) instance_id: String,
    pub(super) sample_rate: u32,
    pub(super) force_live: bool,
    pub(super) latency_ms: u64,
    pub(super) min_upstream_latency_ms: u64,
    pub(super) start: StartTime,
}

impl BusSpec {
    pub(super) fn new(instance_id: &str, properties: &HashMap<String, PropertyValue>) -> Self {
        Self {
            instance_id: instance_id.to_string(),
            sample_rate: parse_sample_rate(properties),
            force_live: get_bool_prop(properties, "force_live", true),
            latency_ms: get_u64_prop(properties, "latency", DEFAULT_LATENCY_MS),
            min_upstream_latency_ms: get_u64_prop(
                properties,
                "min_upstream_latency",
                DEFAULT_MIN_UPSTREAM_LATENCY_MS,
            ),
            start: parse_start_time(properties),
        }
    }

    /// Full element id of `id` in this block.
    pub(super) fn id(&self, id: &str) -> String {
        format!("{}:{}", self.instance_id, id)
    }

    /// Make a bus mixer with the block's settings and `latency_ms`.
    pub(super) fn audiomixer(
        &self,
        id: &str,
        latency_ms: u64,
    ) -> Result<gst::Element, BlockBuildError> {
        make_audiomixer(
            id,
            self.force_live,
            latency_ms,
            self.min_upstream_latency_ms,
            self.start,
        )
    }

    /// Follow the bus mixer `mixer_id` with its rate pin. Returns the pin's
    /// id: the bus's output now leaves from there.
    pub(super) fn push_rate_pin(
        &self,
        mixer_id: &str,
        elements: &mut Elements,
        links: &mut Links,
    ) -> Result<String, BlockBuildError> {
        let pin_id = format!("{}_rate", mixer_id);
        elements.push((pin_id.clone(), make_rate_pin(&pin_id, self.sample_rate)?));
        links.push((
            ElementPadRef::pad(mixer_id, "src"),
            ElementPadRef::pad(&pin_id, "sink"),
        ));
        Ok(pin_id)
    }
}

fn make(factory: &str, id: &str) -> Result<gst::Element, BlockBuildError> {
    gst::ElementFactory::make(factory)
        .name(id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", id, e)))
}

/// The master of aux bus `aux` (0-based): mixer → rate pin → volume → level
/// → output tee, and the AFL tap from the tee into Solo.
pub(super) fn aux_master(
    spec: &BusSpec,
    aux: usize,
    properties: &HashMap<String, PropertyValue>,
    elements: &mut Elements,
    links: &mut Links,
) -> Result<(), BlockBuildError> {
    let id = |what: &str| spec.id(&format!("aux{}_{}", aux, what));
    let mixer_id = id("mixer");
    elements.push((
        mixer_id.clone(),
        spec.audiomixer(&mixer_id, spec.latency_ms)?,
    ));
    let out_id = spec.push_rate_pin(&mixer_id, elements, links)?;

    let fader = get_float_prop(properties, &format!("aux{}_fader", aux + 1), 1.0);
    let mute = get_bool_prop(properties, &format!("aux{}_mute", aux + 1), false);
    let volume_id = id("volume");
    let volume = make("volume", &volume_id)?;
    volume.set_property("volume", fader);
    volume.set_property("mute", mute);
    elements.push((volume_id.clone(), volume));

    let level_id = id("level");
    let level = make("level", &level_id)?;
    level.set_property("interval", METER_INTERVAL_NS);
    level.set_property("post-messages", true);
    elements.push((level_id.clone(), level));

    // Allow-not-linked, so an unconnected aux_out does not stall the bus.
    let tee_id = id("out_tee");
    let tee = make("tee", &tee_id)?;
    tee.set_property("allow-not-linked", true);
    elements.push((tee_id.clone(), tee));

    links.push((
        ElementPadRef::pad(&out_id, "src"),
        ElementPadRef::pad(&volume_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&volume_id, "src"),
        ElementPadRef::pad(&level_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&level_id, "src"),
        ElementPadRef::pad(&tee_id, "sink"),
    ));

    // AFL tap on the aux master. Taps from the output tee (post-master,
    // post-mute, post-meter) so the operator hears exactly what is leaving
    // the aux send. Like channel PFL/AFL, the gate volume is either 0 or 1 —
    // `bool_to_volume` transforms the public Bool property to the underlying
    // volume value.
    let afl = get_bool_prop(properties, &format!("aux{}_afl", aux + 1), false);
    let afl_volume_id = id("afl_volume");
    let afl_volume = make("volume", &afl_volume_id)?;
    afl_volume.set_property("volume", if afl { 1.0 } else { 0.0 });
    elements.push((afl_volume_id.clone(), afl_volume));
    let afl_queue_id = id("afl_queue");
    elements.push((afl_queue_id.clone(), make("queue", &afl_queue_id)?));

    links.push((
        ElementPadRef::element(&tee_id),
        ElementPadRef::pad(&afl_volume_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&afl_volume_id, "src"),
        ElementPadRef::pad(&afl_queue_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&afl_queue_id, "src"),
        ElementPadRef::element(spec.id("solo_mixer")),
    ));
    Ok(())
}

/// Element id (relative to the block) of the output of dynamic aux bus
/// `aux` (0-based).
pub(super) fn aux_intersink_id(aux: usize) -> String {
    format!("aux{}_intersink", aux)
}

/// The output of a dynamic aux bus `aux` (0-based): its output tee into an
/// `intersink` on `channel`. Not synced to the clock: the bus mixer
/// already runs at the pace of the pipeline, like an Inter Output with
/// `sync` off.
pub(super) fn aux_inter_output(
    spec: &BusSpec,
    aux: usize,
    channel: &str,
    elements: &mut Elements,
    links: &mut Links,
) -> Result<(), BlockBuildError> {
    let sink_id = spec.id(&aux_intersink_id(aux));
    let sink = crate::blocks::builtin::inter::create_intersink(&sink_id, channel, false)?;
    elements.push((sink_id.clone(), sink));
    links.push((
        ElementPadRef::element(spec.id(&format!("aux{}_out_tee", aux))),
        ElementPadRef::pad(&sink_id, "sink"),
    ));
    Ok(())
}

/// Element id (relative to the block) of the tee a send of channel `ch`
/// (0-based) taps: before the fader or after it.
pub(super) fn send_tap_tee(ch: usize, pre_fader: bool) -> String {
    if pre_fader {
        format!("pre_fader_tee_{}", ch)
    } else {
        format!("post_fader_tee_{}", ch)
    }
}

/// Element id (relative to the block) of the send of channel `ch` into aux
/// bus `aux` (both 0-based): the volume that sets its level.
pub(super) fn send_id(ch: usize, aux: usize) -> String {
    format!("aux_send_{}_{}", ch, aux)
}

/// Whether the send of channel `ch` into aux bus `aux` (both 0-based) taps
/// before the fader. All aux sends default post-fader (matches
/// definition.rs).
pub(super) fn send_is_pre_fader(
    properties: &HashMap<String, PropertyValue>,
    ch: usize,
    aux: usize,
) -> bool {
    get_bool_prop(properties, &send_pre_property(ch, aux), false)
}

/// Block property that sets the tap of the send of channel `ch` into aux
/// bus `aux` (both 0-based).
pub(super) fn send_pre_property(ch: usize, aux: usize) -> String {
    format!("ch{}_aux{}_pre", ch + 1, aux + 1)
}

/// The send of channel `ch` into aux bus `aux` (both 0-based): its volume
/// and queue, and the links to the bus mixer. The link from the channel's
/// tap tee into the send comes last in `links`, so `dynamic` can make it
/// once the send runs.
pub(super) fn aux_send(
    spec: &BusSpec,
    ch: usize,
    aux: usize,
    properties: &HashMap<String, PropertyValue>,
    elements: &mut Elements,
    links: &mut Links,
) -> Result<(), BlockBuildError> {
    let level = get_float_prop(
        properties,
        &format!("ch{}_aux{}_level", ch + 1, aux + 1),
        0.0,
    );
    let send_id = spec.id(&send_id(ch, aux));
    let send = make("volume", &send_id)?;
    send.set_property("volume", level);
    elements.push((send_id.clone(), send));
    let queue_id = spec.id(&format!("aux_queue_{}_{}", ch, aux));
    elements.push((queue_id.clone(), make("queue", &queue_id)?));

    links.push((
        ElementPadRef::pad(&send_id, "src"),
        ElementPadRef::pad(&queue_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&queue_id, "src"),
        ElementPadRef::element(spec.id(&format!("aux{}_mixer", aux))),
    ));
    let tee = spec.id(&send_tap_tee(ch, send_is_pre_fader(properties, ch, aux)));
    links.push((
        ElementPadRef::element(&tee),
        ElementPadRef::pad(&send_id, "sink"),
    ));
    Ok(())
}
