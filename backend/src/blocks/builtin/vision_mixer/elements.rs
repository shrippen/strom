//! GStreamer element factory helpers for the vision mixer block.

use crate::blocks::BlockBuildError;
use crate::gpu;
use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, info, trace};

/// Compositor backend selection result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompositorBackend {
    OpenGL,
    /// NVIDIA `cudacompositor`. Frames stay in CUDA memory from NVDEC to
    /// NVENC; no shader FX (wipes fall back to fade, like the CPU backend).
    Cuda,
    Software,
}

/// Compositor factory per backend.
const GL_COMPOSITOR: &str = "glvideomixerelement";
const CUDA_COMPOSITOR: &str = "cudacompositor";
const CPU_COMPOSITOR: &str = "compositor";

impl CompositorBackend {
    fn compositor_factory(self) -> &'static str {
        match self {
            CompositorBackend::OpenGL => GL_COMPOSITOR,
            CompositorBackend::Cuda => CUDA_COMPOSITOR,
            CompositorBackend::Software => CPU_COMPOSITOR,
        }
    }
}

/// Determine which compositor backend to use.
pub fn select_backend(preference: &str) -> Result<CompositorBackend, BlockBuildError> {
    match preference {
        "gpu" => {
            if gst::ElementFactory::find("glvideomixerelement").is_some() {
                Ok(CompositorBackend::OpenGL)
            } else {
                Err(BlockBuildError::ElementCreation(
                    "GPU backend requested but glvideomixerelement not available".to_string(),
                ))
            }
        }
        "cuda" => {
            if gst::ElementFactory::find(CUDA_COMPOSITOR).is_some() {
                Ok(CompositorBackend::Cuda)
            } else {
                Err(BlockBuildError::ElementCreation(format!(
                    "CUDA backend requested but {} not available",
                    CUDA_COMPOSITOR
                )))
            }
        }
        "cpu" => Ok(CompositorBackend::Software),
        _ => {
            // Auto: prefer GPU, but only if a real hardware GL renderer is available.
            // Mesa software renderers (llvmpipe) are slower than the CPU compositor.
            if gst::ElementFactory::find("glvideomixerelement").is_some() && gpu::has_hardware_gl()
            {
                info!("Vision mixer: using GPU (OpenGL) backend");
                Ok(CompositorBackend::OpenGL)
            } else {
                info!("Vision mixer: no hardware GL, using CPU backend");
                Ok(CompositorBackend::Software)
            }
        }
    }
}

pub use crate::blocks::builtin::start_time::StartTime;

/// Create the distribution (PGM) compositor element.
pub fn make_dist_compositor(
    backend: CompositorBackend,
    latency_ms: u64,
    min_upstream_latency_ms: u64,
    start: StartTime,
) -> Result<gst::Element, BlockBuildError> {
    let element_type = backend.compositor_factory();

    let mixer = gst::ElementFactory::make(element_type)
        .name("mixer")
        .property("force-live", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", element_type, e)))?;

    apply_post_build_properties(&mixer, latency_ms, min_upstream_latency_ms, start);
    if mixer.find_property("background").is_some() {
        mixer.set_property_from_str("background", "black");
    }
    debug!(
        "Created distribution compositor: {} ({})",
        element_type,
        backend_name(backend)
    );
    Ok(mixer)
}

/// Create the multiview compositor element.
pub fn make_mv_compositor(
    backend: CompositorBackend,
    latency_ms: u64,
    min_upstream_latency_ms: u64,
    start: StartTime,
) -> Result<gst::Element, BlockBuildError> {
    let element_type = backend.compositor_factory();

    let mixer = gst::ElementFactory::make(element_type)
        .name("mv_comp")
        .property("force-live", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", element_type, e)))?;

    apply_post_build_properties(&mixer, latency_ms, min_upstream_latency_ms, start);
    if mixer.find_property("background").is_some() {
        mixer.set_property_from_str("background", "black");
    }

    debug!(
        "Created multiview compositor: {} ({})",
        element_type,
        backend_name(backend)
    );
    Ok(mixer)
}

/// Apply compositor properties that can be set after construction.
/// Note: force-live is construct-only and must be set via ElementFactory::make().property().
fn apply_post_build_properties(
    mixer: &gst::Element,
    latency_ms: u64,
    min_upstream_latency_ms: u64,
    start: StartTime,
) {
    if mixer.find_property("latency").is_some() {
        let latency_ns = latency_ms * 1_000_000;
        mixer.set_property("latency", latency_ns);
    }
    if mixer.find_property("min-upstream-latency").is_some() {
        let latency_ns = min_upstream_latency_ms * 1_000_000;
        mixer.set_property("min-upstream-latency", latency_ns);
    }
    // Pads that never got a buffer (a DSK layer or input nobody feeds yet)
    // would make the live aggregator wait for its deadline on every
    // frame, adding the whole latency budget to program (~50 ms).
    if mixer.find_property("ignore-inactive-pads").is_some() {
        mixer.set_property("ignore-inactive-pads", true);
    }
    if mixer.find_property("start-time-selection").is_some() {
        // Use "zero" instead of "first" to avoid a race condition in GStreamer 1.26:
        // with "first", if the aggregator srcpad task runs before any buffer arrives,
        // it falls through to using the absolute monotonic clock time as start time,
        // causing the compositor to wait for an impossibly far deadline (2× system uptime).
        // With "zero" and force-live=true, running time starts at 0 which is correct
        // for live pipelines using monotonic clock.
        // With base time 0, running time is clock time and "zero" would make
        // the compositor catch up from 0 forever; "first" is right there
        // (and the race above is harmless: clock time is the start time).
        mixer.set_property_from_str("start-time-selection", start.nick());
    }
}

/// Create a tee element for splitting input to multiple consumers.
pub fn make_tee(name: &str) -> Result<gst::Element, BlockBuildError> {
    gst::ElementFactory::make("tee")
        .name(name)
        .property("allow-not-linked", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("tee: {}", e)))
}

/// Create a queue element.
pub fn make_queue(name: &str) -> Result<gst::Element, BlockBuildError> {
    gst::ElementFactory::make("queue")
        .name(name)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("queue: {}", e)))
}

/// Create a `level` audio-metering element configured with the standard interval.
pub fn make_level(name: &str) -> Result<gst::Element, BlockBuildError> {
    gst::ElementFactory::make("level")
        .name(name)
        .property("interval", strom_types::vision_mixer::VU_METER_INTERVAL_NS)
        .property("post-messages", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("level: {}", e)))
}

/// Create a terminating `fakesink` for an audio metering branch.
///
/// `sync=false` and `async=false` so an unconnected audio input doesn't stall
/// preroll — the level element still posts messages when data flows.
pub fn make_meter_fakesink(name: &str) -> Result<gst::Element, BlockBuildError> {
    gst::ElementFactory::make("fakesink")
        .name(name)
        .property("sync", false)
        .property("async", false)
        .property("silent", true)
        .property("enable-last-sample", false)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("fakesink: {}", e)))
}

/// Create a `glshader` FX slot, pre-loaded with the identity fragment so it
/// negotiates and renders as a passthrough until an effect is programmed.
/// The `create-shader` handler enables runtime fragment swaps — without it
/// the fragment property is inert once the first shader is compiled.
/// GPU pipeline only — the CPU path has no FX slots.
pub fn make_glshader(name: &str) -> Result<gst::Element, BlockBuildError> {
    let elem = gst::ElementFactory::make("glshader")
        .name(name)
        .property("fragment", crate::gst::shaders::identity_fragment())
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("glshader: {}", e)))?;
    crate::gst::shaders::attach_create_shader_handler(&elem);
    Ok(elem)
}

/// Create a simple GStreamer element by factory name.
pub fn make_element(factory: &str, name: &str) -> Result<gst::Element, BlockBuildError> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", factory, e)))
}

/// Suppress upstream latency queries on the sink pad of a queue element.
///
/// The PGM feed from the distribution compositor (mixer) is tee'd into the
/// multiview compositor (mv_comp). Without this probe the latency query from
/// mv_comp traverses back through mixer, causing the two compositors' latencies
/// to stack. The probe answers the LATENCY query directly with min=0 so that
/// mv_comp's peer latency is determined by its direct input paths instead.
pub fn suppress_latency_query(queue: &gst::Element) {
    let pad = queue
        .static_pad("sink")
        .expect("queue must have a sink pad");
    pad.add_probe(gst::PadProbeType::QUERY_UPSTREAM, |_pad, info| {
        if let Some(query) = info.query_mut() {
            if let gst::QueryViewMut::Latency(latency) = query.view_mut() {
                trace!(
                    "Suppressed latency query on PGM->MV path (preventing compositor latency stacking)"
                );
                latency.set(true, gst::ClockTime::ZERO, None::<gst::ClockTime>);
                return gst::PadProbeReturn::Handled;
            }
        }
        gst::PadProbeReturn::Ok
    });
}

fn backend_name(backend: CompositorBackend) -> &'static str {
    match backend {
        CompositorBackend::OpenGL => "OpenGL",
        CompositorBackend::Cuda => "CUDA",
        CompositorBackend::Software => "Software",
    }
}
