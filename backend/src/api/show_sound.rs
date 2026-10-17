//! Show Sound API handlers: sound cues in a running Show Sound block.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use std::sync::Arc;
use strom_types::show_sound::{
    AddSoundRequest, ShowSoundState, SoundFadeRequest, SoundStopRequest, ZoneLevelRequest,
};
use strom_types::{api::ErrorResponse, FlowId};

use crate::blocks::builtin::show_sound::{self, sound, ShowSound, ShowSoundKey};
use crate::state::AppState;

type ApiError = (StatusCode, Json<ErrorResponse>);

fn block(flow_id: FlowId, block_id: String) -> Result<Arc<ShowSound>, ApiError> {
    show_sound::running(&ShowSoundKey { flow_id, block_id }).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new(
            "No running Show Sound block of that id - is the flow running?",
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
            Json(ErrorResponse::new(format!("sound call failed: {}", e))),
        )),
    }
}

/// The block's zones and sounds.
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_sound",
    tag = "show_sound",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID")
    ),
    responses(
        (status = 200, description = "Zones and sounds", body = ShowSoundState),
        (status = 404, description = "No running Show Sound block", body = ErrorResponse)
    )
)]
pub async fn get_show_sound(
    State(_state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Json<ShowSoundState>, ApiError> {
    let block = block(flow_id, block_id)?;
    Ok(Json(sound::state(&block)))
}

/// Add a sound: preloaded to start at once on `play`, or played as soon as
/// it is decoded. A sound of the same id is replaced.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_sound/sounds",
    tag = "show_sound",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID")
    ),
    request_body = AddSoundRequest,
    responses(
        (status = 204, description = "Sound added"),
        (status = 404, description = "No running Show Sound block", body = ErrorResponse),
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
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_sound/sounds/{sound_id}/play",
    tag = "show_sound",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("sound_id" = String, Path, description = "Sound ID")
    ),
    responses(
        (status = 204, description = "Playing, or plays once decoded"),
        (status = 404, description = "No running Show Sound block", body = ErrorResponse),
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
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_sound/sounds/{sound_id}/fade",
    tag = "show_sound",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("sound_id" = String, Path, description = "Sound ID")
    ),
    request_body = SoundFadeRequest,
    responses(
        (status = 204, description = "Fading"),
        (status = 404, description = "No running Show Sound block", body = ErrorResponse),
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
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_sound/sounds/{sound_id}/stop",
    tag = "show_sound",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("sound_id" = String, Path, description = "Sound ID")
    ),
    request_body = SoundStopRequest,
    responses(
        (status = 204, description = "Stopping"),
        (status = 404, description = "No running Show Sound block", body = ErrorResponse),
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
    path = "/api/flows/{flow_id}/blocks/{block_id}/show_sound/zones/{zone}",
    tag = "show_sound",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID"),
        ("zone" = usize, Path, description = "Zone index, from 0")
    ),
    request_body = ZoneLevelRequest,
    responses(
        (status = 204, description = "Level set"),
        (status = 404, description = "No running Show Sound block", body = ErrorResponse),
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
