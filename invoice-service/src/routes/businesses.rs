use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::auth::generate_api_key;
use crate::models::{Business, ErrorResponse};

#[derive(Deserialize)]
pub struct CreateBusinessRequest {
    pub name: String,
}

#[derive(Serialize)]
pub struct CreateApiKeyResponse {
    pub id: Uuid,
    pub raw_key: String,
}

pub async fn create_business(
    State(state): State<AppState>,
    Json(body): Json<CreateBusinessRequest>,
) -> Result<(StatusCode, Json<Business>), (StatusCode, Json<ErrorResponse>)> {
    if body.name.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "validation_error",
                "Name cannot be empty",
            )),
        ));
    }

    let business = sqlx::query_as::<_, Business>(
        "INSERT INTO businesses (id, name) VALUES (gen_random_uuid(), $1) RETURNING *",
    )
    .bind(&body.name)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Failed to create business: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("internal_error", "Database error")),
        )
    })?;

    Ok((StatusCode::CREATED, Json(business)))
}

pub async fn create_api_key(
    State(state): State<AppState>,
    Path(business_id): Path<Uuid>,
) -> Result<(StatusCode, Json<CreateApiKeyResponse>), (StatusCode, Json<ErrorResponse>)> {
    // Verify business exists
    let exists: (bool,) = sqlx::query_as("SELECT EXISTS(SELECT 1 FROM businesses WHERE id = $1)")
        .bind(business_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or((false,));

    if !exists.0 {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("not_found", "Business not found")),
        ));
    }

    let (raw_key, prefix, hash) = generate_api_key();
    let id = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO api_keys (id, business_id, key_prefix, key_hash) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(business_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Failed to create API key: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("internal_error", "Database error")),
        )
    })?;

    Ok((
        StatusCode::CREATED,
        Json(CreateApiKeyResponse { id, raw_key }),
    ))
}
