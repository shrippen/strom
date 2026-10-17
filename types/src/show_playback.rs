//! Types for the Show Playback block (`builtin.show_playback`): sound zones
//! and stages, and the sounds and pictures played into them at run time.

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// Most zones one block carries.
pub const MAX_ZONES: usize = 16;

/// Most channels of one zone.
pub const MAX_ZONE_CHANNELS: u32 = 8;

/// Most sounds one block plays at once.
pub const MAX_SOUNDS: usize = 64;

/// Gain range of cues, crosspoints and zones (dB): the lowest is silence.
pub const GAIN_MIN_DB: f64 = -60.0;
pub const GAIN_MAX_DB: f64 = 12.0;

/// Longest fade (ms).
pub const FADE_MAX_MS: u64 = 600_000;

/// Most stages one block carries.
pub const MAX_STAGES: usize = 8;

/// Most pictures one block shows at once.
pub const MAX_PICTURES: usize = 64;

/// Largest stage side (px).
pub const MAX_STAGE_PX: u32 = 7680;

/// Highest picture layer; a higher layer is in front.
pub const MAX_LAYER: u32 = 1000;

/// One zone: a named group of output channels, one block output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ShowZone {
    pub name: String,
    /// 1 to [`MAX_ZONE_CHANNELS`].
    pub channels: u32,
}

/// One stage: a picture surface of its own, one block output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ShowStage {
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// One crosspoint: a channel of the sound's file into a channel of a zone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SoundRoute {
    /// File channel, from 0.
    pub channel: u32,
    /// Zone, by its index in the block's `zones`.
    pub zone: usize,
    /// Zone channel, from 0.
    pub zone_channel: u32,
    /// Level of this crosspoint (dB).
    #[serde(default)]
    pub gain_db: f64,
}

/// When a sound or picture added to the block starts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SoundStart {
    /// Decode its start and wait for `play`, so it is heard at once then.
    #[default]
    Preload,
    /// Play as soon as it is decoded.
    Play,
}

/// How often a sound or video plays (SND.3, VIS.2); its plays follow
/// each other without a gap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SoundRepeat {
    #[default]
    Once,
    /// This many plays in all (1 is once).
    Times(u32),
    /// Until it is stopped.
    Forever,
}

/// Add a sound to the block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct AddSoundRequest {
    /// Name of the sound in this block; a sound of the same id is replaced.
    pub id: String,
    /// `file://` URI (or a path in the media folder) of the sound file.
    pub uri: String,
    /// Level of the whole sound (dB).
    #[serde(default)]
    pub gain_db: f64,
    /// Where its channels go. Empty: each file channel to the same channel
    /// of the first zone.
    #[serde(default)]
    pub routes: Vec<SoundRoute>,
    #[serde(default)]
    pub start: SoundStart,
    #[serde(default)]
    pub repeat: SoundRepeat,
}

/// What a fade leaves behind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum FadeEnd {
    /// The sound plays on at the new level.
    #[default]
    Keep,
    /// The sound stops when the fade ends.
    Stop,
}

/// Fade a sound's level (S-curve).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SoundFadeRequest {
    /// Target level (dB); [`GAIN_MIN_DB`] or lower is silence.
    pub gain_db: f64,
    pub duration_ms: u64,
    #[serde(default)]
    pub end: FadeEnd,
}

/// Stop a sound, faded out over `fade_ms`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SoundStopRequest {
    #[serde(default)]
    pub fade_ms: u64,
}

/// Set a zone's level and mute, faded over `duration_ms`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ZoneLevelRequest {
    #[serde(default)]
    pub gain_db: Option<f64>,
    #[serde(default)]
    pub mute: Option<bool>,
    #[serde(default)]
    pub duration_ms: u64,
}

/// How a picture fills its stage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum PictureFit {
    /// Whole picture, aspect kept, bars where it does not fill.
    #[default]
    Fit,
    /// The stage filled, aspect kept, the picture cut where it overhangs.
    Fill,
    /// The stage filled, aspect changed.
    Stretch,
}

/// What a video does at its end.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum PictureEnd {
    /// Its last frame stays until it is stopped.
    #[default]
    Hold,
    /// It leaves the stage.
    Stop,
}

/// Show a picture (video or still image) on a stage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct AddPictureRequest {
    /// Name of the picture in this block; a picture of the same id is
    /// replaced.
    pub id: String,
    /// `file://` URI (or a path in the media folder) of the file.
    pub uri: String,
    /// Stage, by its index in the block's `stages`.
    #[serde(default)]
    pub stage: usize,
    /// 0 to [`MAX_LAYER`]; on the same layer the later picture is in front.
    #[serde(default)]
    pub layer: u32,
    /// 0 (clear) to 1.
    #[serde(default = "one")]
    pub opacity: f64,
    #[serde(default)]
    pub fit: PictureFit,
    #[serde(default)]
    pub end: PictureEnd,
    #[serde(default)]
    pub start: SoundStart,
    /// Plays of a video; `end` applies after the last (a still image
    /// shows until stopped anyway).
    #[serde(default)]
    pub repeat: SoundRepeat,
}

fn one() -> f64 {
    1.0
}

/// Fade a picture's opacity (S-curve).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PictureFadeRequest {
    pub opacity: f64,
    pub duration_ms: u64,
    #[serde(default)]
    pub end: FadeEnd,
}

/// Set a stage's brightness (dimmer, 0 = black), faded over `duration_ms`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct StageLevelRequest {
    pub brightness: f64,
    #[serde(default)]
    pub duration_ms: u64,
}

/// Where a sound or picture is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SoundState {
    /// Being decoded.
    Loading,
    /// Decoded, waiting for `play`.
    Ready,
    Playing,
    /// Played to its end, or stopped; gone on the next state read.
    Ended,
    /// Could not be played; `error` says why.
    Failed,
}

/// One sound of the block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SoundInfo {
    pub id: String,
    pub state: SoundState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The block's sounds and pictures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ShowPlaybackState {
    pub zones: Vec<ShowZone>,
    pub sounds: Vec<SoundInfo>,
    #[serde(default)]
    pub stages: Vec<ShowStage>,
    /// The pictures on the stages.
    #[serde(default)]
    pub pictures: Vec<SoundInfo>,
}

/// Linear gain of `db`; [`GAIN_MIN_DB`] and below is 0.
pub fn gain_of_db(db: f64) -> f64 {
    if db <= GAIN_MIN_DB {
        0.0
    } else {
        10f64.powf(db.min(GAIN_MAX_DB) / 20.0)
    }
}

/// The S-curve fades follow: 0 at 0, 1 at 1, flat at both ends.
pub fn s_curve(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    (1.0 - (t * std::f64::consts::PI).cos()) / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_is_silent_at_the_floor_and_capped_at_the_top() {
        assert_eq!(gain_of_db(0.0), 1.0);
        assert_eq!(gain_of_db(GAIN_MIN_DB), 0.0);
        assert_eq!(gain_of_db(-100.0), 0.0);
        assert!((gain_of_db(-6.0) - 0.501).abs() < 0.001);
        assert_eq!(gain_of_db(40.0), gain_of_db(GAIN_MAX_DB));
    }

    #[test]
    fn the_s_curve_starts_and_ends_flat() {
        assert_eq!(s_curve(0.0), 0.0);
        assert!((s_curve(1.0) - 1.0).abs() < 1e-12);
        assert!((s_curve(0.5) - 0.5).abs() < 1e-12);
        assert!(s_curve(0.05) < 0.01);
    }
}
