//! The parts of a channel: its strip (delay, convert, gain, dynamics, EQ,
//! meter, pan, fader, the PFL/AFL taps into Solo, the sends into every aux
//! bus and the routing into Main and the groups), and the input of a
//! dynamic channel. The builder puts them together with the flow, and
//! `dynamic` builds the same parts into a running mixer.
//!
//! ```text
//! input_N | intersrc_N → delay_N → convert_N → … → routing_tee_N → Main, groups
//! ```

use std::collections::HashMap;

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::mixer::DynamicChannel;
use strom_types::{element::ElementPadRef, PropertyValue};
use tracing::debug;

use super::buses::*;
use super::elements::*;
use super::properties::*;
use super::{METER_INTERVAL_NS, MIN_KNEE_LINEAR};
use crate::blocks::builtin::time_offset;
use crate::blocks::BlockBuildError;

/// Buffer time of a dynamic channel's `intersrc` when it names none (the
/// Inter Input block's default).
pub(super) const DEFAULT_MAX_TIME_MS: u64 = 500;

/// What a channel strip connects to besides its own elements.
pub(super) struct ChannelRouting<'a> {
    /// `rust` or `lv2`.
    pub(super) dsp_backend: &'a str,
    /// Every built aux bus (0-based): the channel sends into each.
    pub(super) aux_buses: &'a [usize],
    pub(super) num_groups: usize,
}

/// Element id (relative to the block) of channel `ch`'s (0-based) delay: a
/// Time Offset identity, so `chN_delay` reaches it like `offset_ms` reaches
/// a Time Offset block.
pub(super) fn delay_id(ch: usize) -> String {
    time_offset::offset_element_id(&format!("delay_{}", ch))
}

/// Element id (relative to the block) of dynamic channel `ch`'s (0-based)
/// input.
pub(super) fn input_src_id(ch: usize) -> String {
    format!("intersrc_{}", ch)
}

/// The input of dynamic channel `dynamic`: an `intersrc` on its source
/// into the strip's delay.
pub(super) fn channel_inter_input(
    spec: &BusSpec,
    ch: usize,
    dynamic: &DynamicChannel,
    elements: &mut Elements,
    links: &mut Links,
) -> Result<(), BlockBuildError> {
    let src_id = spec.id(&input_src_id(ch));
    let src = gst::ElementFactory::make("intersrc")
        .name(&src_id)
        .property("producer-name", &dynamic.source)
        .property(
            "max-time",
            dynamic
                .max_time_ms
                .unwrap_or(DEFAULT_MAX_TIME_MS)
                .saturating_mul(1_000_000),
        )
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("intersrc: {}", e)))?;
    elements.push((src_id.clone(), src));
    links.push((
        ElementPadRef::pad(&src_id, "src"),
        ElementPadRef::pad(spec.id(&delay_id(ch)), "sink"),
    ));
    Ok(())
}

/// The strip of channel `ch` (0-based), from its delay (the channel's
/// input) to its sends into the buses. The links from the strip into the
/// buses come with it, so a strip built into a running mixer only joins
/// them when it is linked.
pub(super) fn channel_strip(
    spec: &BusSpec,
    ch: usize,
    routing: &ChannelRouting,
    properties: &HashMap<String, PropertyValue>,
    elements: &mut Elements,
    internal_links: &mut Links,
) -> Result<(), BlockBuildError> {
    let mixer_id = spec.id("audiomixer");
    let solo_mixer_id = spec.id("solo_mixer");

    // Delay (lip sync): later running times, so the buses play the sound
    // that much later. In front of everything, so the meter and the sends
    // are as late as Main.
    // Never negative: a sound cannot come before its source.
    let delay_ms = get_float_prop(properties, &format!("ch{}_delay", ch + 1), 0.0).max(0.0);
    let (delay_id, delay) =
        time_offset::make_offset_identity(&spec.id(&format!("delay_{}", ch)), delay_ms)?;
    answer_latency_for_unfed_input(&delay);
    elements.push((delay_id.clone(), delay));
    internal_links.push((
        ElementPadRef::pad(&delay_id, "src"),
        ElementPadRef::pad(spec.id(&format!("convert_{}", ch)), "sink"),
    ));

    let ch_num = ch + 1; // 1-indexed for display

    // Get channel properties
    let gain_db = get_float_prop(properties, &format!("ch{}_gain", ch_num), 0.0);

    let pan = properties
        .get(&format!("ch{}_pan", ch_num))
        .and_then(|v| match v {
            PropertyValue::Float(f) => Some(*f),
            _ => None,
        })
        .unwrap_or(0.0);

    let fader = properties
        .get(&format!("ch{}_fader", ch_num))
        .and_then(|v| match v {
            PropertyValue::Float(f) => Some(*f),
            _ => None,
        })
        .unwrap_or(1.0); // Default 0 dB (unity)

    let mute = properties
        .get(&format!("ch{}_mute", ch_num))
        .and_then(|v| match v {
            PropertyValue::Bool(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(false);

    // audioconvert (ensure proper format for processing)
    let convert_id = format!("{}:convert_{}", spec.instance_id, ch);
    let convert = gst::ElementFactory::make("audioconvert")
        .name(&convert_id)
        .build()
        .map_err(|e| {
            BlockBuildError::ElementCreation(format!("audioconvert ch{}: {}", ch_num, e))
        })?;
    elements.push((convert_id.clone(), convert));

    // audioresample: the buses run at `sample_rate`, so a source at
    // another rate is converted here rather than refused.
    let resample_id = format!("{}:resample_{}", spec.instance_id, ch);
    let resample = gst::ElementFactory::make("audioresample")
        .name(&resample_id)
        .build()
        .map_err(|e| {
            BlockBuildError::ElementCreation(format!("audioresample ch{}: {}", ch_num, e))
        })?;
    elements.push((resample_id.clone(), resample));

    // capsfilter to ensure F32LE stereo format for LV2 plugins, at the
    // buses' rate
    let caps_id = format!("{}:caps_{}", spec.instance_id, ch);
    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("rate", spec.sample_rate as i32)
        .field("channels", 2i32)
        .field("layout", "interleaved")
        .build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .name(&caps_id)
        .property("caps", &caps)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter ch{}: {}", ch_num, e)))?;
    elements.push((caps_id.clone(), capsfilter));

    // ----------------------------------------------------------------
    // Input gain stage
    // ----------------------------------------------------------------
    let gain_id = format!("{}:gain_{}", spec.instance_id, ch);
    let gain_linear = db_to_linear(gain_db);
    let gain_elem = gst::ElementFactory::make("volume")
        .name(&gain_id)
        .property("volume", gain_linear)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("gain ch{}: {}", ch_num, e)))?;
    elements.push((gain_id.clone(), gain_elem));

    // ----------------------------------------------------------------
    // HPF (High-Pass Filter)
    // ----------------------------------------------------------------
    let hpf_enabled = get_bool_prop(properties, &format!("ch{}_hpf_enabled", ch_num), false);
    let hpf_freq = get_float_prop(properties, &format!("ch{}_hpf_freq", ch_num), 80.0);

    let hpf_id = format!("{}:hpf_{}", spec.instance_id, ch);
    let hpf = make_hpf_element(&hpf_id, hpf_enabled, hpf_freq)?;
    elements.push((hpf_id.clone(), hpf));

    // ----------------------------------------------------------------
    // Gate (LSP Gate Stereo with fallback)
    // ----------------------------------------------------------------
    let gate_enabled = get_bool_prop(properties, &format!("ch{}_gate_enabled", ch_num), false);
    let gate_threshold = get_float_prop(properties, &format!("ch{}_gate_threshold", ch_num), -40.0);
    let gate_attack = get_float_prop(properties, &format!("ch{}_gate_attack", ch_num), 5.0);
    let gate_release = get_float_prop(properties, &format!("ch{}_gate_release", ch_num), 100.0);
    let gate_id = format!("{}:gate_{}", spec.instance_id, ch);
    let gate = make_gate_element(
        &gate_id,
        gate_enabled,
        gate_threshold,
        gate_attack,
        gate_release,
        routing.dsp_backend,
    )?;
    elements.push((gate_id.clone(), gate));

    // ----------------------------------------------------------------
    // Compressor (LSP Compressor Stereo with fallback)
    // ----------------------------------------------------------------
    let comp_enabled = get_bool_prop(properties, &format!("ch{}_comp_enabled", ch_num), false);
    let comp_threshold = get_float_prop(properties, &format!("ch{}_comp_threshold", ch_num), -20.0);
    let comp_ratio = get_float_prop(properties, &format!("ch{}_comp_ratio", ch_num), 4.0);
    let comp_attack = get_float_prop(properties, &format!("ch{}_comp_attack", ch_num), 10.0);
    let comp_release = get_float_prop(properties, &format!("ch{}_comp_release", ch_num), 100.0);
    let comp_makeup = get_float_prop(properties, &format!("ch{}_comp_makeup", ch_num), 0.0);
    let comp_knee = get_float_prop(properties, &format!("ch{}_comp_knee", ch_num), -6.0);

    let comp_id = format!("{}:comp_{}", spec.instance_id, ch);
    let compressor = make_compressor_element(
        &comp_id,
        comp_enabled,
        comp_threshold,
        comp_ratio,
        comp_attack,
        comp_release,
        comp_makeup,
        routing.dsp_backend,
    )?;
    // Set knee - Rust backend uses "knee" (linear), LV2 uses "kn" (linear)
    // kn range: 0.0631..1.0 (linear gain, default ~0.5 = -6dB)
    if compressor.find_property("knee").is_some() {
        let kn_val = db_to_linear(comp_knee).clamp(MIN_KNEE_LINEAR, 1.0) as f32;
        compressor.set_property("knee", kn_val);
    } else if compressor.find_property("kn").is_some() {
        let kn_val = db_to_linear(comp_knee).clamp(MIN_KNEE_LINEAR, 1.0) as f32;
        compressor.set_property("kn", kn_val);
    }
    elements.push((comp_id.clone(), compressor));

    // ----------------------------------------------------------------
    // EQ (LSP Parametric Equalizer x8 Stereo with fallback)
    // ----------------------------------------------------------------
    let eq_enabled = get_bool_prop(properties, &format!("ch{}_eq_enabled", ch_num), false);

    let eq_defaults: [(f64, f64); 4] = [(80.0, 1.0), (400.0, 1.0), (2000.0, 1.0), (8000.0, 1.0)];
    let eq_bands: [(f64, f64, f64); 4] = std::array::from_fn(|band| {
        let (def_freq, def_q) = eq_defaults[band];
        let freq = get_float_prop(
            properties,
            &format!("ch{}_eq{}_freq", ch_num, band + 1),
            def_freq,
        );
        let gain = get_float_prop(
            properties,
            &format!("ch{}_eq{}_gain", ch_num, band + 1),
            0.0,
        );
        let q = get_float_prop(properties, &format!("ch{}_eq{}_q", ch_num, band + 1), def_q);
        (freq, gain, q)
    });

    let eq_id = format!("{}:eq_{}", spec.instance_id, ch);
    let eq = make_eq_element(&eq_id, eq_enabled, &eq_bands, routing.dsp_backend)?;
    elements.push((eq_id.clone(), eq));

    // ----------------------------------------------------------------
    // Pre-fader tee (for PFL tap and pre-fader aux sends)
    // ----------------------------------------------------------------
    let pre_fader_tee_id = format!("{}:pre_fader_tee_{}", spec.instance_id, ch);
    let pre_fader_tee = gst::ElementFactory::make("tee")
        .name(&pre_fader_tee_id)
        .property("allow-not-linked", true)
        .build()
        .map_err(|e| {
            BlockBuildError::ElementCreation(format!("pre_fader_tee ch{}: {}", ch_num, e))
        })?;
    elements.push((pre_fader_tee_id.clone(), pre_fader_tee));

    // audiopanorama (pan control)
    let pan_id = format!("{}:pan_{}", spec.instance_id, ch);
    let panorama = gst::ElementFactory::make("audiopanorama")
        .name(&pan_id)
        .property("panorama", pan as f32)
        .build()
        .map_err(|e| {
            BlockBuildError::ElementCreation(format!("audiopanorama ch{}: {}", ch_num, e))
        })?;
    elements.push((pan_id.clone(), panorama));

    // Channel volume + mute as independent properties on GstVolume.
    let volume_id = format!("{}:volume_{}", spec.instance_id, ch);
    let volume = gst::ElementFactory::make("volume")
        .name(&volume_id)
        .property("volume", fader)
        .property("mute", mute)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("volume ch{}: {}", ch_num, e)))?;
    elements.push((volume_id.clone(), volume));

    // ----------------------------------------------------------------
    // Post-fader tee (for post-fader aux sends)
    // ----------------------------------------------------------------
    let post_fader_tee_id = format!("{}:post_fader_tee_{}", spec.instance_id, ch);
    let post_fader_tee = gst::ElementFactory::make("tee")
        .name(&post_fader_tee_id)
        .property("allow-not-linked", true)
        .build()
        .map_err(|e| {
            BlockBuildError::ElementCreation(format!("post_fader_tee ch{}: {}", ch_num, e))
        })?;
    elements.push((post_fader_tee_id.clone(), post_fader_tee));

    // level (pre-fader metering — taps post-EQ/dynamics, pre-fader/pan/mute
    // so the meter reflects the input signal regardless of fader position).
    // Built here, but linked in-path between `eq` and `pre_fader_tee` below.
    let level_id = format!("{}:level_{}", spec.instance_id, ch);
    let level = gst::ElementFactory::make("level")
        .name(&level_id)
        .property("interval", METER_INTERVAL_NS) // 100ms
        .property("post-messages", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("level ch{}: {}", ch_num, e)))?;
    elements.push((level_id.clone(), level));

    // ----------------------------------------------------------------
    // Routing tee (after post_fader_tee, for multi-destination routing)
    // ----------------------------------------------------------------
    let routing_tee_id = format!("{}:routing_tee_{}", spec.instance_id, ch);
    let routing_tee = gst::ElementFactory::make("tee")
        .name(&routing_tee_id)
        .property("allow-not-linked", true)
        .build()
        .map_err(|e| {
            BlockBuildError::ElementCreation(format!("routing_tee ch{}: {}", ch_num, e))
        })?;
    elements.push((routing_tee_id.clone(), routing_tee));

    // ----------------------------------------------------------------
    // Solo bus sends: PFL taps pre-fader, AFL taps post-fader.
    // Each is an independent volume-gate that sums into solo_mixer.
    //
    // Every send from a channel to a bus (solo, aux, main, group) has
    // its own queue. Each bus is paced by its own consumer, and an
    // aggregator pad holds only about its latency: without the queue,
    // a bus held by a clock-synced consumer blocks the channel's
    // thread and the other buses time the channel out. The
    // `test_channel_keeps_feeding_*` tests cover these channel sends,
    // not the bus-to-bus queues.
    // ----------------------------------------------------------------
    let pfl_enabled = get_bool_prop(properties, &format!("ch{}_pfl", ch_num), false);
    let afl_enabled = get_bool_prop(properties, &format!("ch{}_afl", ch_num), false);

    let pfl_volume_id = format!("{}:pfl_volume_{}", spec.instance_id, ch);
    let pfl_volume = gst::ElementFactory::make("volume")
        .name(&pfl_volume_id)
        .property("volume", if pfl_enabled { 1.0 } else { 0.0 })
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("pfl_volume ch{}: {}", ch_num, e)))?;
    elements.push((pfl_volume_id.clone(), pfl_volume));

    let pfl_queue_id = format!("{}:pfl_queue_{}", spec.instance_id, ch);
    let pfl_queue = gst::ElementFactory::make("queue")
        .name(&pfl_queue_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("pfl_queue ch{}: {}", ch_num, e)))?;
    elements.push((pfl_queue_id.clone(), pfl_queue));

    let afl_volume_id = format!("{}:afl_volume_{}", spec.instance_id, ch);
    let afl_volume = gst::ElementFactory::make("volume")
        .name(&afl_volume_id)
        .property("volume", if afl_enabled { 1.0 } else { 0.0 })
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("afl_volume ch{}: {}", ch_num, e)))?;
    elements.push((afl_volume_id.clone(), afl_volume));

    let afl_queue_id = format!("{}:afl_queue_{}", spec.instance_id, ch);
    let afl_queue = gst::ElementFactory::make("queue")
        .name(&afl_queue_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("afl_queue ch{}: {}", ch_num, e)))?;
    elements.push((afl_queue_id.clone(), afl_queue));

    // ----------------------------------------------------------------
    // Aux send paths (pre or post fader)
    // ----------------------------------------------------------------
    for &aux in routing.aux_buses {
        aux_send(spec, ch, aux, properties, elements, internal_links)?;
    }

    // ----------------------------------------------------------------
    // Main chain links
    // ----------------------------------------------------------------
    // Chain: convert → resample → caps → gain → hpf → gate → comp → eq → level → pre_fader_tee
    // The channel `level` element sits pre-fader (after EQ/dynamics, before
    // pan/fader/mute) so the meter reflects the signal hitting the fader.
    internal_links.push((
        ElementPadRef::pad(&convert_id, "src"),
        ElementPadRef::pad(&resample_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&resample_id, "src"),
        ElementPadRef::pad(&caps_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&caps_id, "src"),
        ElementPadRef::pad(&gain_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&gain_id, "src"),
        ElementPadRef::pad(&hpf_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&hpf_id, "src"),
        ElementPadRef::pad(&gate_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&gate_id, "src"),
        ElementPadRef::pad(&comp_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&comp_id, "src"),
        ElementPadRef::pad(&eq_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&eq_id, "src"),
        ElementPadRef::pad(&level_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&level_id, "src"),
        ElementPadRef::pad(&pre_fader_tee_id, "sink"),
    ));

    // pre_fader_tee → pan → volume → post_fader_tee → routing_tee
    internal_links.push((
        ElementPadRef::element(&pre_fader_tee_id), // Request pad from tee
        ElementPadRef::pad(&pan_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&pan_id, "src"),
        ElementPadRef::pad(&volume_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&volume_id, "src"),
        ElementPadRef::pad(&post_fader_tee_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::element(&post_fader_tee_id), // Request pad from tee
        ElementPadRef::pad(&routing_tee_id, "sink"),
    ));

    // PFL path: pre_fader_tee → pfl_volume → pfl_queue → solo_mixer
    internal_links.push((
        ElementPadRef::element(&pre_fader_tee_id),
        ElementPadRef::pad(&pfl_volume_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&pfl_volume_id, "src"),
        ElementPadRef::pad(&pfl_queue_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&pfl_queue_id, "src"),
        ElementPadRef::element(&solo_mixer_id),
    ));

    // AFL path: post_fader_tee → afl_volume → afl_queue → solo_mixer
    internal_links.push((
        ElementPadRef::element(&post_fader_tee_id),
        ElementPadRef::pad(&afl_volume_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&afl_volume_id, "src"),
        ElementPadRef::pad(&afl_queue_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&afl_queue_id, "src"),
        ElementPadRef::element(&solo_mixer_id),
    ));

    // ----------------------------------------------------------------
    // Multi-destination routing (Main + Groups)
    // Each destination has a volume element to enable/disable routing
    // ----------------------------------------------------------------

    // Route to main mixer
    let to_main_enabled = get_bool_prop(properties, &format!("ch{}_to_main", ch_num), true);
    let to_main_vol_id = format!("{}:to_main_vol_{}", spec.instance_id, ch);
    let to_main_vol = gst::ElementFactory::make("volume")
        .name(&to_main_vol_id)
        .property("volume", if to_main_enabled { 1.0 } else { 0.0 })
        .build()
        .map_err(|e| {
            BlockBuildError::ElementCreation(format!("to_main_vol ch{}: {}", ch_num, e))
        })?;
    elements.push((to_main_vol_id.clone(), to_main_vol));

    let to_main_queue_id = format!("{}:to_main_queue_{}", spec.instance_id, ch);
    let to_main_queue = gst::ElementFactory::make("queue")
        .name(&to_main_queue_id)
        .build()
        .map_err(|e| {
            BlockBuildError::ElementCreation(format!("to_main_queue ch{}: {}", ch_num, e))
        })?;
    elements.push((to_main_queue_id.clone(), to_main_queue));

    // Link: routing_tee → to_main_vol → to_main_queue → main_mixer
    internal_links.push((
        ElementPadRef::element(&routing_tee_id),
        ElementPadRef::pad(&to_main_vol_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&to_main_vol_id, "src"),
        ElementPadRef::pad(&to_main_queue_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&to_main_queue_id, "src"),
        ElementPadRef::element(&mixer_id),
    ));

    // Route to groups
    for sg in 0..routing.num_groups {
        let to_grp_enabled =
            get_bool_prop(properties, &format!("ch{}_to_grp{}", ch_num, sg + 1), false);

        let to_grp_vol_id = format!("{}:to_grp{}_vol_{}", spec.instance_id, sg, ch);
        let to_grp_vol = gst::ElementFactory::make("volume")
            .name(&to_grp_vol_id)
            .property("volume", if to_grp_enabled { 1.0 } else { 0.0 })
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!(
                    "to_grp{}_vol ch{}: {}",
                    sg + 1,
                    ch_num,
                    e
                ))
            })?;
        elements.push((to_grp_vol_id.clone(), to_grp_vol));

        let to_grp_queue_id = format!("{}:to_grp{}_queue_{}", spec.instance_id, sg, ch);
        let to_grp_queue = gst::ElementFactory::make("queue")
            .name(&to_grp_queue_id)
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!(
                    "to_grp{}_queue ch{}: {}",
                    sg + 1,
                    ch_num,
                    e
                ))
            })?;
        elements.push((to_grp_queue_id.clone(), to_grp_queue));

        // Link: routing_tee → to_grp_vol → to_grp_queue → group_mixer
        let sg_mixer_id = format!("{}:group{}_mixer", spec.instance_id, sg);
        internal_links.push((
            ElementPadRef::element(&routing_tee_id),
            ElementPadRef::pad(&to_grp_vol_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&to_grp_vol_id, "src"),
            ElementPadRef::pad(&to_grp_queue_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&to_grp_queue_id, "src"),
            ElementPadRef::element(&sg_mixer_id),
        ));

        if to_grp_enabled {
            debug!("Channel {} routed to group {}", ch_num, sg + 1);
        }
    }

    debug!(
        "Channel {} created: gain={:.1}dB, pan={}, fader={}, mute={}, pfl={}, afl={}, to_main={}",
        ch_num, gain_db, pan, fader, mute, pfl_enabled, afl_enabled, to_main_enabled
    );

    Ok(())
}

/// Every element of channel `ch`'s (0-based) input and strip, source
/// first, so stopping them in this order stops each one before what it
/// feeds. A static channel has no `intersrc`; callers skip what is missing.
pub(super) fn channel_element_names(
    spec: &BusSpec,
    ch: usize,
    routing: &ChannelRouting,
) -> Vec<String> {
    let mut names = vec![spec.id(&input_src_id(ch)), spec.id(&delay_id(ch))];
    for what in [
        "convert",
        "resample",
        "caps",
        "gain",
        "hpf",
        "gate",
        "comp",
        "eq",
        "level",
        "pre_fader_tee",
        "pan",
        "volume",
        "post_fader_tee",
        "routing_tee",
        "pfl_volume",
        "pfl_queue",
        "afl_volume",
        "afl_queue",
        "to_main_vol",
        "to_main_queue",
    ] {
        names.push(spec.id(&format!("{}_{}", what, ch)));
    }
    for sg in 0..routing.num_groups {
        names.push(spec.id(&format!("to_grp{}_vol_{}", sg, ch)));
        names.push(spec.id(&format!("to_grp{}_queue_{}", sg, ch)));
    }
    for &aux in routing.aux_buses {
        names.push(spec.id(&send_id(ch, aux)));
        names.push(spec.id(&format!("aux_queue_{}_{}", ch, aux)));
    }
    names
}
