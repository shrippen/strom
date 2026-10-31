//! Pipeline construction for the vision mixer block.
//!
//! Sub-modules:
//!   - [`pipeline_gpu`] — `build_gpu_pipeline` (glvideomixer-based)
//!   - [`pipeline_cpu`] — `build_cpu_pipeline` (compositor-based, no GL; also
//!     the CUDA backend, which has the same topology in CUDA memory)
//!   - [`audio_meter`] — per-input + PGM `level` element chains and bus handler
//!   - [`pad_layout`]  — compositor sink-pad initial geometry/zorder/alpha

use super::elements::CompositorBackend;
use super::layout;
use super::overlay::{self, OverlayRenderer, VisionMixerOverlayState};
use super::properties;
use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use strom_types::vision_mixer;
use strom_types::{
    block::{ExternalPad, ExternalPads},
    MediaType, PropertyValue,
};
use tracing::info;

mod audio_meter;
pub(crate) mod pad_layout;
pub(crate) mod pipeline_cpu;
mod pipeline_gpu;
pub(crate) mod preview_out;
pub(crate) mod tiles_out;

/// Caps feature of CUDA device memory (NVIDIA `nvcodec` elements).
pub(crate) const CUDA_MEMORY_FEATURE: &str = "memory:CUDAMemory";

/// Vision Mixer block builder.
pub struct VisionMixerBuilder;

impl BlockBuilder for VisionMixerBuilder {
    fn get_external_pads(&self, props: &HashMap<String, PropertyValue>) -> Option<ExternalPads> {
        let num_inputs = properties::num_inputs_or_max(props);
        let num_dsk = properties::parse_num_dsk_inputs(props);

        // Internal element IDs are bare names — block expansion adds the instance_id prefix
        let mut inputs: Vec<ExternalPad> = (0..num_inputs)
            .map(|i| {
                ExternalPad::with_label(
                    format!("video_in_{}", i),
                    format!("V{}", i),
                    MediaType::Video,
                    format!("queue_{}", i),
                    "sink",
                )
            })
            .collect();

        if properties::parse_bool(props, strom_types::stinger::ENABLE_STINGER_PROPERTY, false) {
            inputs.push(ExternalPad::with_label(
                strom_types::stinger::STINGER_INPUT_PAD,
                "Stinger",
                MediaType::Video,
                "queue_stinger",
                "sink",
            ));
        }

        // DSK input pads
        for i in 0..num_dsk {
            inputs.push(ExternalPad::with_label(
                format!("dsk_in_{}", i),
                format!("DSK{}", i + 1),
                MediaType::Video,
                format!("queue_dsk_{}", i),
                "sink",
            ));
        }

        // Audio input pads (one per video input, plus a dedicated PGM audio pad).
        // Each audio branch feeds a level element so the multiview overlay can
        // render per-input VU meters.
        for i in 0..num_inputs {
            inputs.push(ExternalPad::with_label(
                format!("audio_in_{}", i),
                format!("A{}", i),
                MediaType::Audio,
                format!("audioconvert_audio_{}", i),
                "sink",
            ));
        }
        inputs.push(ExternalPad::with_label(
            "pgm_audio_in",
            "PGM Audio",
            MediaType::Audio,
            "audioconvert_audio_pgm",
            "sink",
        ));

        let mut outputs = vec![
            ExternalPad::with_label("pgm_out", "PGM", MediaType::Video, "queue_dist_out", "src"),
            ExternalPad::with_label(
                "multiview_out",
                "MV",
                MediaType::Video,
                "queue_mv_out",
                "src",
            ),
        ];
        if properties::parse_bool(props, vision_mixer::ENABLE_PREVIEW_OUT_PROPERTY, false) {
            outputs.push(ExternalPad::with_label(
                vision_mixer::PREVIEW_OUTPUT_PAD,
                "PVW",
                MediaType::Video,
                "queue_pvw_out",
                "src",
            ));
        }
        if properties::parse_bool(props, vision_mixer::ENABLE_TILES_OUT_PROPERTY, false) {
            outputs.push(ExternalPad::with_label(
                vision_mixer::TILES_OUTPUT_PAD,
                "Tiles",
                MediaType::Video,
                "queue_tiles_out",
                "src",
            ));
        }

        Some(ExternalPads { inputs, outputs })
    }

    fn build(
        &self,
        instance_id: &str,
        props: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        let num_inputs =
            properties::parse_num_inputs(props).map_err(BlockBuildError::InvalidProperty)?;
        let capacity = properties::parse_max_inputs(props, num_inputs)
            .map_err(BlockBuildError::InvalidProperty)?;
        // Checked here so a bad list fails the build; the inputs themselves
        // are added once the pipeline runs (see `vision_mixer::dynamic`).
        properties::parse_dynamic_inputs(props, num_inputs, capacity)
            .map_err(BlockBuildError::InvalidProperty)?;
        let num_pips = properties::parse_num_pips(props);
        let pgm_input = properties::parse_initial_pgm(props, num_inputs);
        let pvw_input = properties::parse_initial_pvw(props, num_inputs);
        let pgm_source = properties::parse_initial_pgm_source(props, num_inputs, num_pips);
        let pvw_source = properties::parse_initial_pvw_source(props, num_inputs, num_pips);
        let labels = properties::parse_input_labels(props, num_inputs);
        let latency_ms = properties::parse_u64(props, "latency", vision_mixer::DEFAULT_LATENCY_MS);
        let min_upstream_ms = properties::parse_u64(
            props,
            "min_upstream_latency",
            vision_mixer::DEFAULT_MIN_UPSTREAM_LATENCY_MS,
        );
        let (pgm_w, pgm_h) = properties::parse_resolution(
            props,
            "pgm_resolution",
            vision_mixer::DEFAULT_PGM_RESOLUTION,
        );
        let (mv_w, mv_h) = properties::parse_resolution(
            props,
            "multiview_resolution",
            vision_mixer::DEFAULT_MULTIVIEW_RESOLUTION,
        );
        let pgm_framerate = properties::parse_framerate(
            props,
            "pgm_framerate",
            vision_mixer::DEFAULT_PGM_FRAMERATE,
        );
        let mv_framerate = properties::parse_framerate(
            props,
            "multiview_framerate",
            vision_mixer::DEFAULT_MULTIVIEW_FRAMERATE,
        );

        let num_dsk_inputs = properties::parse_num_dsk_inputs(props);

        let pip_bg_inputs: Vec<Option<usize>> = (0..num_pips)
            .map(|i| properties::parse_pip_bg(props, i, num_inputs))
            .collect();
        // Zones are runtime-only — the operator configures them via the
        // /pip endpoint after the pipeline is built.
        let pip_zones: Vec<Vec<strom_types::vision_mixer::Zone>> =
            (0..num_pips).map(|_| Vec::new()).collect();

        let output_format = properties::parse_output_format(props);
        let gl_download =
            properties::parse_bool(props, "gl_download", vision_mixer::DEFAULT_GL_DOWNLOAD);
        let show_vu_meters = properties::parse_show_vu_meters(props);
        let enable_fx = properties::parse_bool(props, "enable_fx", vision_mixer::DEFAULT_ENABLE_FX);
        let swap_pvw_pgm =
            properties::parse_bool(props, "swap_pvw_pgm", vision_mixer::DEFAULT_SWAP_PVW_PGM);
        let enable_stinger =
            properties::parse_bool(props, strom_types::stinger::ENABLE_STINGER_PROPERTY, false);
        let enable_preview_out =
            properties::parse_bool(props, vision_mixer::ENABLE_PREVIEW_OUT_PROPERTY, false);

        let tiles = properties::parse_bool(props, vision_mixer::ENABLE_TILES_OUT_PROPERTY, false)
            .then(|| {
                tiles_out::TilesGrid::new(
                    properties::parse_tile_cells(props, capacity),
                    properties::parse_resolution(
                        props,
                        vision_mixer::TILES_CELL_PROPERTY,
                        vision_mixer::DEFAULT_TILES_CELL,
                    ),
                    properties::parse_u64(props, vision_mixer::TILES_COLUMNS_PROPERTY, 0) as usize,
                    properties::parse_framerate(
                        props,
                        vision_mixer::TILES_FRAMERATE_PROPERTY,
                        &format!("{}/{}", mv_framerate.0, mv_framerate.1),
                    ),
                    properties::parse_format(props, vision_mixer::TILES_FORMAT_PROPERTY),
                )
            });

        let pref = props
            .get("compositor_preference")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("auto");
        let mut backend = super::elements::select_backend(pref)?;
        // Free slots need runtime inputs, which the OpenGL mixer has not
        // got yet: "auto" picks CUDA or the CPU instead, unless that would
        // lose the shader FX (OpenGL only), which fails below like "gpu".
        if backend == CompositorBackend::OpenGL
            && capacity > num_inputs
            && pref != "gpu"
            && !enable_fx
        {
            backend = if gst::ElementFactory::find(super::elements::CUDA_COMPOSITOR).is_some() {
                CompositorBackend::Cuda
            } else {
                CompositorBackend::Software
            };
            info!(
                "Vision mixer: max_inputs {} needs dynamic inputs, using the {:?} backend \
                 instead of OpenGL",
                capacity, backend
            );
        }
        if backend == CompositorBackend::OpenGL && capacity > num_inputs {
            return Err(BlockBuildError::InvalidProperty(format!(
                "max_inputs {} needs the CPU or CUDA backend: the OpenGL mixer has no \
                 dynamic inputs yet, and the CPU and CUDA ones have no shader FX \
                 (set max_inputs to 0 or num_inputs {}, or turn off shader FX, or \
                 choose the cpu or cuda backend)",
                capacity, num_inputs
            )));
        }
        let start_time = properties::parse_start_time(props);

        info!(
            "Building vision mixer: {} inputs ({} slots), PGM={}x{}@{}/{}, MV={}x{}@{}/{}, backend={:?}, pgm={}, pvw={}",
            num_inputs, capacity, pgm_w, pgm_h, pgm_framerate.0, pgm_framerate.1,
            mv_w, mv_h, mv_framerate.0, mv_framerate.1,
            backend, pgm_input, pvw_input
        );

        let p = PipelineParams {
            instance_id,
            flow_id: properties::parse_flow_id(props),
            num_inputs,
            capacity,
            num_dsk_inputs,
            num_pips,
            pgm_input,
            pvw_input,
            pgm_source,
            pvw_source,
            pip_bg_inputs: &pip_bg_inputs,
            pip_zones: &pip_zones,
            labels: &labels,
            latency_ms,
            min_upstream_ms,
            start_time,
            pgm_w,
            pgm_h,
            mv_w,
            mv_h,
            pgm_framerate,
            mv_framerate,
            backend,
            output_format,
            gl_download,
            show_vu_meters,
            enable_fx,
            swap_pvw_pgm,
            enable_stinger,
            enable_preview_out,
            tiles,
        };

        match backend {
            CompositorBackend::OpenGL => pipeline_gpu::build_gpu_pipeline(&p, ctx),
            CompositorBackend::Software | CompositorBackend::Cuda => {
                pipeline_cpu::build_cpu_pipeline(&p, ctx)
            }
        }
    }
}

/// Shared parameters for pipeline construction.
pub(super) struct PipelineParams<'a> {
    pub(super) instance_id: &'a str,
    /// Owning flow, used to key the overlay registries so flow teardown can
    /// clear them — see `overlay::unregister_flow`.
    pub(super) flow_id: strom_types::FlowId,
    /// Static inputs, fed by flow links: `0..num_inputs`.
    pub(super) num_inputs: usize,
    /// Input slots (`max_inputs`): the pad indices reserved for inputs. The
    /// slots above `num_inputs` take dynamic inputs at runtime.
    pub(super) capacity: usize,
    pub(super) num_dsk_inputs: usize,
    pub(super) num_pips: usize,
    pub(super) pgm_input: usize,
    pub(super) pvw_input: usize,
    pub(super) pgm_source: strom_types::vision_mixer::Source,
    pub(super) pvw_source: strom_types::vision_mixer::Source,
    pub(super) pip_bg_inputs: &'a [Option<usize>],
    pub(super) pip_zones: &'a [Vec<strom_types::vision_mixer::Zone>],
    pub(super) labels: &'a [String],
    pub(super) latency_ms: u64,
    pub(super) min_upstream_ms: u64,
    pub(super) start_time: super::elements::StartTime,
    pub(super) pgm_w: u32,
    pub(super) pgm_h: u32,
    pub(super) mv_w: u32,
    pub(super) mv_h: u32,
    pub(super) pgm_framerate: (i32, i32),
    pub(super) mv_framerate: (i32, i32),
    pub(super) backend: CompositorBackend,
    pub(super) output_format: Option<String>,
    pub(super) gl_download: bool,
    pub(super) show_vu_meters: bool,
    /// Build shader FX slots (GPU pipeline only; the CPU builder ignores it).
    pub(super) enable_fx: bool,
    /// Mirror the multiview layout (PGM left, PVW right).
    pub(super) swap_pvw_pgm: bool,
    /// Build the stinger input: a graphic pad, and on the GPU a matte pad.
    pub(super) enable_stinger: bool,
    /// Build the preview output (`preview_out`).
    pub(super) enable_preview_out: bool,
    /// Build the tile output (`tiles_out`) on this grid.
    pub(super) tiles: Option<tiles_out::TilesGrid>,
}

impl PipelineParams<'_> {
    /// Sink pad indices of the compositors this mixer builds.
    pub(super) fn pad_map(&self) -> vision_mixer::PadMap {
        vision_mixer::PadMap::new(self.capacity, self.num_dsk_inputs, self.num_pips)
    }

    /// Aspect the multiview layout fits its tiles to (the program's).
    pub(super) fn source_aspect(&self) -> f64 {
        if self.pgm_h > 0 {
            self.pgm_w as f64 / self.pgm_h as f64
        } else {
            16.0 / 9.0
        }
    }

    /// The multiview layout of the static inputs.
    pub(super) fn mv_layout(&self) -> layout::OverlayLayout {
        let inputs: Vec<usize> = (0..self.num_inputs).collect();
        layout::compute_layout_for(
            self.mv_w,
            self.mv_h,
            &inputs,
            self.capacity,
            self.num_pips,
            self.source_aspect(),
            self.swap_pvw_pgm,
        )
    }

    /// What building one input chain needs, owned, so that a dynamic input
    /// can be built while the mixer runs.
    pub(super) fn input_spec(&self) -> InputSpec {
        InputSpec {
            instance_id: self.instance_id.to_string(),
            backend: self.backend,
            output_format: self.output_format.clone(),
            num_pips: self.num_pips,
            pm: self.pad_map(),
            underlay: (self.num_pips > 0).then(|| self.underlay_source()),
            preview_out: self.enable_preview_out,
            tiles: self.tiles.clone(),
            pgm_w: self.pgm_w,
            pgm_h: self.pgm_h,
            mv_w: self.mv_w,
            mv_h: self.mv_h,
            source_aspect: self.source_aspect(),
            swap_pvw_pgm: self.swap_pvw_pgm,
        }
    }
}

/// The parameters of a mixer that one input chain depends on (see
/// [`PipelineParams::input_spec`]).
#[derive(Debug, Clone)]
pub(crate) struct InputSpec {
    pub(crate) instance_id: String,
    pub(crate) backend: CompositorBackend,
    pub(crate) output_format: Option<String>,
    pub(crate) num_pips: usize,
    pub(crate) pm: vision_mixer::PadMap,
    /// Border underlay sources (live, caps), when the mixer has PiPs.
    pub(crate) underlay: Option<(bool, String)>,
    /// The mixer has the preview output (one more tee branch per input).
    pub(crate) preview_out: bool,
    /// The mixer has the tile output on this grid (one more tee branch).
    pub(crate) tiles: Option<tiles_out::TilesGrid>,
    pub(crate) pgm_w: u32,
    pub(crate) pgm_h: u32,
    pub(crate) mv_w: u32,
    pub(crate) mv_h: u32,
    pub(crate) source_aspect: f64,
    pub(crate) swap_pvw_pgm: bool,
}

impl InputSpec {
    /// Create a namespaced element ID.
    pub(crate) fn id(&self, name: &str) -> String {
        format!("{}:{}", self.instance_id, name)
    }

    /// Tee source pad of the preview output branch (after the dist, thumb,
    /// PVW and PiP branches).
    pub(crate) fn pvw_out_tee_src(&self) -> usize {
        3 + self.num_pips
    }

    /// Tee source pad of the tile output branch (after the preview's).
    pub(crate) fn tiles_out_tee_src(&self) -> usize {
        self.pvw_out_tee_src() + usize::from(self.preview_out)
    }

    /// Multiview layout with tiles for `inputs` (ascending).
    pub(crate) fn mv_layout(&self, inputs: &[usize]) -> layout::OverlayLayout {
        layout::compute_layout_for(
            self.mv_w,
            self.mv_h,
            inputs,
            self.pm.capacity,
            self.num_pips,
            self.source_aspect,
            self.swap_pvw_pgm,
        )
    }

    fn finish_caps(&self, builder: gst::caps::Builder<gst::caps::NoFeature>) -> gst::Caps {
        match self.backend {
            CompositorBackend::Cuda => builder.features([CUDA_MEMORY_FEATURE]).build(),
            CompositorBackend::OpenGL | CompositorBackend::Software => builder.build(),
        }
    }

    /// Caps pinning only the pixel format.
    pub(crate) fn format_caps(&self, format: &str) -> gst::Caps {
        self.finish_caps(gst::Caps::builder("video/x-raw").field("format", format))
    }

    /// Raw video caps in the compositor's native memory, any format.
    pub(crate) fn memory_caps(&self) -> gst::Caps {
        self.finish_caps(gst::Caps::builder("video/x-raw"))
    }
}

impl<'a> PipelineParams<'a> {
    /// Create a namespaced element ID.
    pub(super) fn id(&self, name: &str) -> String {
        format!("{}:{}", self.instance_id, name)
    }

    /// Border underlay sources: live or not, and their caps.
    ///
    /// Non-live (the default) adds no latency, but a non-live
    /// `videotestsrc` stamps its buffers from 0 and pushes them while the
    /// pipeline prerolls. With `start_time_selection=first` (base time 0,
    /// inputs carry clock time) the compositors take the earliest first
    /// buffer as output start, so the underlays would start the output at
    /// running time 0 — far behind the live inputs, whose frames then wait
    /// as "future" buffers (content pads show nothing) until the
    /// compositor has caught up with clock time. Live underlays stamp
    /// running time instead; at the program frame rate they add at most
    /// one output frame of latency.
    pub(super) fn underlay_source(&self) -> (bool, String) {
        let live = self.start_time == super::elements::StartTime::First;
        let (n, d) = if live { self.pgm_framerate } else { (5, 1) };
        (
            live,
            format!(
                "video/x-raw,format=RGBA,width=16,height=16,framerate={n}/{d},pixel-aspect-ratio=1/1"
            ),
        )
    }

    /// Caps feature of the compositor's native memory, if not system memory.
    /// Only the CUDA backend needs one here: the GL builder spells out its
    /// GL-memory caps itself.
    fn memory_feature(&self) -> Option<&'static str> {
        match self.backend {
            CompositorBackend::Cuda => Some(CUDA_MEMORY_FEATURE),
            CompositorBackend::OpenGL | CompositorBackend::Software => None,
        }
    }

    /// Finish a caps builder, tagging it with the backend's memory feature.
    fn finish_caps(&self, builder: gst::caps::Builder<gst::caps::NoFeature>) -> gst::Caps {
        match self.memory_feature() {
            Some(feature) => builder.features([feature]).build(),
            None => builder.build(),
        }
    }

    /// Raw video caps with resolution, framerate and an optional pixel format.
    fn raw_caps(&self, w: u32, h: u32, framerate: (i32, i32), format: Option<&str>) -> gst::Caps {
        let mut builder = gst::Caps::builder("video/x-raw")
            .field("width", w as i32)
            .field("height", h as i32)
            .field("framerate", gst::Fraction::new(framerate.0, framerate.1))
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1));
        if let Some(fmt) = format {
            builder = builder.field("format", fmt);
        }
        self.finish_caps(builder)
    }

    /// Build PGM output caps with resolution, framerate, and optional pixel format.
    pub(super) fn pgm_caps(&self) -> gst::Caps {
        self.raw_caps(
            self.pgm_w,
            self.pgm_h,
            self.pgm_framerate,
            self.output_format.as_deref(),
        )
    }

    /// Build multiview output caps with resolution, framerate, and optional pixel format.
    pub(super) fn mv_caps(&self) -> gst::Caps {
        self.raw_caps(
            self.mv_w,
            self.mv_h,
            self.mv_framerate,
            self.output_format.as_deref(),
        )
    }

    /// PGM caps with `format` instead of `output_format` — the software
    /// compositor's blend space, see [`Self::alpha_blend_format`].
    pub(super) fn pgm_blend_caps(&self, format: &str) -> gst::Caps {
        self.raw_caps(self.pgm_w, self.pgm_h, self.pgm_framerate, Some(format))
    }

    /// Multiview caps with `format` instead of `output_format` — the software
    /// compositor's blend space, see [`Self::alpha_blend_format`].
    pub(super) fn mv_blend_caps(&self, format: &str) -> gst::Caps {
        self.raw_caps(self.mv_w, self.mv_h, self.mv_framerate, Some(format))
    }

    /// The format the software compositor has to blend in so that keyed pads
    /// keep their per-pixel alpha, or `None` when `output_format` needs no
    /// substitute.
    ///
    /// `compositor` blends in its own output format and converts every sink
    /// pad to it first, so an alpha-less `output_format` (I420, NV12, RGB, ...)
    /// strips the alpha of DSK graphics, the multiview overlay and the border
    /// underlays. Blending in the alpha-carrying counterpart and converting
    /// once on the way out keeps the key and still hands `output_format`
    /// downstream. Alpha-carrying and unknown formats are left alone.
    pub(super) fn alpha_blend_format(&self) -> Option<&'static str> {
        // `cudacompositor` gets CUDA memory, which no CPU convert takes.
        if self.backend != CompositorBackend::Software {
            return None;
        }
        let fmt = self.output_format.as_deref()?;
        let format = fmt.parse::<gst_video::VideoFormat>().ok()?;
        let info = gst_video::VideoFormatInfo::from_format(format);
        if info.has_alpha() {
            return None;
        }
        // Match the chroma subsampling of the requested format so the extra
        // hop costs nothing the output would not have cost anyway.
        Some(match fmt {
            "YUY2" | "UYVY" => "A422",
            "v210" => "A422_10LE",
            _ if info.is_rgb() || info.is_gray() => "RGBA",
            _ => "A420",
        })
    }

    /// Build PGM output caps for the GL-memory passthrough path.
    /// Constrains framerate/resolution without forcing a download to system memory.
    pub(super) fn pgm_caps_glmem(&self) -> gst::Caps {
        let s = format!(
            "video/x-raw(memory:GLMemory),width={},height={},framerate={}/{},pixel-aspect-ratio=1/1",
            self.pgm_w, self.pgm_h, self.pgm_framerate.0, self.pgm_framerate.1
        );
        s.parse().expect("valid GL memory caps for PGM")
    }

    /// Build multiview output caps for the GL-memory passthrough path.
    pub(super) fn mv_caps_glmem(&self) -> gst::Caps {
        let s = format!(
            "video/x-raw(memory:GLMemory),width={},height={},framerate={}/{},pixel-aspect-ratio=1/1",
            self.mv_w, self.mv_h, self.mv_framerate.0, self.mv_framerate.1
        );
        s.parse().expect("valid GL memory caps for MV")
    }
}

/// Set up the overlay renderer: creates shared state, registers it, and starts
/// a 1Hz timer that pushes overlay frames via appsrc when state changes.
///
/// Returns the shared overlay state so callers can install additional
/// consumers (e.g. a bus handler that writes audio meter values into it).
pub(super) fn setup_overlay_renderer(
    p: &PipelineParams,
    appsrc: &gst_app::AppSrc,
    overlay_caps: &gst::Caps,
    mv_layout: &layout::OverlayLayout,
    ctx: &BlockBuildContext,
) -> Arc<VisionMixerOverlayState> {
    let pip_initial = overlay::PipInitialState {
        num_pips: p.num_pips,
        pip_bgs: (0..p.num_pips)
            .map(|i| p.pip_bg_inputs.get(i).copied().flatten())
            .collect(),
        pip_zones: (0..p.num_pips)
            .map(|i| p.pip_zones.get(i).cloned().unwrap_or_default())
            .collect(),
        pgm_pip: p.pgm_source.as_pip(),
        pvw_pip: p.pvw_source.as_pip(),
    };

    // pgm_input/pvw_input must match what the compositor actually renders.
    // For a single Input source it's that input; for a PiP source we initialize
    // from the PiP's bg input so transitions and overlay rendering stay consistent
    // (Take from PiP-PGM falls back to bg-as-single-input).
    let initial_pgm_input = p
        .pgm_source
        .as_pip()
        .and_then(|p_idx| pip_initial.pip_bgs.get(p_idx).copied().flatten())
        .unwrap_or(p.pgm_input);
    let initial_pvw_input = p
        .pvw_source
        .as_pip()
        .and_then(|p_idx| pip_initial.pip_bgs.get(p_idx).copied().flatten())
        .unwrap_or(p.pvw_input);

    let overlay_state = Arc::new(VisionMixerOverlayState::with_slots(
        p.num_inputs,
        p.capacity,
        p.num_dsk_inputs,
        initial_pgm_input,
        initial_pvw_input,
        p.labels.to_vec(),
        mv_layout.clone(),
        p.pgm_w,
        p.pgm_h,
        p.show_vu_meters,
        pip_initial,
    ));

    // Dynamic inputs are built like static ones, from the same spec; the
    // GL mixer has none yet (rejected above when `max_inputs` asks).
    if p.backend != CompositorBackend::OpenGL {
        if let Ok(mut dynamic) = overlay_state.dynamic.lock() {
            dynamic.spec = Some(p.input_spec());
        }
    }

    // Register the overlay state so the API layer can access it
    overlay::register_overlay_state(p.flow_id, p.instance_id, Arc::clone(&overlay_state));

    let renderer = Arc::new(Mutex::new(OverlayRenderer::new(
        appsrc.clone(),
        overlay_caps.clone(),
        Arc::clone(&overlay_state),
        p.mv_w as i32,
        p.mv_h as i32,
    )));

    let block_id = p.instance_id.to_string();
    overlay::register_overlay_renderer(p.flow_id, &block_id, Arc::clone(&renderer));

    let block_id_for_timer = block_id.clone();
    let flow_id = p.flow_id;
    let renderer_for_timer = Arc::clone(&renderer);
    let mv_framerate = p.mv_framerate;
    ctx.register_element_setup(Box::new(move |_flow_id, _events| {
        if let Ok(mut r) = renderer_for_timer.lock() {
            r.track_mixer_pad();
        }
        overlay::start_overlay_timer(
            flow_id,
            block_id_for_timer.clone(),
            renderer_for_timer.clone(),
            mv_framerate,
        );
    }));

    overlay_state
}
