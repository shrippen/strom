//! System monitoring types for CPU and GPU statistics.

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// OpenGL renderer information detected from GStreamer's GL context.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct GlRendererInfo {
    /// GL renderer string (e.g. "NVIDIA GeForce RTX 3080/PCIe/SSE2")
    pub renderer: String,
    /// GL version string (e.g. "4.6.0 NVIDIA 535.183.01")
    pub version: String,
    /// GL vendor string (e.g. "NVIDIA Corporation")
    pub vendor: String,
    /// GLSL version string (e.g. "4.60 NVIDIA")
    pub glsl_version: String,
}

/// System monitoring statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SystemStats {
    /// CPU usage percentage (0-100)
    pub cpu_usage: f32,
    /// Number of CPU cores available to this process (cgroup-aware)
    pub num_cores: usize,
    /// Total system memory in bytes
    pub total_memory: u64,
    /// Used system memory in bytes
    pub used_memory: u64,
    /// GPU statistics (if available)
    pub gpu_stats: Vec<GpuStats>,
    /// OpenGL renderer info from GStreamer (if available)
    pub gl_renderer: Option<GlRendererInfo>,
    /// Timestamp of the measurement
    pub timestamp: i64,
}

/// GPU statistics for a single GPU device.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct GpuStats {
    /// GPU device index
    pub index: u32,
    /// GPU device name
    pub name: String,
    /// GPU utilization percentage (0-100)
    pub utilization: f32,
    /// Memory utilization percentage (0-100)
    pub memory_utilization: f32,
    /// Total memory in bytes
    pub total_memory: u64,
    /// Used memory in bytes
    pub used_memory: u64,
    /// Temperature in Celsius (if available)
    pub temperature: Option<f32>,
    /// Power usage in watts (if available)
    pub power_usage: Option<f32>,
    /// Hardware video decoder (e.g. NVDEC) utilization percentage (if available)
    #[serde(default)]
    pub decoder_utilization: Option<f32>,
    /// Hardware video encoder (e.g. NVENC) utilization percentage (if available)
    #[serde(default)]
    pub encoder_utilization: Option<f32>,
}
