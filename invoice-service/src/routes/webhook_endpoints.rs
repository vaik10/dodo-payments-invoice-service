use axum::{Json, extract::State, http::StatusCode};
use rand::Rng;

use crate::AppState;
use crate::auth::AuthenticatedBusiness;
use crate::models::{
    CreateWebhookEndpointRequest, ErrorResponse, WebhookEndpoint, WebhookEndpointResponse,
};

pub async fn create_webhook_endpoint(
    State(state): State<AppState>,
    auth: AuthenticatedBusiness,
    Json(body): Json<CreateWebhookEndpointRequest>,
) -> Result<(StatusCode, Json<WebhookEndpointResponse>), (StatusCode, Json<ErrorResponse>)> {
    if body.url.trim().is_empty() || !body.url.starts_with("http") {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("validation_error", "Invalid URL")),
        ));
    }

    // Generate a 32-byte hex string for the signing secret
    let secret: String = (0..32)
        .map(|_| {
            let x: u8 = rand::rng().random_range(0..16);
            format!("{:x}", x)
        })
        .collect();

    let endpoint = sqlx::query_as::<_, WebhookEndpoint>(
        "INSERT INTO webhook_endpoints (id, business_id, url, signing_secret) 
         VALUES (gen_random_uuid(), $1, $2, $3) RETURNING *",
    )
    .bind(auth.business_id)
    .bind(&body.url)
    .bind(&secret)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Failed to create webhook endpoint: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("internal_error", "Database error")),
        )
    })?;

    Ok((StatusCode::CREATED, Json(endpoint.into())))
}
