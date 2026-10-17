//! Media player API types shared between backend and frontend.

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// Playback state of a media player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum PlayerState {
    Playing,
    Paused,
    Stopped,
}

impl std::fmt::Display for PlayerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Playing => write!(f, "playing"),
            Self::Paused => write!(f, "paused"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

/// Player control action.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum PlayerAction {
    Play,
    Pause,
    Stop,
    Next,
    Previous,
}

/// Request to control the media player.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PlayerControlRequest {
    pub action: PlayerAction,
}

/// Request to set the playlist.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[cfg_attr(feature = "validation", derive(garde::Validate))]
pub struct SetPlaylistRequest {
    /// List of file URIs (e.g., "file:///path/to/video.mp4")
    #[cfg_attr(feature = "validation", garde(length(min = 1)))]
    pub files: Vec<String>,
    /// Loop back to the first file at the end; unchanged when absent.
    #[serde(default)]
    #[cfg_attr(feature = "validation", garde(skip))]
    pub loop_playlist: Option<bool>,
}

/// Request to seek to a position.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SeekRequest {
    /// Position in nanoseconds
    pub position_ns: u64,
}

/// Request to go to a specific file.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct GotoRequest {
    /// File index (0-based)
    pub index: usize,
}

/// Response with the current player state.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PlayerStateResponse {
    /// Current playback state
    pub state: PlayerState,
    /// Current position in nanoseconds
    pub position_ns: u64,
    /// Total duration in nanoseconds
    pub duration_ns: u64,
    /// Current file index (0-based)
    pub current_file_index: usize,
    /// Total number of files in playlist
    pub total_files: usize,
    /// Current file path/URI
    pub current_file: Option<String>,
    /// Full playlist
    pub playlist: Vec<String>,
    /// Whether playlist loops
    pub loop_playlist: bool,
}
