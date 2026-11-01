//! Byte counters of a flow's output elements.
//!
//! What an output actually sends or writes, so a client can show a measured
//! bitrate rather than the one it configured. Nothing here runs per buffer:
//! `rtmp2sink` keeps its own socket counters in its `stats` structure, and the
//! Live Recorder's file sink adds each write to an atomic. Both are read only
//! when the API asks.

use super::PipelineManager;
use crate::blocks::builtin::live_recorder::fragment_sink::FragmentFileSink;
use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::api::{OutputByteCount, OutputBytes, OutputMedium};

impl PipelineManager {
    /// Byte counters of every output element in the pipeline that has one.
    pub fn get_output_bytes(&self) -> OutputBytes {
        OutputBytes {
            outputs: self
                .elements
                .iter()
                .filter_map(|(name, element)| Some((name.clone(), output_byte_count(element)?)))
                .collect(),
        }
    }
}

/// The byte counter of `element`, or `None` when it is not an output that
/// keeps one.
///
/// `rtmp2sink`: `out-bytes-total` of its connection, the bytes written to the
/// socket with the RTMP chunk headers, so the real network rate. Zero while it
/// has no connection, and it counts from zero again after a reconnect.
///
/// The Live Recorder's file sink: the bytes it wrote to its files.
pub fn output_byte_count(element: &gst::Element) -> Option<OutputByteCount> {
    if let Some(sink) = element.downcast_ref::<FragmentFileSink>() {
        return Some(OutputByteCount {
            medium: OutputMedium::File,
            bytes: sink.bytes_written(),
        });
    }
    let factory = element.factory()?;
    if factory.name() != "rtmp2sink" {
        return None;
    }
    let bytes = element
        .property_value("stats")
        .get::<Option<gst::Structure>>()
        .ok()
        .flatten()
        .and_then(|s| s.get::<u64>("out-bytes-total").ok())?;
    Some(OutputByteCount {
        medium: OutputMedium::Network,
        bytes,
    })
}
