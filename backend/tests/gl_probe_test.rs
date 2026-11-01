//! The startup GL probe must find the renderer wherever GL works.
//!
//! On GStreamer 1.28, `glupload` towards an unconstrained appsink negotiates
//! DMABuf instead of GL memory; the probe buffer then carries no GL context and
//! the renderer reads as unknown, so the "auto" compositor backends fall back
//! to the CPU.

pub mod common;

#[test]
fn the_gl_probe_finds_the_renderer() {
    if !common::gl_available(&["glupload"]) {
        return;
    }
    strom::gpu::detect_gpu_capabilities();
    assert!(
        strom::gpu::gl_renderer_info().is_some(),
        "GL works here, but the startup probe found no renderer"
    );
}

/// The upload probe reads a frame that went through `glupload` the safe way
/// (a queue keeps the allocation proposal from the source) as intact, so it
/// cannot report a working host as broken. Whether the direct path is broken
/// depends on the driver; the startup decision must match what the probe sees.
#[test]
fn the_upload_probe_reads_a_working_upload_as_working() {
    if !common::gl_available(&["glupload", "gldownload"]) {
        return;
    }
    assert_eq!(
        strom::gpu::upload_keeps_pixels(true),
        Some(true),
        "a white frame through tee ! queue ! glupload ! gldownload came back empty"
    );
    strom::gpu::detect_gpu_capabilities();
    assert_eq!(
        strom::gpu::udmabuf_upload_broken(),
        strom::gpu::upload_keeps_pixels(false) == Some(false)
    );
}
