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
/// (`tee ! queue` keeps the allocation proposal from the source) as intact,
/// so it cannot report a working host as broken.
#[test]
fn the_upload_probe_reads_a_working_upload_as_working() {
    if !common::gl_available(&["glupload", "gldownload"]) {
        return;
    }
    assert_eq!(
        strom::gpu::upload_keeps_pixels(true),
        Some(true),
        "a white frame through tee ! queue ! glupload ! gldownload did not come back white"
    );
}

/// The first four bytes of a white UYVP frame converted by `videoconvert`
/// for `glupload` and read back as RGBA, with the allocation query hidden
/// at `hide` ("convert": videoconvert's src, as at the vision mixer's
/// stinger; "upload": glupload's sink, as in the compositor block) or not
/// at all.
fn converted_white(hide: Option<&str>) -> Vec<u8> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    let pipeline = gst::parse::launch(
        "videotestsrc pattern=white num-buffers=1 ! \
         video/x-raw,format=UYVP,width=64,height=64 ! tee ! queue ! \
         videoconvert name=convert ! glupload name=upload ! glcolorconvert ! gldownload ! \
         video/x-raw,format=RGBA ! appsink name=sink sync=false",
    )
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    if let Some(name) = hide {
        let element = pipeline.by_name(name).unwrap();
        let pad = element
            .static_pad(if name == "convert" { "src" } else { "sink" })
            .unwrap();
        strom::gst::gl_input_front::hide_system_memory_allocation(&pad);
    }
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gstreamer_app::AppSink>()
        .unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(10));
    pipeline.set_state(gst::State::Null).unwrap();
    let sample = sample.expect("a frame");
    let map = sample.buffer().unwrap().map_readable().unwrap();
    map[..4].to_vec()
}

/// With the allocation query hidden where the vision mixer's stinger and
/// the compositor block hide it, a frame `videoconvert` converts for
/// `glupload` stays white. On a host whose udmabuf upload is broken the
/// unprotected chain gives `00 88 00 ff`; elsewhere (CI) the test shows only
/// that hiding the query does not hurt.
#[test]
fn a_converted_frame_stays_white_with_the_allocation_query_hidden() {
    if !common::gl_available(&["glupload", "gldownload", "glcolorconvert"]) {
        return;
    }
    for hide in ["convert", "upload"] {
        assert_eq!(
            converted_white(Some(hide)),
            [0xff; 4],
            "allocation query hidden at {hide}"
        );
    }
}
