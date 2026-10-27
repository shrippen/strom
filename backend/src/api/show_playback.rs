//! Show Playback API handlers: sounds and pictures in a running Show
//! Playback block.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use std::sync::Arc;
use strom_types::show_playback::{
    AddPictureRequest, AddSoundRequest, PictureFadeRequest, ShowPlaybackState, SoundFadeRequest,
    SoundStopRequest, StageLevelRequest, ZoneLevelRequest,
};
use strom_types::{api::ErrorResponse, FlowId};

use crate::blocks::builtin::show_playback::{self, picture, sound, ShowPlayback, ShowPlaybackKey};
use crate::state::AppState;

type ApiError = (StatusCode, Json<ErrorResponse>);

fn block(flow_id: FlowId, block_id: String) -> Result<Arc<ShowPlayback>, ApiError> {
    show_playback::running(&ShowPlaybackKey { flow_id, block_id }).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new(
            "No running Show Playback block of that id - is the flow running?",
        )),
    ))
}

/// Run a sound call off the async runtime: building a branch changes the
/// pipeline, and taking one down waits for its elements.
async fn call<F>(f: F) -> Result<StatusCode, ApiError>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(())) => Ok(StatusCode::NO_CONTENT),
        Ok(Err(e)) => Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorResponse::new(e)),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(format!(
                "show playback call failed: {}",
                e
            ))),
        )),
    }
}

/// The block's zones, stages, sounds and pictures.
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_playback",
    tag = "show_playback",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID")
    ),
    responses(
        (status = 200, description = "Zones, stages, sounds and pictures", body = ShowPlaybackState),
        (status = 404, description = "No running Show Playback block", body = ErrorResponse)
    )
)]
pub async fn get_show_playback(
    State(_state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Json<ShowPlaybackState>, ApiError> {
    let block = block(flow_id, block_id)?;
    Ok(Json(sound::state(&block)))
}

/// Add a sound: preloaded to start at once on `play`, or played as soon as
/// it is decoded. A sound of the same id is replaced.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_playback/sounds",
    tag = "show_playback",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID")
    ),
    request_body = AddSoundRequest,
    responses(
        (status = 204, description = "Sound added"),
        (status = 404, description = "No running Show Playback block", body = ErrorResponse),
        (status = 422, description = "The sound cannot be added (zone, channel, limit)", body = ErrorResponse)
    )
)]
pub async fn add_sound(
    State(_state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    Json(request): Json<AddSoundRequest>,
) -> Result<StatusCode, ApiError> {
    let block = block(flow_id, block_id)?;
    call(move || sound::add(&block, &request)).await
}

/// Play a sound.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_playback/sounds/{sound_id}/play",
    tag = "show_playback",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("sound_id" = String, Path, description = "Sound ID")
    ),
    responses(
        (status = 204, description = "Playing, or plays once decoded"),
        (status = 404, description = "No running Show Playback block", body = ErrorResponse),
        (status = 422, description = "No such sound, or it has ended", body = ErrorResponse)
    )
)]
pub async fn play_sound(
    State(_state): State<AppState>,
    Path((flow_id, block_id, sound_id)): Path<(FlowId, String, String)>,
) -> Result<StatusCode, ApiError> {
    let block = block(flow_id, block_id)?;
    call(move || sound::play(&block, &sound_id)).await
}

/// Fade a sound's level along an S-curve, optionally stopping it at the end.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_playback/sounds/{sound_id}/fade",
    tag = "show_playback",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("sound_id" = String, Path, description = "Sound ID")
    ),
    request_body = SoundFadeRequest,
    responses(
        (status = 204, description = "Fading"),
        (status = 404, description = "No running Show Playback block", body = ErrorResponse),
        (status = 422, description = "No such sound", body = ErrorResponse)
    )
)]
pub async fn fade_sound(
    State(_state): State<AppState>,
    Path((flow_id, block_id, sound_id)): Path<(FlowId, String, String)>,
    Json(request): Json<SoundFadeRequest>,
) -> Result<StatusCode, ApiError> {
    let block = block(flow_id, block_id)?;
    call(move || sound::fade(&block, &sound_id, &request)).await
}

/// Stop a sound, faded out over `fade_ms`.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_playback/sounds/{sound_id}/stop",
    tag = "show_playback",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("sound_id" = String, Path, description = "Sound ID")
    ),
    request_body = SoundStopRequest,
    responses(
        (status = 204, description = "Stopping"),
        (status = 404, description = "No running Show Playback block", body = ErrorResponse),
        (status = 422, description = "No such sound", body = ErrorResponse)
    )
)]
pub async fn stop_sound(
    State(_state): State<AppState>,
    Path((flow_id, block_id, sound_id)): Path<(FlowId, String, String)>,
    Json(request): Json<SoundStopRequest>,
) -> Result<StatusCode, ApiError> {
    let block = block(flow_id, block_id)?;
    call(move || sound::stop(&block, &sound_id, request.fade_ms)).await
}

/// Set a zone's level and mute, faded.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_playback/zones/{zone}",
    tag = "show_playback",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("zone" = usize, Path, description = "Zone index, from 0")
    ),
    request_body = ZoneLevelRequest,
    responses(
        (status = 204, description = "Level set"),
        (status = 404, description = "No running Show Playback block", body = ErrorResponse),
        (status = 422, description = "No such zone", body = ErrorResponse)
    )
)]
pub async fn set_zone_level(
    State(_state): State<AppState>,
    Path((flow_id, block_id, zone)): Path<(FlowId, String, usize)>,
    Json(request): Json<ZoneLevelRequest>,
) -> Result<StatusCode, ApiError> {
    let block = block(flow_id, block_id)?;
    call(move || sound::set_zone(&block, zone, &request)).await
}

/// Show a picture on a stage, preloaded or at once; one of the same id is replaced.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_playback/pictures",
    tag = "show_playback",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID")
    ),
    request_body = AddPictureRequest,
    responses(
        (status = 204, description = "Picture added"),
        (status = 404, description = "No running Show Playback block", body = ErrorResponse),
        (status = 422, description = "No such stage, or too many pictures", body = ErrorResponse)
    )
)]
pub async fn add_picture(
    State(_state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    Json(request): Json<AddPictureRequest>,
) -> Result<StatusCode, ApiError> {
    let block = block(flow_id, block_id)?;
    call(move || picture::add(&block, &request)).await
}

/// Show a preloaded picture.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_playback/pictures/{picture_id}/play",
    tag = "show_playback",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("picture_id" = String, Path, description = "Picture ID")
    ),
    responses(
        (status = 204, description = "Showing"),
        (status = 404, description = "No running Show Playback block", body = ErrorResponse),
        (status = 422, description = "No such picture", body = ErrorResponse)
    )
)]
pub async fn play_picture(
    State(_state): State<AppState>,
    Path((flow_id, block_id, picture_id)): Path<(FlowId, String, String)>,
) -> Result<StatusCode, ApiError> {
    let block = block(flow_id, block_id)?;
    call(move || picture::play(&block, &picture_id)).await
}

/// Fade a picture's opacity along an S-curve, optionally stopping it at the end.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_playback/pictures/{picture_id}/fade",
    tag = "show_playback",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("picture_id" = String, Path, description = "Picture ID")
    ),
    request_body = PictureFadeRequest,
    responses(
        (status = 204, description = "Fading"),
        (status = 404, description = "No running Show Playback block", body = ErrorResponse),
        (status = 422, description = "No such picture", body = ErrorResponse)
    )
)]
pub async fn fade_picture(
    State(_state): State<AppState>,
    Path((flow_id, block_id, picture_id)): Path<(FlowId, String, String)>,
    Json(request): Json<PictureFadeRequest>,
) -> Result<StatusCode, ApiError> {
    let block = block(flow_id, block_id)?;
    call(move || picture::fade(&block, &picture_id, &request)).await
}

/// Take a picture off its stage, faded out.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_playback/pictures/{picture_id}/stop",
    tag = "show_playback",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("picture_id" = String, Path, description = "Picture ID")
    ),
    request_body = SoundStopRequest,
    responses(
        (status = 204, description = "Stopping"),
        (status = 404, description = "No running Show Playback block", body = ErrorResponse),
        (status = 422, description = "No such picture", body = ErrorResponse)
    )
)]
pub async fn stop_picture(
    State(_state): State<AppState>,
    Path((flow_id, block_id, picture_id)): Path<(FlowId, String, String)>,
    Json(request): Json<SoundStopRequest>,
) -> Result<StatusCode, ApiError> {
    let block = block(flow_id, block_id)?;
    call(move || picture::stop(&block, &picture_id, request.fade_ms)).await
}

/// Set a stage's brightness (its dimmer), faded.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_playback/stages/{stage}",
    tag = "show_playback",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("stage" = usize, Path, description = "Stage index, from 0")
    ),
    request_body = StageLevelRequest,
    responses(
        (status = 204, description = "Brightness set"),
        (status = 404, description = "No running Show Playback block", body = ErrorResponse),
        (status = 422, description = "No such stage", body = ErrorResponse)
    )
)]
pub async fn set_stage_level(
    State(_state): State<AppState>,
    Path((flow_id, block_id, stage)): Path<(FlowId, String, usize)>,
    Json(request): Json<StageLevelRequest>,
) -> Result<StatusCode, ApiError> {
    let block = block(flow_id, block_id)?;
    call(move || picture::set_stage(&block, stage, &request)).await
}

/// Pause a sound where it is (RUN.5).
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_playback/sounds/{sound_id}/pause",
    tag = "show_playback",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("sound_id" = String, Path, description = "Sound ID")
    ),
    responses(
        (status = 204, description = "Paused"),
        (status = 404, description = "No running Show Playback block", body = ErrorResponse),
        (status = 422, description = "No such sound, or it does not play", body = ErrorResponse)
    )
)]
pub async fn pause_sound(
    State(_state): State<AppState>,
    Path((flow_id, block_id, sound_id)): Path<(FlowId, String, String)>,
) -> Result<StatusCode, ApiError> {
    let block = block(flow_id, block_id)?;
    call(move || sound::pause(&block, &sound_id)).await
}

/// Resume a paused sound where it was.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_playback/sounds/{sound_id}/resume",
    tag = "show_playback",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("sound_id" = String, Path, description = "Sound ID")
    ),
    responses(
        (status = 204, description = "Playing again"),
        (status = 404, description = "No running Show Playback block", body = ErrorResponse),
        (status = 422, description = "No such sound, or it is not paused", body = ErrorResponse)
    )
)]
pub async fn resume_sound(
    State(_state): State<AppState>,
    Path((flow_id, block_id, sound_id)): Path<(FlowId, String, String)>,
) -> Result<StatusCode, ApiError> {
    let block = block(flow_id, block_id)?;
    call(move || sound::resume(&block, &sound_id)).await
}

/// Pause a picture: its frame stays, a video keeps its place.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_playback/pictures/{picture_id}/pause",
    tag = "show_playback",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("picture_id" = String, Path, description = "Picture ID")
    ),
    responses(
        (status = 204, description = "Paused"),
        (status = 404, description = "No running Show Playback block", body = ErrorResponse),
        (status = 422, description = "No such picture, or it does not play", body = ErrorResponse)
    )
)]
pub async fn pause_picture(
    State(_state): State<AppState>,
    Path((flow_id, block_id, picture_id)): Path<(FlowId, String, String)>,
) -> Result<StatusCode, ApiError> {
    let block = block(flow_id, block_id)?;
    call(move || picture::pause(&block, &picture_id)).await
}

/// Resume a paused picture where it was.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_playback/pictures/{picture_id}/resume",
    tag = "show_playback",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("picture_id" = String, Path, description = "Picture ID")
    ),
    responses(
        (status = 204, description = "Showing again"),
        (status = 404, description = "No running Show Playback block", body = ErrorResponse),
        (status = 422, description = "No such picture, or it is not paused", body = ErrorResponse)
    )
)]
pub async fn resume_picture(
    State(_state): State<AppState>,
    Path((flow_id, block_id, picture_id)): Path<(FlowId, String, String)>,
) -> Result<StatusCode, ApiError> {
    let block = block(flow_id, block_id)?;
    call(move || picture::resume(&block, &picture_id)).await
}
