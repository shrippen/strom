//! The tile output (`tiles_out`): every input scaled into its own cell of
//! one grid, in input order.
//!
//! ```text
//!   tee_i.src_{t} → queue_to_tiles_out_i → tiles_comp.sink_i
//!   tiles_comp → [convert] → [download] → capsfilter_tiles → queue_tiles_out
//! ```
//!
//! A control surface that shows every input live reads this one stream
//! and cuts it up, instead of one stream per input: the inputs are scaled
//! on the mixer's GPU (or CPU), where they already are, and leave as one
//! frame. Input `first + c` sits in cell `c` (row-major, see `tiles_grid`),
//! aspect fitted; a cell without an input stays black, an input outside
//! the cells has no branch. The cells never move, so nothing has to follow
//! operations. The tile compositor's pad of cell `c` is `sink_c`.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::element::ElementPadRef;
use strom_types::vision_mixer::{tiles_grid, PadMap};
use strom_types::PropertyValue;
use tracing::info;

use super::super::elements::{self, CompositorBackend};
use super::{InputSpec, PipelineParams};
use crate::blocks::BlockBuildError;

/// Size and grid of the tile output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TilesGrid {
    /// The input in cell 0.
    pub(crate) first: usize,
    /// Number of cells.
    pub(crate) cells: usize,
    pub(crate) cell_w: u32,
    pub(crate) cell_h: u32,
    pub(crate) columns: usize,
    pub(crate) rows: usize,
    pub(crate) framerate: (i32, i32),
    /// Pixel format; `None` lets the compositor pick.
    pub(crate) format: Option<String>,
}

impl TilesGrid {
    /// The grid for `cells` inputs from input `first` on.
    pub(crate) fn new(
        (first, cells): (usize, usize),
        (cell_w, cell_h): (u32, u32),
        columns: usize,
        framerate: (i32, i32),
        format: Option<String>,
    ) -> Self {
        let cells = cells.max(1);
        let (columns, rows) = tiles_grid(cells, columns);
        Self {
            first,
            cells,
            cell_w,
            cell_h,
            columns,
            rows,
            framerate,
            format,
        }
    }

    /// Width and height of the whole output.
    pub(crate) fn size(&self) -> (u32, u32) {
        (
            self.cell_w * self.columns as u32,
            self.cell_h * self.rows as u32,
        )
    }

    /// The cell of input `i`, if it has one.
    pub(crate) fn cell_of(&self, i: usize) -> Option<usize> {
        i.checked_sub(self.first).filter(|c| *c < self.cells)
    }

    /// The inputs that have a cell, below `inputs`.
    pub(crate) fn inputs(&self, inputs: usize) -> std::ops::Range<usize> {
        self.first.min(inputs)..(self.first + self.cells).min(inputs)
    }

    /// Rectangle of cell `c`.
    pub(crate) fn cell(&self, c: usize) -> (i32, i32, i32, i32) {
        let (col, row) = (c % self.columns, c / self.columns);
        (
            (col as u32 * self.cell_w) as i32,
            (row as u32 * self.cell_h) as i32,
            self.cell_w as i32,
            self.cell_h as i32,
        )
    }

    /// Pad properties of cell `c`'s pad: the cell, the picture fitted.
    pub(crate) fn pad_properties(&self, c: usize) -> HashMap<String, PropertyValue> {
        let (x, y, w, h) = self.cell(c);
        HashMap::from([
            ("xpos".to_string(), PropertyValue::Int(x as i64)),
            ("ypos".to_string(), PropertyValue::Int(y as i64)),
            ("width".to_string(), PropertyValue::Int(w as i64)),
            ("height".to_string(), PropertyValue::Int(h as i64)),
            ("alpha".to_string(), PropertyValue::Float(1.0)),
            (
                "sizing-policy".to_string(),
                PropertyValue::String("keep-aspect-ratio".to_string()),
            ),
        ])
    }
}

/// Build the tile compositor, its output chain and one branch per input
/// from `tee_{i}.src_{tee_src}`. Returns the compositor.
pub(super) fn append(
    p: &PipelineParams,
    grid: &TilesGrid,
    tee_src: usize,
    elems: &mut Vec<(String, gst::Element)>,
    links: &mut Vec<(ElementPadRef, ElementPadRef)>,
) -> Result<gst::Element, BlockBuildError> {
    let comp =
        elements::make_mv_compositor(p.backend, p.latency_ms, p.min_upstream_ms, p.start_time)?;
    let comp_id = p.id("tiles_comp");
    comp.set_property("name", &comp_id);
    elems.push((comp_id.clone(), comp.clone()));

    // In cell order: the GL mixer names its pads in link order.
    let spec = p.input_spec();
    for i in grid.inputs(p.num_inputs) {
        append_input_branch(&spec, grid, i, tee_src, elems, links)?;
    }

    let (w, h) = grid.size();
    let caps = |features: Option<&str>| {
        let builder = gst::Caps::builder("video/x-raw")
            .field("width", w as i32)
            .field("height", h as i32)
            .field(
                "framerate",
                gst::Fraction::new(grid.framerate.0, grid.framerate.1),
            )
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
            .field_if_some("format", grid.format.as_deref());
        match features {
            Some(f) => builder.features([f]).build(),
            None => builder.build(),
        }
    };
    // Downloaded, the caps come after the download, as the preview's:
    // before it, `gldownload` hands GL memory on to anything that takes
    // it, and the frames leave in a context the next flow does not share.
    let mut chain = vec![comp_id];
    let (memory, download) = match p.backend {
        CompositorBackend::OpenGL => {
            let q_post_id = p.id("queue_post_tiles");
            elems.push((q_post_id.clone(), elements::make_queue(&q_post_id)?));
            chain.push(q_post_id);
            // glvideomixer blends RGBA only: another format is converted
            // on the GPU, before the download.
            if grid.format.is_some() {
                let cc_id = p.id("glcolorconvert_tiles");
                elems.push((
                    cc_id.clone(),
                    elements::make_element("glcolorconvert", &cc_id)?,
                ));
                chain.push(cc_id);
            }
            (Some("memory:GLMemory"), "gldownload")
        }
        CompositorBackend::Cuda => (Some(super::CUDA_MEMORY_FEATURE), "cudadownload"),
        CompositorBackend::Software => (None, ""),
    };
    let feature = if memory.is_some() && p.gl_download {
        let dl_id = p.id("download_tiles");
        elems.push((dl_id.clone(), elements::make_element(download, &dl_id)?));
        chain.push(dl_id);
        None
    } else {
        memory
    };
    let cf_id = p.id("capsfilter_tiles");
    elems.push((cf_id.clone(), capsfilter(&cf_id, &caps(feature))?));
    chain.push(cf_id);
    let q_out_id = p.id("queue_tiles_out");
    elems.push((q_out_id.clone(), elements::make_queue(&q_out_id)?));
    chain.push(q_out_id);
    for pair in chain.windows(2) {
        links.push((
            ElementPadRef::pad(&pair[0], "src"),
            ElementPadRef::pad(&pair[1], "sink"),
        ));
    }
    info!(
        "Vision mixer {}: tile output {}x{} ({}x{} cells of {}x{}), format {}",
        p.instance_id,
        w,
        h,
        grid.columns,
        grid.rows,
        grid.cell_w,
        grid.cell_h,
        grid.format.as_deref().unwrap_or("auto")
    );
    Ok(comp)
}

/// Input `i`'s branch to the tile compositor:
/// `tee_i.src_{tee_src} → queue_to_tiles_out_i → tiles_comp.sink_c` for
/// its cell `c`; nothing for an input without a cell. Shared by static and
/// dynamic inputs.
pub(crate) fn append_input_branch(
    spec: &InputSpec,
    grid: &TilesGrid,
    i: usize,
    tee_src: usize,
    elems: &mut Vec<(String, gst::Element)>,
    links: &mut Vec<(ElementPadRef, ElementPadRef)>,
) -> Result<(), BlockBuildError> {
    let Some(cell) = grid.cell_of(i) else {
        return Ok(());
    };
    let q_id = spec.id(&format!("queue_to_tiles_out_{}", i));
    elems.push((q_id.clone(), elements::make_queue(&q_id)?));
    links.push((
        ElementPadRef::pad(spec.id(&format!("tee_{}", i)), format!("src_{}", tee_src)),
        ElementPadRef::pad(&q_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&q_id, "src"),
        ElementPadRef::pad(spec.id("tiles_comp"), PadMap::sink(cell)),
    ));
    Ok(())
}

fn capsfilter(id: &str, caps: &gst::Caps) -> Result<gst::Element, BlockBuildError> {
    gst::ElementFactory::make("capsfilter")
        .name(id)
        .property("caps", caps)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", id, e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_fill_the_grid_row_by_row() {
        let grid = TilesGrid::new((0, 17), (640, 360), 0, (30, 1), None);
        assert_eq!((grid.columns, grid.rows), (5, 4));
        assert_eq!(grid.size(), (3200, 1440));
        assert_eq!(grid.cell(0), (0, 0, 640, 360));
        assert_eq!(grid.cell(4), (2560, 0, 640, 360));
        assert_eq!(grid.cell(5), (0, 360, 640, 360));
        assert_eq!(grid.cell(16), (640, 1080, 640, 360));
    }

    #[test]
    fn inputs_outside_the_cells_get_none() {
        // Input 0 (a background) and the inputs after the fourth cell
        // stay out; the grid is the cells', not the capacity's.
        let grid = TilesGrid::new((1, 4), (640, 360), 0, (30, 1), None);
        assert_eq!((grid.columns, grid.rows), (2, 2));
        assert_eq!(grid.cell_of(0), None);
        assert_eq!(grid.cell_of(1), Some(0));
        assert_eq!(grid.cell_of(4), Some(3));
        assert_eq!(grid.cell_of(5), None);
        assert_eq!(grid.inputs(3), 1..3);
        assert_eq!(grid.inputs(100), 1..5);
        assert_eq!(grid.inputs(0), 0..0);
    }
}
