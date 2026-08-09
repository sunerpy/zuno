use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Serialize;
use serde_json::Value;

use super::Data;
use super::error::ApiError;
use super::state::ApiState;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LocationResponse {
    location: Location,
    data: Vec<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Location {
    directory: String,
    project: Project,
}

#[derive(Debug, Serialize)]
struct Project {
    id: &'static str,
    directory: String,
}

fn empty_location(state: &ApiState) -> LocationResponse {
    LocationResponse {
        location: Location {
            directory: state.directory().to_owned(),
            project: Project {
                id: oc_paths::GLOBAL_PROJECT_ID,
                directory: state.directory().to_owned(),
            },
        },
        data: Vec::new(),
    }
}

pub async fn permission_requests(State(state): State<ApiState>) -> Json<impl Serialize> {
    Json(empty_location(&state))
}

pub async fn question_requests(State(state): State<ApiState>) -> Json<impl Serialize> {
    Json(empty_location(&state))
}

pub async fn saved_permissions() -> Json<Data<Vec<Value>>> {
    Json(Data::new(Vec::new()))
}

pub async fn remove_saved_permission(Path(_id): Path<String>) -> StatusCode {
    StatusCode::NO_CONTENT
}

pub async fn session_questions(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
) -> Result<Json<Data<Vec<Value>>>, ApiError> {
    state.sessions().get(&session_id)?;
    Ok(Json(Data::new(Vec::new())))
}
