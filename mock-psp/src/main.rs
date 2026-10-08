use axum::{
    Json, Router,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, time::Duration};
use tokio::time::sleep;
use tracing::info;
use uuid::Uuid;

#[derive(Deserialize, Debug)]
struct ChargeRequest {
    card_token: String,
    amount_cents: i64,
    #[allow(dead_code)]
    idempotency_key: Option<String>,
}

#[derive(Serialize)]
struct SuccessResponse {
    status: String,
    psp_ref: String,
}

#[derive(Serialize)]
struct FailureResponse {
    status: String,
    code: String,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

async fn health_check() -> impl IntoResponse {
    StatusCode::OK
}

async fn charge(Json(payload): Json<ChargeRequest>) -> Response {
    info!(
        card_token = %payload.card_token,
        amount_cents = %payload.amount_cents,
        "Received charge request"
    );

    match payload.card_token.as_str() {
        "tok_success" => {
            sleep(Duration::from_millis(100)).await;
            (
                StatusCode::OK,
                Json(SuccessResponse {
                    status: "succeeded".to_string(),
                    psp_ref: Uuid::new_v4().to_string(),
                }),
            )
                .into_response()
        }
        "tok_insufficient_funds" => {
            sleep(Duration::from_millis(100)).await;
            (
                StatusCode::OK,
                Json(FailureResponse {
                    status: "failed".to_string(),
                    code: "insufficient_funds".to_string(),
                }),
            )
                .into_response()
        }
        "tok_card_declined" => {
            sleep(Duration::from_millis(100)).await;
            (
                StatusCode::OK,
                Json(FailureResponse {
                    status: "failed".to_string(),
                    code: "card_declined".to_string(),
                }),
            )
                .into_response()
        }
        "tok_timeout" => {
            sleep(Duration::from_secs(30)).await;
            (
                StatusCode::OK,
                Json(SuccessResponse {
                    status: "succeeded".to_string(),
                    psp_ref: Uuid::new_v4().to_string(),
                }),
            )
                .into_response()
        }
        "tok_network_error" => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "internal_error".to_string(),
            }),
        )
            .into_response(),
        _ => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "invalid_token".to_string(),
            }),
        )
            .into_response(),
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "9090".to_string())
        .parse::<u16>()
        .expect("PORT must be a valid u16");

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/charge", post(charge));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("Mock PSP listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
