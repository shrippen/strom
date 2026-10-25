//! Vision Mixer block — TV broadcast production switcher.
//!
//! A source selector with PVW/PGM workflow and transition support.
//! Takes configurable video inputs (`MIN_NUM_INPUTS` to `MAX_NUM_INPUTS`,
//! see strom-types), outputs a high-res PGM stream and a broadcast multiview
//! with cairo-drawn overlays.
//!
//! Internally tees each input to two compositors:
//! - **Distribution compositor** ("mixer"): full-res PGM output with transitions
//! - **Multiview compositor** ("mv_comp"): PVW/PGM large panels over a
//!   thumbnail grid that grows with the number of inputs, with borders,
//!   labels, and clock drawn by a cairo overlay pad
//!
//! GPU pipeline (per input; pad indices come from `strom_types::vision_mixer::PadMap`):
//! ```text
//! queue_i → glupload_i → glcolorconvert_i → tee_i
//!   tee_i.src_0 → dist_comp.sink_i
//!   tee_i.src_1 → mv_comp.sink_i          (thumbnail)
//!   tee_i.src_2 → mv_comp.sink_{N+1+i}    (PVW big display candidate)
//! ```
//!
//! Distribution output: `dist_comp → capsfilter_dist → tee_pgm → [pgm_out]`
//! Multiview output: `mv_comp → [download] → capsfilter_mv → tee_mv → [multiview_out]`
//! (the overlay is an `appsrc` pad on `mv_comp`, not a cairooverlay element)

mod builder;
mod definition;
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
