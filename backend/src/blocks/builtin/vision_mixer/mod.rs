//! Vision Mixer block — TV broadcast production switcher.
//!
//! A source selector with PVW/PGM workflow and transition support.
//! Takes configurable video inputs (`MIN_NUM_INPUTS` to `MAX_NUM_INPUTS`,
//! see strom-types), outputs a high-res PGM stream and a broadcast multiview
//! with cairo-drawn overlays. With `max_inputs` above `num_inputs`, inputs
//! can also be added and removed while it runs (see [`dynamic`]).
//!
//! Internally tees each input to two compositors:
//! - **Distribution compositor** ("mixer"): full-res PGM output with transitions
//! - **Multiview compositor** ("mv_comp"): PVW/PGM large panels over a
//!   thumbnail grid that grows with the number of inputs, with borders,
//!   labels, and clock drawn by a cairo overlay pad
//!
//! GPU pipeline (per input; pad indices come from `strom_types::vision_mixer::PadMap`):
//! ```text
//! queue_i → glupload_i → glcolorconvert_i → [fx_look_i] → tee_i
//!   tee_i.src_0 → queue_to_dist_i → [fx_take_i] → mixer.sink_i
//!   tee_i.src_1 → queue_to_mv_thumb_i → mv_comp.sink_i       (thumbnail)
//!   tee_i.src_2 → queue_to_mv_pvw_i → mv_comp.sink_{N+1+i}   (PVW big display candidate)
//! ```
//! The CPU and CUDA pipelines have the same branches, with a convert (and
//! on CUDA an upload) in front of the tee and a `videocrop` in front of
//! every croppable compositor pad.
//!
//! The outputs (see the builders for the exact chains): `mixer` feeds
//! `tee_pgm` and `[pgm_out]` through `capsfilter_dist`, `mv_comp` feeds
//! `[multiview_out]` through `capsfilter_mv`. The multiview overlay is an
//! `appsrc` pad on `mv_comp`, not a cairooverlay element.

mod builder;
mod definition;
pub mod dynamic;
mod elements;
pub(crate) mod geometry;
pub(crate) mod layout;
pub mod overlay;
pub(crate) mod properties;
#[cfg(test)]
mod tests;
pub(crate) mod underlays;

/// Block definition id.
pub const BLOCK_ID: &str = "builtin.vision_mixer";

// Public API
pub use builder::VisionMixerBuilder;
pub use definition::get_blocks;
pub use overlay::VisionMixerOverlayState;
pub use properties::apply_live_label;
