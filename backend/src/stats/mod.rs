//! Statistics collection module for running pipelines.
//!
//! This module provides functions to collect runtime statistics from
//! GStreamer elements within running pipelines: RTP/AES67 related blocks,
//! Inter Input queues and video encoders.

pub mod collector;
pub mod encoder;
pub mod inter;
pub mod rtp;

pub use collector::StatsCollector;
pub use rtp::collect_rtp_jitterbuffer_stats;
