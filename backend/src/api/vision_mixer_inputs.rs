//! Dynamic inputs of a running vision mixer: list, add, remove.
//!
//! A mixer takes dynamic inputs when its `max_inputs` property reserves
//! more input slots than `num_inputs` (CPU and CUDA backends). A dynamic
//! input is fed from an inter channel inside the mixer, so adding one needs
//! no flow link and leaves program, audio and the outputs running.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use strom_types::api::ErrorResponse;
use strom_types::vision_mixer::{AddVisionMixerInputRequest, VisionMixerInputs};
use strom_types::FlowId;
use tracing::info;

use crate::blocks::builtin::vision_mixer::dynamic::DynamicInputError;
use crate::state::AppState;

type ApiError = (StatusCode, Json<ErrorResponse>);

fn error_response(what: &str, e: DynamicInputError) -> ApiError {
    let status = match e {
        DynamicInputError::NotSupported(_) | DynamicInputError::Invalid(_) => {
            StatusCode::BAD_REQUEST
        }
        DynamicInputError::Conflict(_) => StatusCode::CONFLICT,
        DynamicInputError::NotFound(_) => StatusCode::NOT_FOUND,
        DynamicInputError::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(ErrorResponse::with_details(what, e.to_string())),
    )
}

/// List the inputs of a running vision mixer: its slots, the static inputs
/// and the dynamic ones with their sources.
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/blocks/{block_id}/inputs",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID")
    ),
    responses(
        (status = 200, description = "The mixer's inputs", body = VisionMixerInputs),
        (status = 404, description = "No running vision mixer with this ID", body = ErrorResponse),
    )
)]
pub async fn list_vision_mixer_inputs(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Json<VisionMixerInputs>, ApiError> {
    state
        .vision_mixer_inputs(&flow_id, &block_id)
        .await
        .map(Json)
        .map_err(|e| error_response("Failed to list vision mixer inputs", e))
}

/// Add an input to a running vision mixer, fed from an inter channel. The
/// input takes the requested free slot (or the lowest free one), shows as
/// a multiview thumbnail and can then be put on preview and program like
/// any other input. It is stored with the flow and built again on restart.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/inputs",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID")
    ),
    request_body = AddVisionMixerInputRequest,
    responses(
        (status = 201, description = "Input added; the mixer's inputs afterwards", body = VisionMixerInputs),
        (status = 400, description = "Invalid request, or the mixer has no free slots (max_inputs) or no dynamic inputs (OpenGL backend)", body = ErrorResponse),
        (status = 404, description = "No running vision mixer with this ID", body = ErrorResponse),
        (status = 409, description = "The requested slot is taken, or every slot is", body = ErrorResponse),
        (status = 500, description = "GStreamer could not build the input", body = ErrorResponse),
    )
)]
pub async fn add_vision_mixer_input(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    Json(request): Json<AddVisionMixerInputRequest>,
) -> Result<(StatusCode, Json<VisionMixerInputs>), ApiError> {
    info!(
        "Adding input {:?} from {:?} to vision mixer {} in flow {}",
        request.index, request.source, block_id, flow_id
    );
    state
        .add_vision_mixer_input(&flow_id, &block_id, request)
        .await
        .map(|(_, inputs)| (StatusCode::CREATED, Json(inputs)))
        .map_err(|e| error_response("Failed to add vision mixer input", e))
}

/// Remove a dynamic input from a running vision mixer. Refused while the
/// input is on program or preview, part of a PiP, or a transition runs.
#[utoipa::path(
    delete,
    path = "/api/flows/{flow_id}/blocks/{block_id}/inputs/{index}",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID"),
        ("index" = usize, Path, description = "Input index")
    ),
    responses(
        (status = 200, description = "Input removed; the mixer's inputs afterwards", body = VisionMixerInputs),
        (status = 404, description = "No running vision mixer, or no dynamic input at this index", body = ErrorResponse),
        (status = 409, description = "A static input, or the input is on program or preview, in a PiP, or a transition runs", body = ErrorResponse),
    )
)]
pub async fn remove_vision_mixer_input(
    State(state): State<AppState>,
    Path((flow_id, block_id, index)): Path<(FlowId, String, usize)>,
) -> Result<Json<VisionMixerInputs>, ApiError> {
    info!(
        "Removing input {} from vision mixer {} in flow {}",
        index, block_id, flow_id
    );
    state
        .remove_vision_mixer_input(&flow_id, &block_id, index)
        .await
        .map(Json)
        .map_err(|e| error_response("Failed to remove vision mixer input", e))
}
