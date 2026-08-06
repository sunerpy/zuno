use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use oc_pty::{CreateInput, PtyId, PtyInfo, UpdateInput};

use super::Data;
use super::error::ApiError;
use super::state::ApiState;

pub async fn list(State(state): State<ApiState>) -> Json<Data<Vec<PtyInfo>>> {
    Json(Data::new(state.pty().list()))
}

pub async fn create(
    State(state): State<ApiState>,
    Json(input): Json<CreateInput>,
) -> Result<Json<Data<PtyInfo>>, ApiError> {
    Ok(Json(Data::new(state.pty().create(input)?)))
}

pub async fn get(
    State(state): State<ApiState>,
    Path(pty_id): Path<String>,
) -> Result<Json<Data<PtyInfo>>, ApiError> {
    Ok(Json(Data::new(state.pty().get(&PtyId::from_raw(pty_id))?)))
}

pub async fn update(
    State(state): State<ApiState>,
    Path(pty_id): Path<String>,
    Json(input): Json<UpdateInput>,
) -> Result<Json<Data<PtyInfo>>, ApiError> {
    Ok(Json(Data::new(
        state.pty().update(&PtyId::from_raw(pty_id), input)?,
    )))
}

pub async fn remove(
    State(state): State<ApiState>,
    Path(pty_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.pty().remove(&PtyId::from_raw(pty_id))?;
    Ok(StatusCode::NO_CONTENT)
}
