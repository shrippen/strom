//! The PGM take engine — `trigger_transition` with the PiP-aware path
//! (cut/fade across compositor regions) and the classic single-input path.

use super::super::{PipelineError, PipelineManager};
use gstreamer::prelude::*;
use tracing::{debug, info};

use crate::gst::crop::set_pad_crop;

use super::mixer_layout::{
    apply_input_group_to_region, apply_pip_crop_after_morph, apply_pip_layout_to_region,
    pads_for_source,
};

impl PipelineManager {
    /// Trigger a transition on a compositor/mixer block.
    ///
    /// Reads the authoritative PGM/PVW source from overlay state. When either
    /// bus is a PiP, takes the source-aware path that animates pads across
    /// dist + multiview compositors; otherwise runs a standard single-pad
    /// transition on the mv mixer.
    ///
    /// Returns `(was_ftb_cancelled, old_pgm, new_pgm, actual_kind)`. The two
    /// middle elements are `None` when the corresponding bus is a PiP source.
    /// `actual_kind` is the transition that actually ran — differs from
    /// `transition_type` when the engine downgraded the request (e.g. Slide
    /// across heterogeneous PiP/input sources downgrades to "fade").
    pub fn trigger_transition(
        &self,
        block_instance_id: &str,
        from_input: usize,
        to_input: usize,
        transition_type: &str,
        duration_ms: u64,
    ) -> Result<(bool, Option<usize>, Option<usize>, String), PipelineError> {
        use crate::gst::transitions::{TransitionController, TransitionType};

        debug!(
            "Triggering {} transition on {} from input {} to {} ({}ms)",
            transition_type, block_instance_id, from_input, to_input, duration_ms
        );

        // Find the mixer element for this block
        let mixer_id = format!("{}:mixer", block_instance_id);
        let mixer = self
            .elements
            .get(&mixer_id)
            .ok_or_else(|| PipelineError::ElementNotFound(mixer_id.clone()))?;

        // Read authoritative PGM/PVW source from overlay state. `None` means
        // the bus is a PiP — handled by the PiP-aware branch below.
        let overlay_state = crate::blocks::builtin::vision_mixer::overlay::get_overlay_state(
            &self.flow_id,
            block_instance_id,
        );
        let num_video_inputs = overlay_state
            .as_ref()
            .map(|s| s.num_inputs)
            .unwrap_or(usize::MAX);
        // When overlay state is missing entirely, fall back to the request-
        // provided indices. When state exists but reports None (PiP on bus),
        // pass None through so the PiP-aware branch below handles it.
        let old_pgm: Option<usize> = match overlay_state.as_ref() {
            Some(s) => s.pgm_input(),
            None => Some(from_input),
        };
        let new_pgm: Option<usize> = match overlay_state.as_ref() {
            Some(s) => s.pvw_input(),
            None => Some(to_input),
        };

        // An unknown transition is refused before anything changes.
        let parsed_type = transition_type.parse::<TransitionType>().map_err(|_| {
            PipelineError::InvalidProperty {
                element: block_instance_id.to_string(),
                property: "transition_type".to_string(),
                reason: format!("Unknown transition type: {}", transition_type),
            }
        })?;

        // Auto-cancel FTB if active. Zone-border underlays need no special
        // handling here: they are regular pads driven by the take paths
        // below exactly like their content pads.
        let was_ftb = overlay_state
            .as_ref()
            .map(|s| {
                s.ftb_active
                    .swap(false, std::sync::atomic::Ordering::Relaxed)
            })
            .unwrap_or(false);
        if was_ftb {
            info!(
                "Auto-cancelling FTB before transition on {}",
                block_instance_id
            );
        }

        // Reset transition shader slots (uniform-only) so an interrupted
        // wipe or master envelope can't linger into this take. No-op when
        // the FX engine isn't built.
        self.reset_take_fx(block_instance_id);

        let (canvas_width, canvas_height) = self.dist_canvas_size(block_instance_id);

        // --- PiP-aware Take ---
        // If either bus is currently a PiP (or will become one via swap), use a
        // Source-aware path. Cut snaps geometry+alpha; Fade cross-fades alphas
        // across both compositor regions (dist for PGM, mv PVW for PVW).
        // Slide/Push/Dip-to-Black with PiP downgrade to Fade — explicit position
        // animation across heterogeneous Source kinds isn't supported yet.
        if let Some(state) = overlay_state.as_ref() {
            let old_pgm_pip = state.pgm_pip();
            let old_pvw_pip = state.pvw_pip();
            if old_pgm_pip.is_some() || old_pvw_pip.is_some() {
                let mv_comp_id = format!("{}:mv_comp", block_instance_id);
                let mv_comp = self
                    .elements
                    .get(&mv_comp_id)
                    .ok_or_else(|| PipelineError::ElementNotFound(mv_comp_id.clone()))?;

                let parsed = Some(parsed_type);
                let is_cut = duration_ms == 0 || matches!(parsed, Some(TransitionType::Cut));
                // Explicit position animation across heterogeneous Source kinds
                // (input ↔ PiP) isn't supported yet — Slide/Push/Dip-to-Black
                // silently degrade to Fade in this branch. Surface that in the
                // log so operators don't think the requested transition ran.
                if !is_cut
                    && !matches!(
                        parsed,
                        Some(TransitionType::Fade) | Some(TransitionType::Cut)
                    )
                {
                    info!(
                        "PiP-aware Take on {}: transition '{}' downgraded to Fade ({}ms) — non-Fade transitions across PiPs not supported yet",
                        block_instance_id, transition_type, duration_ms
                    );
                }

                // Swap Source state PVW ↔ PGM.
                let new_pgm_pip = old_pvw_pip;
                let new_pvw_pip = old_pgm_pip;
                let new_pgm_swap = state.pvw_input();
                let new_pvw_swap = state.pgm_input();

                let dist_region = (0, 0, canvas_width, canvas_height);
                let r = &state.layout.pvw_rect;
                let pvw_region = (r.x as i32, r.y as i32, r.w as i32, r.h as i32);
                // Fallback aspect = PGM canvas aspect (typically 16:9), used
                // for tile-grid cells and for inputs whose caps are unknown.
                let src_aspect = if canvas_height > 0 {
                    canvas_width as f64 / canvas_height as f64
                } else {
                    16.0 / 9.0
                };
                let src_aspects =
                    self.vision_mixer_source_aspects(block_instance_id, state.num_inputs);
                let pgm_w = state.pgm_w.max(1) as f64;
                let dist_underlay =
                    state
                        .dist_underlay_base()
                        .map(|base| crate::gst::underlay::UnderlayCtx {
                            base,
                            scale: canvas_width as f64 / pgm_w,
                        });
                let pvw_underlay =
                    state
                        .mv_pvw_underlay_base()
                        .map(|base| crate::gst::underlay::UnderlayCtx {
                            base,
                            scale: state.layout.pvw_rect.w / pgm_w,
                        });

                let old_dist_targets = pads_for_source(
                    state,
                    old_pgm_pip,
                    old_pgm,
                    dist_region,
                    0,
                    strom_types::vision_mixer::DIST_PGM_ZORDER,
                    strom_types::vision_mixer::DIST_PIP_OVERLAY_ZORDER,
                    src_aspect,
                    &src_aspects,
                    dist_underlay,
                );
                let new_dist_targets = pads_for_source(
                    state,
                    new_pgm_pip,
                    new_pgm_swap,
                    dist_region,
                    0,
                    strom_types::vision_mixer::DIST_PGM_ZORDER,
                    strom_types::vision_mixer::DIST_PIP_OVERLAY_ZORDER,
                    src_aspect,
                    &src_aspects,
                    dist_underlay,
                );
                let old_pvw_targets = pads_for_source(
                    state,
                    old_pvw_pip,
                    state.pvw_input(),
                    pvw_region,
                    state.num_inputs + 1,
                    strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER,
                    strom_types::vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
                    src_aspect,
                    &src_aspects,
                    pvw_underlay,
                );
                let new_pvw_targets = pads_for_source(
                    state,
                    new_pvw_pip,
                    new_pvw_swap,
                    pvw_region,
                    state.num_inputs + 1,
                    strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER,
                    strom_types::vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
                    src_aspect,
                    &src_aspects,
                    pvw_underlay,
                );

                if is_cut {
                    // Snap apply: hide everything in the region, then set new active pads.
                    if let Some(p) = new_pgm_pip {
                        apply_pip_layout_to_region(
                            mixer,
                            0,
                            state.num_inputs,
                            dist_region,
                            state.pip_bg_input(p),
                            &state.pip_zones(p),
                            &state.pip_transforms(p),
                            strom_types::vision_mixer::DIST_PGM_ZORDER,
                            strom_types::vision_mixer::DIST_PIP_OVERLAY_ZORDER,
                            src_aspect,
                            &src_aspects,
                            dist_underlay,
                        );
                    } else {
                        apply_input_group_to_region(
                            mixer,
                            0,
                            state.num_inputs,
                            dist_region,
                            new_pgm_swap,
                            strom_types::vision_mixer::DIST_PGM_ZORDER,
                            src_aspect,
                            &src_aspects,
                            dist_underlay,
                        );
                    }
                    if let Some(p) = new_pvw_pip {
                        apply_pip_layout_to_region(
                            mv_comp,
                            state.num_inputs + 1,
                            state.num_inputs,
                            pvw_region,
                            state.pip_bg_input(p),
                            &state.pip_zones(p),
                            &state.pip_transforms(p),
                            strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER,
                            strom_types::vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
                            src_aspect,
                            &src_aspects,
                            pvw_underlay,
                        );
                    } else {
                        apply_input_group_to_region(
                            mv_comp,
                            state.num_inputs + 1,
                            state.num_inputs,
                            pvw_region,
                            new_pvw_swap,
                            strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER,
                            src_aspect,
                            &src_aspects,
                            pvw_underlay,
                        );
                    }
                } else {
                    // Morph transition: sources shared between old and new
                    // animate position+size; fresh sources alpha-fade in; departing
                    // sources alpha-fade out. Runs in parallel on both dist (PGM)
                    // and mv PVW big regions.
                    // Only program's pads are on air before the morph: an
                    // input a held T-bar showed goes off first (the morph
                    // would leave it, or move it from where the bar put it).
                    use gstreamer_controller::prelude::*;
                    for pad in mixer.sink_pads() {
                        let Some(idx) = pad
                            .name()
                            .strip_prefix("sink_")
                            .and_then(|i| i.parse::<usize>().ok())
                            .filter(|idx| *idx < state.num_inputs)
                        else {
                            continue;
                        };
                        // Set by the bar: visible, no fade programmed.
                        let animated = pad
                            .control_binding("alpha")
                            .and_then(|b| {
                                b.property::<Option<gstreamer::ControlSource>>("control-source")
                            })
                            .and_then(|cs| {
                                cs.downcast::<gstreamer_controller::InterpolationControlSource>()
                                    .ok()
                            })
                            .is_some_and(|cs| !cs.list_control_points().is_empty());
                        if old_dist_targets.iter().all(|t| t.pad_idx != idx)
                            && pad.property::<f64>("alpha") > 0.0
                            && !animated
                        {
                            crate::gst::control_bindings::wipe_control_bindings(
                                pad.upcast_ref(),
                                &["alpha"],
                            );
                            pad.set_property("alpha", 0.0f64);
                        }
                    }
                    let dist_controller =
                        TransitionController::new(mixer.clone(), canvas_width, canvas_height);
                    dist_controller
                        .animate_pad_transition(
                            &old_dist_targets,
                            &new_dist_targets,
                            duration_ms,
                            &self.pipeline,
                        )
                        .map_err(|e| PipelineError::TransitionError(e.to_string()))?;

                    let mv_controller = TransitionController::new(
                        mv_comp.clone(),
                        state.layout.canvas_width as i32,
                        state.layout.canvas_height as i32,
                    );
                    mv_controller
                        .animate_pad_transition(
                            &old_pvw_targets,
                            &new_pvw_targets,
                            duration_ms,
                            &self.pipeline,
                        )
                        .map_err(|e| PipelineError::TransitionError(e.to_string()))?;

                    // Apply per-source crop for the incoming compositions:
                    // pads shared between old and new (e.g. a source that is
                    // fullscreen on PGM and cropped in the incoming PiP)
                    // animate their crop in sync with the geometry morph;
                    // fresh pads snap; fading-out pads keep their crop until
                    // hidden. An input-mode side uses an empty transform map,
                    // so shared pads animate their punch-out back to no crop.
                    let new_pgm_transforms = new_pgm_pip
                        .map(|p| state.pip_transforms(p))
                        .unwrap_or_default();
                    apply_pip_crop_after_morph(
                        mixer,
                        &dist_controller,
                        &self.pipeline,
                        &new_pgm_transforms,
                        state.num_inputs,
                        0,
                        &old_dist_targets,
                        &new_dist_targets,
                        duration_ms,
                    );
                    let new_pvw_transforms = new_pvw_pip
                        .map(|p| state.pip_transforms(p))
                        .unwrap_or_default();
                    apply_pip_crop_after_morph(
                        mv_comp,
                        &mv_controller,
                        &self.pipeline,
                        &new_pvw_transforms,
                        state.num_inputs,
                        state.num_inputs + 1,
                        &old_pvw_targets,
                        &new_pvw_targets,
                        duration_ms,
                    );

                    // Master-FX takes keep their full-frame envelope on the
                    // PiP path too — the pad animation underneath is the
                    // fade above, but the glitch/flash/punch still lands.
                    if let Some(TransitionType::MasterFx(kind)) = parsed {
                        if self.vision_mixer_fx_available(block_instance_id) {
                            if let Ok(now) = dist_controller.current_stream_time(&self.pipeline) {
                                let _ = self.apply_master_envelope(
                                    block_instance_id,
                                    kind,
                                    now,
                                    duration_ms,
                                );
                            }
                        }
                    }
                }

                // Persist new state.
                state.set_pgm_pip(new_pgm_pip);
                state.set_pvw_pip(new_pvw_pip);
                state.set_pgm_input(new_pgm_swap);
                state.set_pvw_input(new_pvw_swap);
                crate::blocks::builtin::vision_mixer::overlay::trigger_overlay_update(
                    &self.flow_id,
                    block_instance_id,
                );

                info!(
                    "PiP-aware Take on {} ({}{}ms): PGM pip={:?} input={:?}; PVW pip={:?} input={:?}",
                    block_instance_id,
                    if is_cut { "Cut, " } else { "Fade, " },
                    duration_ms,
                    new_pgm_pip,
                    new_pgm_swap,
                    new_pvw_pip,
                    new_pvw_swap,
                );
                // After the downgrade guard above, the PiP-aware branch only
                // ever runs a Cut or a Fade.
                let actual_kind = if is_cut { "cut" } else { "fade" }.to_string();
                return Ok((was_ftb, old_pgm, new_pgm_swap, actual_kind));
            }
        }

        let trans_type = parsed_type;

        // Reset all video pads to a clean state before the transition:
        // clear control bindings, restore alpha/position/size for the current
        // PGM input. Single-input PGM only (multi-source compositions are PiPs).
        // A cut rests the new program straight away: from a held T-bar the
        // old one would otherwise come back whole for a frame.
        let is_cut = duration_ms == 0 || matches!(trans_type, TransitionType::Cut);
        self.reset_classic_take_pads(
            block_instance_id,
            mixer,
            overlay_state.as_deref(),
            if is_cut { new_pgm } else { old_pgm },
            num_video_inputs,
            canvas_width,
            canvas_height,
        );

        // Single-input transition only. Multi-source compositions are now
        // expressed as PiPs which are handled by the PiP-aware branch above.
        let from = old_pgm.unwrap_or(from_input);
        let to = new_pgm.unwrap_or(to_input);
        let controller = TransitionController::new(mixer.clone(), canvas_width, canvas_height);

        // Shader transitions need the FX engine — downgrade to Fade when it
        // isn't built (CPU backend or Shader FX disabled).
        let trans_type = match trans_type {
            TransitionType::Wipe(_) | TransitionType::MasterFx(_)
                if !self.vision_mixer_fx_available(block_instance_id) =>
            {
                info!(
                    "Transition '{}' downgraded to Fade on {} — shader FX engine not available",
                    transition_type, block_instance_id
                );
                TransitionType::Fade
            }
            t => t,
        };
        // Without a duration every transition is a cut: the pads already
        // rest on the new program, and a slide, push or dip would first put
        // it out of the picture again.
        let trans_type = if is_cut {
            TransitionType::Cut
        } else {
            trans_type
        };

        match trans_type {
            TransitionType::Wipe(kind) => {
                self.shader_wipe_take(block_instance_id, &controller, from, to, kind, duration_ms)?;
            }
            TransitionType::MasterFx(kind) => {
                self.master_fx_take(block_instance_id, &controller, from, to, kind, duration_ms)?;
            }
            t => {
                controller
                    .transition(from, to, t, duration_ms, &self.pipeline)
                    .map_err(|e| PipelineError::TransitionError(e.to_string()))?;
            }
        }

        let actual_kind = if duration_ms == 0 {
            TransitionType::Cut.to_string()
        } else {
            trans_type.to_string()
        };
        Ok((was_ftb, old_pgm, new_pgm, actual_kind))
    }
    /// Hold the take from program to preview at `position` (a T-bar):
    /// the input pads show `transition_type` at that point (see
    /// `transitions::tbar`); DSKs and the rest keep what they do. A PiP on
    /// program or preview is refused, except at position 0, which always
    /// puts program back as it was (with a PiP on program, as it is). A cut (at the end) finishes it; a take with
    /// a duration, or a stinger, starts from program as it rests, so the
    /// held picture jumps back first. Nothing remembers the hold: a
    /// geometry refresh (an input changing size) rests the pads too.
    pub fn hold_vision_mixer_transition(
        &self,
        block_instance_id: &str,
        transition_type: &str,
        position: f64,
    ) -> Result<(), PipelineError> {
        use crate::gst::transitions::TransitionType;
        use strom_types::vision_mixer::{aspect_fit_rect, DIST_PGM_ZORDER};

        let invalid = |reason: String| PipelineError::InvalidProperty {
            element: block_instance_id.to_string(),
            property: "tbar".to_string(),
            reason,
        };
        if !(0.0..=1.0).contains(&position) {
            return Err(invalid(format!("position {} is outside 0..1", position)));
        }
        let trans_type = transition_type
            .parse::<TransitionType>()
            .map_err(|_| invalid(format!("Unknown transition type: {}", transition_type)))?;
        let mixer_id = format!("{}:mixer", block_instance_id);
        let mixer = self
            .elements
            .get(&mixer_id)
            .ok_or_else(|| PipelineError::ElementNotFound(mixer_id.clone()))?;
        let state = crate::blocks::builtin::vision_mixer::overlay::get_overlay_state(
            &self.flow_id,
            block_instance_id,
        )
        .ok_or_else(|| PipelineError::ElementNotFound(block_instance_id.to_string()))?;
        // Setting the pads would bring program back under the black while
        // fade to black stays on.
        if state.ftb_active.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(invalid("fade to black is on".to_string()));
        }
        // A take still animating, or program fading back from black,
        // would jump to the bar's values: the bar waits until they land.
        if let Some(left) =
            crate::blocks::builtin::vision_mixer::geometry::inputs_animation_remaining(
                mixer,
                state.num_inputs,
            )
        {
            return Err(invalid(format!(
                "a transition is still running ({} ms left)",
                left.as_millis()
            )));
        }
        if state.pgm_pip().is_some() {
            // Nothing can be held with a PiP on program (a take to it rests
            // the pads): at 0 there is nothing to put back.
            if position == 0.0 {
                return Ok(());
            }
            return Err(invalid("the T-bar mixes inputs only, not PiPs".to_string()));
        }
        let Some(from) = state.pgm_input() else {
            return Err(invalid("needs an input on program".to_string()));
        };
        let (canvas_width, canvas_height) = self.dist_canvas_size(block_instance_id);
        let to = match state.pvw_input() {
            Some(to) if to != from && position > 0.0 => to,
            // Position 0 (or nothing to mix): program as it rests,
            // whatever preview has become meanwhile.
            _ if position == 0.0 || state.pvw_pip().is_none() => {
                self.reset_take_pads(
                    block_instance_id,
                    mixer,
                    Some(&state),
                    Some(from),
                    state.num_inputs,
                    canvas_width,
                    canvas_height,
                    false,
                );
                return Ok(());
            }
            _ => return Err(invalid("the T-bar mixes inputs only, not PiPs".to_string())),
        };

        // The bar shows wipes and master FX as a fade. The inputs' take
        // slots go neutral (an inverted wipe left the outgoing input
        // masked away; the bar only runs once its keyframes are past). The
        // program's take slot is left alone: an envelope still decaying
        // from a take outlasts the pads' keyframes and ends by itself.
        self.reset_input_take_fx(block_instance_id);
        let aspects = self.vision_mixer_source_aspects(block_instance_id, state.num_inputs);
        let canvas_aspect = if canvas_height > 0 {
            canvas_width as f64 / canvas_height as f64
        } else {
            16.0 / 9.0
        };
        let rest = |idx: usize| {
            let aspect = aspects.get(&idx).copied().unwrap_or(canvas_aspect);
            aspect_fit_rect(0, 0, canvas_width, canvas_height, aspect)
        };
        let (from_rect, to_rect) = (rest(from), rest(to));
        let frame = crate::gst::transitions::tbar::tbar_frame(
            trans_type,
            position,
            canvas_width,
            canvas_height,
            crate::gst::transitions::plan::rect_contains(to_rect, from_rect),
        );
        let moved =
            |(x, y, w, h): (i32, i32, i32, i32), (dx, dy): (i32, i32)| (x + dx, y + dy, w, h);

        // Each pad goes straight to its values, never through its resting
        // place: the mixer renders between two property writes, and a
        // detour would flash as a frame on air.
        let mut bar_pads = (None, None);
        let mut others = Vec::new();
        for pad in mixer.sink_pads() {
            let Some(idx) = pad
                .name()
                .strip_prefix("sink_")
                .and_then(|i| i.parse::<usize>().ok())
                .filter(|idx| *idx < state.num_inputs)
            else {
                continue;
            };
            crate::gst::control_bindings::wipe_control_bindings(
                pad.upcast_ref(),
                &["alpha", "xpos", "ypos", "width", "height", "zorder"],
            );
            set_pad_crop(&pad, &Default::default());
            if idx == from {
                bar_pads.0 = Some(pad);
            } else if idx == to {
                bar_pads.1 = Some(pad);
            } else {
                others.push((idx, pad));
            }
        }
        let (Some(from_pad), Some(to_pad)) = bar_pads else {
            return Err(PipelineError::ElementNotFound(format!(
                "{}:mixer sink_{} or sink_{}",
                block_instance_id, from, to
            )));
        };
        // Between the two pads the same: towards preview the incoming one
        // (on top) moves first, back towards program the outgoing one, so a
        // push never opens a gap between them for a frame.
        let to_now = (
            to_pad.property::<i32>("xpos"),
            to_pad.property::<i32>("ypos"),
        );
        let distance = |(x, y): (i32, i32)| (x - to_rect.0).abs() + (y - to_rect.1).abs();
        // An incoming pad that gets fainter (the effect changed under the
        // held bar) goes last, so the outgoing one covers first.
        let to_alpha_now = to_pad.property::<f64>("alpha");
        let towards_preview = frame.to_alpha >= to_alpha_now
            && (frame.to_alpha > to_alpha_now
                || distance({
                    let (x, y, _, _) = moved(to_rect, frame.to_offset);
                    (x, y)
                }) < distance(to_now));
        let from_values = (
            moved(from_rect, frame.from_offset),
            frame.from_alpha,
            Some(DIST_PGM_ZORDER),
        );
        let to_values = (
            moved(to_rect, frame.to_offset),
            frame.to_alpha,
            Some(DIST_PGM_ZORDER + 1),
        );
        let order = if towards_preview {
            [(&to_pad, to_values), (&from_pad, from_values)]
        } else {
            [(&from_pad, from_values), (&to_pad, to_values)]
        };
        for (pad, (rect, alpha, zorder)) in order {
            set_bar_pad(pad, rect, alpha, zorder);
        }
        // The rest goes off last: a preview chosen while the bar was held
        // replaces the old one only once the new one is in its place.
        for (idx, pad) in &others {
            set_bar_pad(pad, rest(*idx), 0.0, None);
        }
        Ok(())
    }

    /// Put every dist mixer pad in its resting state for an input-to-input
    /// take: animation bindings neutralised, inputs aspect-fit to the canvas
    /// with only `active_pgm_input` visible, DSKs as switched, everything
    /// else (border underlays, stinger pads) hidden.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reset_classic_take_pads(
        &self,
        block_instance_id: &str,
        mixer: &gstreamer::Element,
        overlay_state: Option<
            &crate::blocks::builtin::vision_mixer::overlay::VisionMixerOverlayState,
        >,
        active_pgm_input: Option<usize>,
        num_video_inputs: usize,
        canvas_width: i32,
        canvas_height: i32,
    ) {
        self.reset_take_pads(
            block_instance_id,
            mixer,
            overlay_state,
            active_pgm_input,
            num_video_inputs,
            canvas_width,
            canvas_height,
            true,
        );
    }

    /// [`Self::reset_classic_take_pads`]; without `others`, only the
    /// input pads, so DSK, underlay and stinger pads keep what they do.
    #[allow(clippy::too_many_arguments)]
    fn reset_take_pads(
        &self,
        block_instance_id: &str,
        mixer: &gstreamer::Element,
        overlay_state: Option<
            &crate::blocks::builtin::vision_mixer::overlay::VisionMixerOverlayState,
        >,
        active_pgm_input: Option<usize>,
        num_video_inputs: usize,
        canvas_width: i32,
        canvas_height: i32,
        others: bool,
    ) {
        let classic_aspects = self.vision_mixer_source_aspects(
            block_instance_id,
            if num_video_inputs == usize::MAX {
                0
            } else {
                num_video_inputs
            },
        );
        let canvas_aspect = if canvas_height > 0 {
            canvas_width as f64 / canvas_height as f64
        } else {
            16.0 / 9.0
        };

        // Explicit geometry: aspect-fit each source into the canvas (pads
        // run sizing-policy=none). Classic takes are input↔input — wipe any
        // crop left behind by an earlier PiP render on these pads.
        let rest = |pad: &gstreamer::Pad, idx: usize, alpha: f64, zorder: Option<u32>| {
            set_pad_crop(pad, &Default::default());
            let aspect = classic_aspects.get(&idx).copied().unwrap_or(canvas_aspect);
            let rect = strom_types::vision_mixer::aspect_fit_rect(
                0,
                0,
                canvas_width,
                canvas_height,
                aspect,
            );
            set_bar_pad(pad, rect, alpha, zorder);
        };
        let mut active = None;
        let mut hidden = Vec::new();
        for pad in mixer.sink_pads() {
            let name = pad.name();
            if name.starts_with("sink_") {
                if let Ok(idx) = name.trim_start_matches("sink_").parse::<usize>() {
                    if !others && idx >= num_video_inputs {
                        continue;
                    }
                    // Neutralize lingering animation bindings (keyframe wipe
                    // — never removed, see crate::gst::control_bindings).
                    crate::gst::control_bindings::wipe_control_bindings(
                        pad.upcast_ref(),
                        &["alpha", "xpos", "ypos", "width", "height"],
                    );
                    if idx < num_video_inputs {
                        if Some(idx) == active_pgm_input {
                            active = Some(pad.clone());
                        } else {
                            hidden.push((idx, pad.clone()));
                        }
                    } else if let Some(state) = overlay_state {
                        let dsk_idx = idx - num_video_inputs;
                        if dsk_idx < state.dsk_enabled.len() {
                            let enabled = state.dsk_enabled[dsk_idx]
                                .load(std::sync::atomic::Ordering::Relaxed);
                            let alpha = if enabled { 1.0f64 } else { 0.0f64 };
                            pad.set_property("alpha", alpha);
                        } else {
                            // Border underlay and stinger pads: classic takes
                            // are input↔input — no zones on PGM, no borders,
                            // no stinger on air.
                            pad.set_property("alpha", 0.0f64);
                        }
                    }
                }
            }
        }
        // Program first, then the others out, then program on top: a frame
        // rendered between two writes shows the old or the new picture (a
        // take from a held T-bar starts half way), never neither.
        if let (Some(pad), Some(idx)) = (&active, active_pgm_input) {
            rest(pad, idx, 1.0, None);
        }
        for (idx, pad) in &hidden {
            rest(pad, *idx, 0.0, None);
        }
        if let (Some(pad), Some(idx)) = (&active, active_pgm_input) {
            rest(
                pad,
                idx,
                1.0,
                Some(strom_types::vision_mixer::DIST_PGM_ZORDER),
            );
        }
    }
}

/// Put `pad` at `rect` and `alpha` (and `zorder`), writing only what
/// changes, in an order a frame between two writes cannot catch half way:
/// a pad that gets fainter fades before it moves, one that gets stronger
/// moves before it shows.
fn set_bar_pad(pad: &gstreamer::Pad, rect: (i32, i32, i32, i32), alpha: f64, zorder: Option<u32>) {
    let fainter = alpha < pad.property::<f64>("alpha");
    if fainter {
        pad.set_property("alpha", alpha);
    }
    let (x, y, w, h) = rect;
    for (name, value) in [("xpos", x), ("ypos", y), ("width", w), ("height", h)] {
        if pad.property::<i32>(name) != value {
            pad.set_property(name, value);
        }
    }
    if let Some(z) = zorder {
        if pad.property::<u32>("zorder") != z {
            pad.set_property("zorder", z);
        }
    }
    if !fainter && pad.property::<f64>("alpha") != alpha {
        pad.set_property("alpha", alpha);
    }
}
