use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::sleep;
use tracing::info;
use uuid::Uuid;

#[derive(Deserialize, Debug)]
struct ChargeRequest {
    card_token: String,
    amount_cents: i64,
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

/// What the PSP knows about a charge, keyed by the caller's idempotency key.
#[derive(Clone)]
enum ChargeRecord {
    Processing,
    Succeeded { psp_ref: String },
    Failed { code: String },
}

impl IntoResponse for ChargeRecord {
    fn into_response(self) -> Response {
        match self {
            ChargeRecord::Processing => (
                StatusCode::OK,
                Json(serde_json::json!({"status": "processing"})),
            )
                .into_response(),
            ChargeRecord::Succeeded { psp_ref } => (
                StatusCode::OK,
                Json(SuccessResponse {
                    status: "succeeded".to_string(),
                    psp_ref,
                }),
            )
                .into_response(),
            ChargeRecord::Failed { code } => (
                StatusCode::OK,
                Json(FailureResponse {
                    status: "failed".to_string(),
                    code,
                }),
            )
                .into_response(),
        }
    }
}

type Charges = Arc<Mutex<HashMap<String, ChargeRecord>>>;

async fn health_check() -> impl IntoResponse {
    StatusCode::OK
}

async fn charge(State(charges): State<Charges>, Json(payload): Json<ChargeRequest>) -> Response {
    info!(
        card_token = %payload.card_token,
        amount_cents = %payload.amount_cents,
        "Received charge request"
    );

    let key = payload.idempotency_key.clone();
    let record = |record: ChargeRecord| {
        if let Some(key) = &key {
            charges.lock().unwrap().insert(key.clone(), record);
        }
    };

    // Like a real PSP, a repeated key returns the original result instead of
    // charging again.
    if let Some(key) = &key {
        let existing = charges.lock().unwrap().get(key).cloned();
        if let Some(existing @ (ChargeRecord::Succeeded { .. } | ChargeRecord::Failed { .. })) =
            existing
        {
            info!(idempotency_key = %key, "Replaying result for repeated idempotency key");
            return existing.into_response();
        }
    }

    match payload.card_token.as_str() {
        "tok_success" => {
            record(ChargeRecord::Processing);
            sleep(Duration::from_millis(100)).await;
            let result = ChargeRecord::Succeeded {
                psp_ref: Uuid::new_v4().to_string(),
            };
            record(result.clone());
            result.into_response()
        }
        "tok_insufficient_funds" | "tok_card_declined" => {
            record(ChargeRecord::Processing);
            sleep(Duration::from_millis(100)).await;
            let result = ChargeRecord::Failed {
                code: payload.card_token["tok_".len()..].to_string(),
            };
            record(result.clone());
            result.into_response()
        }
        "tok_timeout" => {
            // The charge goes through, but only after the caller has given up.
            // A lookup during the 30s reports `processing`, then `succeeded`.
            record(ChargeRecord::Processing);
            // Spawned because axum drops this handler when the caller disconnects,
            // and the charge must still complete, as it would at a real PSP.
            let charges = charges.clone();
            let completion = tokio::spawn(async move {
                sleep(Duration::from_secs(30)).await;
                let result = ChargeRecord::Succeeded {
                    psp_ref: Uuid::new_v4().to_string(),
                };
                if let Some(key) = key {
                    charges.lock().unwrap().insert(key, result.clone());
                }
                result
            });
            completion.await.unwrap().into_response()
        }
        // Fails before any charge is made, so a lookup finds nothing (404).
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

async fn get_charge(State(charges): State<Charges>, Path(key): Path<String>) -> Response {
    match charges.lock().unwrap().get(&key).cloned() {
        Some(record) => record.into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "charge_not_found".to_string(),
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
        .route("/charge", post(charge))
        .route("/charges/{idempotency_key}", get(get_charge))
        .with_state(Charges::default());

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("Mock PSP listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
