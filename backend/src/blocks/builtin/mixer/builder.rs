use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::events::EventBroadcaster;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, element::ElementPadRef, FlowId, MediaType, PropertyValue};
use tracing::{info, warn};

use super::buses::*;
use super::channels::*;
use super::elements::*;
use super::metering::connect_mixer_meter_handler;
use super::properties::*;
use super::{METER_INTERVAL_NS, MIN_KNEE_LINEAR};
use strom_types::mixer::{
    DEFAULT_INTERNAL_BUS_LATENCY_MS, DEFAULT_LATENCY_MS, DEFAULT_MIN_UPSTREAM_LATENCY_MS,
};

/// Mixer block builder.
pub struct MixerBuilder;

impl BlockBuilder for MixerBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let num_channels = parse_num_channels(properties);
        let num_aux_buses = parse_num_aux_buses(properties);
        let num_groups = parse_num_groups(properties);
        // Create input pads dynamically
        let inputs = (0..num_channels)
            .map(|i| ExternalPad {
                name: format!("input_{}", i + 1),
                label: Some(format!("A{}", i)),
                media_type: MediaType::Audio,
                internal_element_id: delay_id(i),
                internal_pad_name: "sink".to_string(),
            })
            .collect();

        // Output pads - point to output tees so unconnected outputs don't cause
        // NOT_LINKED flow errors. Each output tee has allow-not-linked=true.
        let mut outputs = vec![
            // Main stereo output
            ExternalPad {
                name: "main_out".to_string(),
                label: Some("Main".to_string()),
                media_type: MediaType::Audio,
                internal_element_id: "main_out_tee".to_string(),
                internal_pad_name: "src_%u".to_string(),
            },
            // Monitor output: follows Main when no PFL/AFL is active anywhere,
            // and the solo mix the moment any channel engages PFL or AFL.
            ExternalPad {
                name: "monitor_out".to_string(),
                label: Some("Monitor".to_string()),
                media_type: MediaType::Audio,
                internal_element_id: "monitor_out_tee".to_string(),
                internal_pad_name: "src_%u".to_string(),
            },
        ];

        // Add aux outputs
        for aux in 0..num_aux_buses {
            outputs.push(ExternalPad {
                name: format!("aux_out_{}", aux + 1),
                label: Some(format!("Aux{}", aux + 1)),
                media_type: MediaType::Audio,
                internal_element_id: format!("aux{}_out_tee", aux),
                internal_pad_name: "src_%u".to_string(),
            });
        }

        // Add group outputs
        for sg in 0..num_groups {
            outputs.push(ExternalPad {
                name: format!("group_out_{}", sg + 1),
                label: Some(format!("Grp{}", sg + 1)),
                media_type: MediaType::Audio,
                internal_element_id: format!("group{}_out_tee", sg),
                internal_pad_name: "src_%u".to_string(),
            });
        }

        Some(ExternalPads { inputs, outputs })
    }

    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building Mixer block instance: {}", instance_id);

        let num_channels = parse_num_channels(properties);
        let num_aux_buses = parse_num_aux_buses(properties);
        let num_groups = parse_num_groups(properties);
        let monitor_source = parse_monitor_source(properties, num_aux_buses);
        let aux_capacity = parse_max_aux_buses(properties, num_aux_buses);
        // A stored bus that no longer fits is left out, a list that does not
        // parse leaves the mixer without its dynamic buses: either way
        // rather than without sound. They stay stored.
        let dynamic_aux_buses =
            match parse_dynamic_aux_buses(properties, num_aux_buses, aux_capacity) {
                Ok((buses, skipped)) => {
                    for reason in skipped {
                        warn!("Mixer {}: aux bus left out: {}", instance_id, reason);
                    }
                    buses
                }
                Err(e) => {
                    warn!("Mixer {}: {}", instance_id, e);
                    Vec::new()
                }
            };
        // Dynamic channels, as for the buses: a stored channel that no longer
        // fits is left out (and stays stored).
        let channel_capacity = parse_max_channels(properties, num_channels);
        let dynamic_channels =
            match parse_dynamic_channels(properties, num_channels, channel_capacity) {
                Ok((channels, skipped)) => {
                    for reason in skipped {
                        warn!("Mixer {}: channel left out: {}", instance_id, reason);
                    }
                    channels
                }
                Err(e) => {
                    warn!("Mixer {}: {}", instance_id, e);
                    Vec::new()
                }
            };
        // Every built aux bus, 0-based: the static ones, then the dynamic.
        let built_aux_buses: Vec<usize> = (0..num_aux_buses)
            .chain(dynamic_aux_buses.iter().map(|bus| bus.aux - 1))
            .collect();
        let spec = BusSpec::new(instance_id, properties);
        let sample_rate = parse_sample_rate(properties);
        // All buses of this block start alike (see `start_time`).
        let start = crate::blocks::builtin::start_time::parse_start_time(properties);
        let dsp_backend = get_string_prop(properties, "dsp_backend", "rust");
        if dsp_backend != "rust" && dsp_backend != "lv2" {
            warn!(
                "Unrecognized dsp_backend '{}', falling back to 'rust'",
                dsp_backend
            );
        }
        let dsp_backend = if dsp_backend == "rust" || dsp_backend == "lv2" {
            dsp_backend
        } else {
            "rust"
        };
        info!(
            "Mixer config: {} channels, {} aux buses, {} groups, dsp={}, rate={}",
            num_channels, num_aux_buses, num_groups, dsp_backend, sample_rate,
        );

        let mut elements = Vec::new();
        let mut internal_links = Vec::new();

        // Mixer aggregator settings
        let force_live = get_bool_prop(properties, "force_live", true);
        let latency_ms = get_u64_prop(properties, "latency", DEFAULT_LATENCY_MS);
        let min_upstream_latency_ms = get_u64_prop(
            properties,
            "min_upstream_latency",
            DEFAULT_MIN_UPSTREAM_LATENCY_MS,
        );
        // Aggregator latency of Solo, Monitor, and Main when it has groups to
        // sum: buses whose inputs come from other buses of this block. An
        // aggregator's latency is slack for late inputs, and it adds to the
        // latency the aggregator reports downstream. The buses they sum
        // already carry the block's slack, so giving them the block `latency`
        // again stacks it (channel → aux → solo → monitor would be three times
        // the block latency, channel → group → main twice), and a pipeline
        // runs every sink at its largest reported latency.
        //
        // It is still the slack for a late buffer from those buses. Once any
        // channel input stops delivering, a bus produces at its own deadline,
        // so a hiccup on Main's thread longer than this leaves a gap in
        // Monitor, and one on a group's thread a gap in Main. 30 ms rode out
        // 25 ms hiccups in tests; 10 ms did not survive 15 ms. Capped at the
        // block latency: more than that would stack again.
        let internal_bus_latency_ms = get_u64_prop(
            properties,
            "internal_bus_latency",
            DEFAULT_INTERNAL_BUS_LATENCY_MS,
        )
        .min(latency_ms);
        // A bus that also takes channel taps straight from the channels keeps
        // the block's slack for them as long as another bus feeds it too: it
        // then reports that bus's latency plus its own. A bus fed only by
        // channels keeps the block latency.
        let bus_latency_ms = |sums_buses: bool| {
            if sums_buses {
                internal_bus_latency_ms
            } else {
                latency_ms
            }
        };

        // ========================================================================
        // Create main audiomixer
        // ========================================================================
        let mixer_id = format!("{}:audiomixer", instance_id);
        // Every channel feeds Main and every group bus, and each group bus
        // feeds Main too. The groups are the first to sum the channels, so they
        // keep the block latency.
        let audiomixer = make_audiomixer(
            &mixer_id,
            force_live,
            bus_latency_ms(num_groups > 0),
            min_upstream_latency_ms,
            start,
        )?;
        elements.push((mixer_id.clone(), audiomixer.clone()));
        let mixer_out_id = spec.push_rate_pin(&mixer_id, &mut elements, &mut internal_links)?;

        // ========================================================================
        // Main bus processing: comp → EQ → limiter
        // ========================================================================
        let main_comp_enabled = get_bool_prop(properties, "main_comp_enabled", false);
        let main_comp_threshold = get_float_prop(properties, "main_comp_threshold", -20.0);
        let main_comp_ratio = get_float_prop(properties, "main_comp_ratio", 4.0);
        let main_comp_attack = get_float_prop(properties, "main_comp_attack", 10.0);
        let main_comp_release = get_float_prop(properties, "main_comp_release", 100.0);
        let main_comp_makeup = get_float_prop(properties, "main_comp_makeup", 0.0);
        let main_comp_knee = get_float_prop(properties, "main_comp_knee", -6.0);

        let main_comp_id = format!("{}:main_comp", instance_id);
        let main_comp = make_compressor_element(
            &main_comp_id,
            main_comp_enabled,
            main_comp_threshold,
            main_comp_ratio,
            main_comp_attack,
            main_comp_release,
            main_comp_makeup,
            dsp_backend,
        )?;
        // Set knee - Rust backend uses "knee" (linear), LV2 uses "kn" (linear)
        if main_comp.find_property("knee").is_some() {
            let kn_val = db_to_linear(main_comp_knee).clamp(MIN_KNEE_LINEAR, 1.0) as f32;
            main_comp.set_property("knee", kn_val);
        } else if main_comp.find_property("kn").is_some() {
            let kn_val = db_to_linear(main_comp_knee).clamp(MIN_KNEE_LINEAR, 1.0) as f32;
            main_comp.set_property("kn", kn_val);
        }
        elements.push((main_comp_id.clone(), main_comp));

        let main_eq_enabled = get_bool_prop(properties, "main_eq_enabled", false);
        let main_eq_bands = [
            (
                get_float_prop(properties, "main_eq1_freq", 80.0),
                get_float_prop(properties, "main_eq1_gain", 0.0),
                get_float_prop(properties, "main_eq1_q", 1.0),
            ),
            (
                get_float_prop(properties, "main_eq2_freq", 400.0),
                get_float_prop(properties, "main_eq2_gain", 0.0),
                get_float_prop(properties, "main_eq2_q", 1.0),
            ),
            (
                get_float_prop(properties, "main_eq3_freq", 2000.0),
                get_float_prop(properties, "main_eq3_gain", 0.0),
                get_float_prop(properties, "main_eq3_q", 1.0),
            ),
            (
                get_float_prop(properties, "main_eq4_freq", 8000.0),
                get_float_prop(properties, "main_eq4_gain", 0.0),
                get_float_prop(properties, "main_eq4_q", 1.0),
            ),
        ];
        let main_eq_id = format!("{}:main_eq", instance_id);
        let main_eq = make_eq_element(&main_eq_id, main_eq_enabled, &main_eq_bands, dsp_backend)?;
        elements.push((main_eq_id.clone(), main_eq));

        let main_limiter_enabled = get_bool_prop(properties, "main_limiter_enabled", false);
        let main_limiter_threshold = get_float_prop(properties, "main_limiter_threshold", -3.0);
        let main_limiter_id = format!("{}:main_limiter", instance_id);
        let main_limiter = make_limiter_element(
            &main_limiter_id,
            main_limiter_enabled,
            main_limiter_threshold,
            dsp_backend,
        )?;
        elements.push((main_limiter_id.clone(), main_limiter));

        // Main output volume (master fader)
        let main_volume_id = format!("{}:main_volume", instance_id);
        let main_volume = gst::ElementFactory::make("volume")
            .name(&main_volume_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("main volume: {}", e)))?;

        // Fader and mute are independent properties on the GstVolume element —
        // the native `mute` property handles anti-click via the volume_ramps
        // manager so we don't need the legacy "volume = 0 if mute" trick.
        let main_fader = properties
            .get("main_fader")
            .and_then(|v| match v {
                PropertyValue::Float(f) => Some(*f),
                _ => None,
            })
            .unwrap_or(1.0);
        let main_mute = get_bool_prop(properties, "main_mute", false);
        main_volume.set_property("volume", main_fader);
        main_volume.set_property("mute", main_mute);
        elements.push((main_volume_id.clone(), main_volume));

        // Main level meter (for main mix metering)
        let main_level_id = format!("{}:main_level", instance_id);
        let main_level = gst::ElementFactory::make("level")
            .name(&main_level_id)
            .property("interval", METER_INTERVAL_NS) // 100ms
            .property("post-messages", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("main level: {}", e)))?;
        elements.push((main_level_id.clone(), main_level));

        // Main output tee (allow-not-linked so unconnected main_out doesn't stall pipeline)
        let main_out_tee_id = format!("{}:main_out_tee", instance_id);
        let main_out_tee = gst::ElementFactory::make("tee")
            .name(&main_out_tee_id)
            .property("allow-not-linked", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("main_out_tee: {}", e)))?;
        elements.push((main_out_tee_id.clone(), main_out_tee));

        // Link: mixer → main_comp → main_eq → main_limiter → main_volume → main_level → main_out_tee
        internal_links.push((
            ElementPadRef::pad(&mixer_out_id, "src"),
            ElementPadRef::pad(&main_comp_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&main_comp_id, "src"),
            ElementPadRef::pad(&main_eq_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&main_eq_id, "src"),
            ElementPadRef::pad(&main_limiter_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&main_limiter_id, "src"),
            ElementPadRef::pad(&main_volume_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&main_volume_id, "src"),
            ElementPadRef::pad(&main_level_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&main_level_id, "src"),
            ElementPadRef::pad(&main_out_tee_id, "sink"),
        ));

        // ========================================================================
        // Create solo bus (sums PFL pre-fader + AFL post-fader sends from every
        // channel) and the Monitor bus that listens to either Main or solo.
        //
        // Routing rule (driven by the state layer as a side effect of any
        // ch{N}_pfl/ch{N}_afl write, not by clients):
        //   - no PFL/AFL active anywhere → main_to_mon=1, solo_to_mon=0
        //   - any PFL/AFL active        → main_to_mon=0, solo_to_mon=1
        // PFL/AFL bools are `persist: false`, so the build-time defaults below
        // are always the no-solo case — no need to inspect properties here.
        // ========================================================================
        let solo_mixer_id = format!("{}:solo_mixer", instance_id);
        // Solo takes each channel's PFL/AFL tap, and the AFL taps of the aux
        // and group buses.
        let solo_mixer = make_audiomixer(
            &solo_mixer_id,
            force_live,
            // Dynamic aux buses join it later: count the free ones too.
            bus_latency_ms(aux_capacity + num_groups > 0),
            min_upstream_latency_ms,
            start,
        )?;
        elements.push((solo_mixer_id.clone(), solo_mixer));
        let solo_out_id = spec.push_rate_pin(&solo_mixer_id, &mut elements, &mut internal_links)?;

        // Gates feeding monitor_mixer
        let solo_to_mon_id = format!("{}:solo_to_mon", instance_id);
        let solo_to_mon = gst::ElementFactory::make("volume")
            .name(&solo_to_mon_id)
            .property("volume", 0.0_f64)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("solo_to_mon: {}", e)))?;
        elements.push((solo_to_mon_id.clone(), solo_to_mon));

        let main_to_mon_id = format!("{}:main_to_mon", instance_id);
        let main_to_mon = gst::ElementFactory::make("volume")
            .name(&main_to_mon_id)
            .property("volume", 1.0_f64)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("main_to_mon: {}", e)))?;
        elements.push((main_to_mon_id.clone(), main_to_mon));

        // Small queues in front of monitor_mixer to decouple the two source
        // chains and avoid backpressure across them.
        let solo_to_mon_queue_id = format!("{}:solo_to_mon_queue", instance_id);
        let solo_to_mon_queue = gst::ElementFactory::make("queue")
            .name(&solo_to_mon_queue_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("solo_to_mon_queue: {}", e)))?;
        elements.push((solo_to_mon_queue_id.clone(), solo_to_mon_queue));

        let main_to_mon_queue_id = format!("{}:main_to_mon_queue", instance_id);
        let main_to_mon_queue = gst::ElementFactory::make("queue")
            .name(&main_to_mon_queue_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("main_to_mon_queue: {}", e)))?;
        elements.push((main_to_mon_queue_id.clone(), main_to_mon_queue));

        // Monitor summing mixer
        let monitor_mixer_id = format!("{}:monitor_mixer", instance_id);
        let monitor_mixer = make_audiomixer(
            &monitor_mixer_id,
            force_live,
            internal_bus_latency_ms,
            min_upstream_latency_ms,
            start,
        )?;
        elements.push((monitor_mixer_id.clone(), monitor_mixer));
        let monitor_out_id =
            spec.push_rate_pin(&monitor_mixer_id, &mut elements, &mut internal_links)?;

        // Monitor master volume (driven by `monitor_fader` exposed property)
        let monitor_master_vol_id = format!("{}:monitor_master_vol", instance_id);
        let monitor_master_level = get_float_prop(properties, "monitor_fader", 1.0);
        let monitor_master_vol = gst::ElementFactory::make("volume")
            .name(&monitor_master_vol_id)
            .property("volume", monitor_master_level)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("monitor master vol: {}", e)))?;
        elements.push((monitor_master_vol_id.clone(), monitor_master_vol));

        // Monitor level meter (broadcast as `{instance}:meter:monitor`)
        let monitor_level_id = format!("{}:monitor_level", instance_id);
        let monitor_level = gst::ElementFactory::make("level")
            .name(&monitor_level_id)
            .property("interval", METER_INTERVAL_NS)
            .property("post-messages", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("monitor_level: {}", e)))?;
        elements.push((monitor_level_id.clone(), monitor_level));

        // Monitor output tee (allow-not-linked so unconnected monitor_out
        // doesn't stall the pipeline)
        let monitor_out_tee_id = format!("{}:monitor_out_tee", instance_id);
        let monitor_out_tee = gst::ElementFactory::make("tee")
            .name(&monitor_out_tee_id)
            .property("allow-not-linked", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("monitor_out_tee: {}", e)))?;
        elements.push((monitor_out_tee_id.clone(), monitor_out_tee));

        // Wiring (`monitor_source` puts an aux bus's tee in place of Main's,
        // e.g. a headphone mix; the gate keeps its name and its rule):
        //   solo_mixer  → solo_to_mon  → solo_to_mon_queue ─┐
        //                                                   ├→ monitor_mixer → monitor_master_vol → monitor_level → monitor_out_tee
        //   main_out_tee→ main_to_mon  → main_to_mon_queue ─┘
        let monitor_source_tee_id = match monitor_source {
            Some(aux) => format!("{}:aux{}_out_tee", instance_id, aux),
            None => main_out_tee_id.clone(),
        };
        internal_links.push((
            ElementPadRef::pad(&solo_out_id, "src"),
            ElementPadRef::pad(&solo_to_mon_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&solo_to_mon_id, "src"),
            ElementPadRef::pad(&solo_to_mon_queue_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&solo_to_mon_queue_id, "src"),
            ElementPadRef::element(&monitor_mixer_id),
        ));
        internal_links.push((
            ElementPadRef::element(&monitor_source_tee_id),
            ElementPadRef::pad(&main_to_mon_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&main_to_mon_id, "src"),
            ElementPadRef::pad(&main_to_mon_queue_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&main_to_mon_queue_id, "src"),
            ElementPadRef::element(&monitor_mixer_id),
        ));
        internal_links.push((
            ElementPadRef::pad(&monitor_out_id, "src"),
            ElementPadRef::pad(&monitor_master_vol_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&monitor_master_vol_id, "src"),
            ElementPadRef::pad(&monitor_level_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&monitor_level_id, "src"),
            ElementPadRef::pad(&monitor_out_tee_id, "sink"),
        ));

        // ========================================================================
        // Create Aux buses
        // ========================================================================
        // The static buses leave through the block pads `aux_out_N`, the
        // dynamic ones through their inter channel.
        for aux in 0..num_aux_buses {
            aux_master(&spec, aux, properties, &mut elements, &mut internal_links)?;
        }
        for bus in &dynamic_aux_buses {
            aux_master(
                &spec,
                bus.aux - 1,
                properties,
                &mut elements,
                &mut internal_links,
            )?;
            aux_inter_output(
                &spec,
                bus.aux - 1,
                &bus.channel,
                &mut elements,
                &mut internal_links,
            )?;
        }

        // ========================================================================
        // Create Groups
        // ========================================================================
        for sg in 0..num_groups {
            let sg_mixer_id = format!("{}:group{}_mixer", instance_id, sg);
            let sg_mixer = make_audiomixer(
                &sg_mixer_id,
                force_live,
                latency_ms,
                min_upstream_latency_ms,
                start,
            )?;
            elements.push((sg_mixer_id.clone(), sg_mixer));
            let sg_out_id = spec.push_rate_pin(&sg_mixer_id, &mut elements, &mut internal_links)?;

            let sg_fader = get_float_prop(properties, &format!("group{}_fader", sg + 1), 1.0);
            let sg_mute = get_bool_prop(properties, &format!("group{}_mute", sg + 1), false);

            let sg_volume_id = format!("{}:group{}_volume", instance_id, sg);
            let sg_volume = gst::ElementFactory::make("volume")
                .name(&sg_volume_id)
                .property("volume", sg_fader)
                .property("mute", sg_mute)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("group{}_volume: {}", sg, e))
                })?;
            elements.push((sg_volume_id.clone(), sg_volume));

            let sg_level_id = format!("{}:group{}_level", instance_id, sg);
            let sg_level = gst::ElementFactory::make("level")
                .name(&sg_level_id)
                .property("interval", METER_INTERVAL_NS)
                .property("post-messages", true)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("group{}_level: {}", sg, e))
                })?;
            elements.push((sg_level_id.clone(), sg_level));

            // Group output tee - allows both external output AND feeding main mixer
            // Also prevents NOT_LINKED when group_out isn't connected externally.
            let sg_out_tee_id = format!("{}:group{}_out_tee", instance_id, sg);
            let sg_out_tee = gst::ElementFactory::make("tee")
                .name(&sg_out_tee_id)
                .property("allow-not-linked", true)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("group{}_out_tee: {}", sg, e))
                })?;
            elements.push((sg_out_tee_id.clone(), sg_out_tee));

            // Queue between group tee and main mixer (isolates scheduling)
            let sg_to_main_queue_id = format!("{}:group{}_to_main_queue", instance_id, sg);
            let sg_to_main_queue = gst::ElementFactory::make("queue")
                .name(&sg_to_main_queue_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("group{}_to_main_queue: {}", sg, e))
                })?;
            elements.push((sg_to_main_queue_id.clone(), sg_to_main_queue));

            // Link: group_mixer → group_volume → group_level → group_out_tee
            //        group_out_tee → queue → main audiomixer
            internal_links.push((
                ElementPadRef::pad(&sg_out_id, "src"),
                ElementPadRef::pad(&sg_volume_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&sg_volume_id, "src"),
                ElementPadRef::pad(&sg_level_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&sg_level_id, "src"),
                ElementPadRef::pad(&sg_out_tee_id, "sink"),
            ));
            // One branch from tee feeds the main mixer
            internal_links.push((
                ElementPadRef::element(&sg_out_tee_id),
                ElementPadRef::pad(&sg_to_main_queue_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&sg_to_main_queue_id, "src"),
                ElementPadRef::element(&mixer_id), // Request pad from main audiomixer
            ));

            // AFL tap on the group master. Taps from `group_out_tee`
            // (post-master, post-mute, post-meter) so the operator hears the
            // summed group exactly as it leaves the bus. Gate volume is 0/1
            // via `bool_to_volume`.
            let sg_afl_enabled = get_bool_prop(properties, &format!("group{}_afl", sg + 1), false);
            let sg_afl_volume_id = format!("{}:group{}_afl_volume", instance_id, sg);
            let sg_afl_volume = gst::ElementFactory::make("volume")
                .name(&sg_afl_volume_id)
                .property("volume", if sg_afl_enabled { 1.0 } else { 0.0 })
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("group{}_afl_volume: {}", sg, e))
                })?;
            elements.push((sg_afl_volume_id.clone(), sg_afl_volume));

            let sg_afl_queue_id = format!("{}:group{}_afl_queue", instance_id, sg);
            let sg_afl_queue = gst::ElementFactory::make("queue")
                .name(&sg_afl_queue_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("group{}_afl_queue: {}", sg, e))
                })?;
            elements.push((sg_afl_queue_id.clone(), sg_afl_queue));

            // group_out_tee → group_afl_volume → group_afl_queue → solo_mixer
            internal_links.push((
                ElementPadRef::element(&sg_out_tee_id),
                ElementPadRef::pad(&sg_afl_volume_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&sg_afl_volume_id, "src"),
                ElementPadRef::pad(&sg_afl_queue_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&sg_afl_queue_id, "src"),
                ElementPadRef::element(&solo_mixer_id),
            ));
        }

        // ========================================================================
        // Create per-channel processing: the static channels, then the
        // dynamic ones (their sound from an inter channel)
        // ========================================================================
        let routing = ChannelRouting {
            dsp_backend,
            aux_buses: &built_aux_buses,
            num_groups,
        };
        for ch in 0..num_channels {
            channel_strip(
                &spec,
                ch,
                &routing,
                properties,
                &mut elements,
                &mut internal_links,
            )?;
        }
        for dynamic in &dynamic_channels {
            let ch = dynamic.channel - 1;
            channel_inter_input(&spec, ch, dynamic, &mut elements, &mut internal_links)?;
            channel_strip(
                &spec,
                ch,
                &routing,
                properties,
                &mut elements,
                &mut internal_links,
            )?;
        }

        info!(
            "Mixer block created with {} channels and {} dynamic ones",
            num_channels,
            dynamic_channels.len()
        );

        // Create bus message handler for metering
        let handler_instance_id = instance_id.to_string();
        let bus_message_handler = Some(Box::new(
            move |bus: &gst::Bus, flow_id: FlowId, events: EventBroadcaster| {
                connect_mixer_meter_handler(bus, flow_id, events, handler_instance_id.clone())
            },
        ) as crate::blocks::BusMessageConnectFn);

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler,
            pad_properties: HashMap::new(),
        })
    }
}
