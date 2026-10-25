//! Dynamic aux buses of a running audio mixer: list, add, remove.
//!
//! A mixer takes dynamic buses when its `max_aux_buses` property reserves
//! more bus numbers than `num_aux_buses`. A dynamic bus publishes its
//! output on an inter channel, so adding one needs no flow link and leaves
//! Main, the other buses and the outputs running.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use strom_types::api::ErrorResponse;
use strom_types::mixer::{AddMixerAuxBusRequest, MixerAuxBuses};
use strom_types::FlowId;
use tracing::info;

use crate::blocks::builtin::mixer::dynamic::AuxBusError;
use crate::state::AppState;

type ApiError = (StatusCode, Json<ErrorResponse>);

fn error_response(what: &str, e: AuxBusError) -> ApiError {
    let status = match e {
        AuxBusError::NotSupported(_) | AuxBusError::Invalid(_) => StatusCode::BAD_REQUEST,
        AuxBusError::Conflict(_) => StatusCode::CONFLICT,
        AuxBusError::NotFound(_) => StatusCode::NOT_FOUND,
        AuxBusError::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(ErrorResponse::with_details(what, e.to_string())),
    )
}

/// List the aux buses of an audio mixer: its capacity, the static buses
/// and the dynamic ones with their channels.
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/blocks/{block_id}/aux_buses",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Audio mixer block instance ID")
    ),
    responses(
        (status = 200, description = "The mixer's aux buses", body = MixerAuxBuses),
        (status = 404, description = "No audio mixer with this ID", body = ErrorResponse),
    )
)]
pub async fn list_mixer_aux_buses(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Json<MixerAuxBuses>, ApiError> {
    state
        .mixer_aux_buses(&flow_id, &block_id)
        .await
        .map(Json)
        .map_err(|e| error_response("Failed to list mixer aux buses", e))
}

/// Add an aux bus to a running audio mixer. The bus takes the requested
/// free number (or the lowest free one), gets a send from every channel
/// (level 0 until set with `chM_auxN_level`) and publishes its output on
/// the inter channel. It is stored with the flow and built again on
/// restart.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/aux_buses",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Audio mixer block instance ID")
    ),
    request_body = AddMixerAuxBusRequest,
    responses(
        (status = 201, description = "Bus added; the mixer's aux buses afterwards", body = MixerAuxBuses),
        (status = 400, description = "Invalid request, or the mixer has no free buses (max_aux_buses)", body = ErrorResponse),
        (status = 404, description = "No running audio mixer with this ID", body = ErrorResponse),
        (status = 409, description = "The requested number or the channel is taken, or every number is", body = ErrorResponse),
        (status = 500, description = "GStreamer could not build the bus", body = ErrorResponse),
    )
)]
pub async fn add_mixer_aux_bus(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    Json(request): Json<AddMixerAuxBusRequest>,
) -> Result<(StatusCode, Json<MixerAuxBuses>), ApiError> {
    info!(
        "Adding aux bus {:?} on {:?} to mixer {} in flow {}",
        request.aux, request.channel, block_id, flow_id
    );
    state
        .add_mixer_aux_bus(&flow_id, &block_id, request)
        .await
        .map(|buses| (StatusCode::CREATED, Json(buses)))
        .map_err(|e| error_response("Failed to add mixer aux bus", e))
}

/// Remove a dynamic aux bus from an audio mixer. Its stored properties
/// (fader, sends) go with it.
#[utoipa::path(
    delete,
    path = "/api/flows/{flow_id}/blocks/{block_id}/aux_buses/{aux}",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Audio mixer block instance ID"),
        ("aux" = usize, Path, description = "Aux bus number (as in auxN_fader)")
    ),
    responses(
        (status = 200, description = "Bus removed; the mixer's aux buses afterwards", body = MixerAuxBuses),
        (status = 404, description = "No audio mixer, or no dynamic bus with this number", body = ErrorResponse),
        (status = 409, description = "A static bus (num_aux_buses)", body = ErrorResponse),
    )
)]
pub async fn remove_mixer_aux_bus(
    State(state): State<AppState>,
    Path((flow_id, block_id, aux)): Path<(FlowId, String, usize)>,
) -> Result<Json<MixerAuxBuses>, ApiError> {
    info!(
        "Removing aux bus {} from mixer {} in flow {}",
        aux, block_id, flow_id
    );
    state
        .remove_mixer_aux_bus(&flow_id, &block_id, aux)
        .await
        .map(Json)
        .map_err(|e| error_response("Failed to remove mixer aux bus", e))
}
