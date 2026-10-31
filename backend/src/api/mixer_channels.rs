//! Dynamic channels of a running audio mixer: list, add, remove.
//!
//! A mixer takes dynamic channels when its `max_channels` property reserves
//! more channel numbers than `num_channels`. A dynamic channel takes its
//! sound from an inter channel, so adding one needs no flow link and leaves
//! Main, the buses and the outputs running.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use strom_types::api::ErrorResponse;
use strom_types::mixer::{AddMixerChannelRequest, MixerChannels};
use strom_types::FlowId;
use tracing::info;

use crate::blocks::builtin::mixer::dynamic_channels::ChannelError;
use crate::state::AppState;

type ApiError = (StatusCode, Json<ErrorResponse>);

fn error_response(what: &str, e: ChannelError) -> ApiError {
    let status = match e {
        ChannelError::NotSupported(_) | ChannelError::Invalid(_) => StatusCode::BAD_REQUEST,
        ChannelError::Conflict(_) => StatusCode::CONFLICT,
        ChannelError::NotFound(_) => StatusCode::NOT_FOUND,
        ChannelError::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(ErrorResponse::with_details(what, e.to_string())),
    )
}

/// List the channels of an audio mixer: its capacity, the number of static
/// channels and the dynamic ones with their sources.
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/blocks/{block_id}/channels",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Audio mixer block instance ID")
    ),
    responses(
        (status = 200, description = "The mixer's channels", body = MixerChannels),
        (status = 404, description = "No audio mixer with this ID", body = ErrorResponse),
        (status = 409, description = "The stored channel list does not parse", body = ErrorResponse),
    )
)]
pub async fn list_mixer_channels(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Json<MixerChannels>, ApiError> {
    state
        .mixer_channels(&flow_id, &block_id)
        .await
        .map(Json)
        .map_err(|e| error_response("Failed to list mixer channels", e))
}

/// Add a channel to a running audio mixer. The channel takes the requested
/// free number (or the lowest free one), its sound from the inter channel
/// `source`, a send into every aux bus (level 0 until set with
/// `chN_auxM_level`) and its stored `chN_…` settings. It is stored with the
/// flow and built again on restart.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/channels",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Audio mixer block instance ID")
    ),
    request_body = AddMixerChannelRequest,
    responses(
        (status = 201, description = "Channel added; the mixer's channels afterwards", body = MixerChannels),
        (status = 400, description = "Invalid request, or the mixer has no free channels (max_channels)", body = ErrorResponse),
        (status = 404, description = "No running audio mixer with this ID", body = ErrorResponse),
        (status = 409, description = "The requested number is taken, or every number is", body = ErrorResponse),
        (status = 500, description = "GStreamer could not build the channel", body = ErrorResponse),
    )
)]
pub async fn add_mixer_channel(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    Json(request): Json<AddMixerChannelRequest>,
) -> Result<(StatusCode, Json<MixerChannels>), ApiError> {
    info!(
        "Adding channel {:?} from {:?} to mixer {} in flow {}",
        request.channel, request.source, block_id, flow_id
    );
    state
        .add_mixer_channel(&flow_id, &block_id, request)
        .await
        .map(|channels| (StatusCode::CREATED, Json(channels)))
        .map_err(|e| error_response("Failed to add mixer channel", e))
}

/// Remove a dynamic channel from an audio mixer. Its stored settings
/// (`chN_…`) go with it.
#[utoipa::path(
    delete,
    path = "/api/flows/{flow_id}/blocks/{block_id}/channels/{channel}",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Audio mixer block instance ID"),
        ("channel" = usize, Path, description = "Channel number (as in chN_fader)")
    ),
    responses(
        (status = 200, description = "Channel removed; the mixer's channels afterwards", body = MixerChannels),
        (status = 400, description = "Channel 0 (channels are numbered from 1)", body = ErrorResponse),
        (status = 404, description = "No audio mixer, or no dynamic channel with this number", body = ErrorResponse),
        (status = 409, description = "A static channel (num_channels, also one stored as dynamic before num_channels grew), or the stored channel list does not parse", body = ErrorResponse),
    )
)]
pub async fn remove_mixer_channel(
    State(state): State<AppState>,
    Path((flow_id, block_id, channel)): Path<(FlowId, String, usize)>,
) -> Result<Json<MixerChannels>, ApiError> {
    info!(
        "Removing channel {} from mixer {} in flow {}",
        channel, block_id, flow_id
    );
    state
        .remove_mixer_channel(&flow_id, &block_id, channel)
        .await
        .map(Json)
        .map_err(|e| error_response("Failed to remove mixer channel", e))
}
