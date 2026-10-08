use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use sha2::{Digest, Sha256};
use tracing::error;
use uuid::Uuid;

use crate::{
    AppState,
    auth::AuthenticatedBusiness,
    models::{
        ErrorResponse, Invoice, PayInvoiceRequest, PaymentAttemptResponse, PspChargeRequest,
        PspChargeResponse,
    },
    webhooks::enqueue_webhook,
};

pub async fn pay_invoice(
    State(state): State<AppState>,
    auth: AuthenticatedBusiness,
    Path(invoice_id): Path<Uuid>,
    headers: HeaderMap,
    Json(body): Json<PayInvoiceRequest>,
) -> Result<(StatusCode, Json<PaymentAttemptResponse>), (StatusCode, Json<ErrorResponse>)> {
    let idem_key = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let idem_key = match idem_key {
        Some(key) => key,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "missing_idempotency_key",
                    "Idempotency-Key header is required",
                )),
            ));
        }
    };

    let mut hasher = Sha256::new();
    hasher.update(format!("{}:{}", body.card_token, invoice_id));
    let request_hash = hex::encode(hasher.finalize());

    // 3. Check idempotency_keys table
    let idempotency_record = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<i32>,
            Option<serde_json::Value>,
        ),
    >(
        "SELECT status, request_hash, locked_until, response_code, response_body 
         FROM idempotency_keys WHERE business_id = $1 AND key = $2",
    )
    .bind(auth.business_id)
    .bind(&idem_key)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        error!("Database error: {:?}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(
                "internal_error",
                "Internal server error",
            )),
        )
    })?;

    if let Some(record) = idempotency_record {
        let (status, record_request_hash, locked_until, response_code, response_body) = record;
        if status == "completed" {
            if record_request_hash == request_hash {
                let code = StatusCode::from_u16(response_code.unwrap_or(200) as u16)
                    .unwrap_or(StatusCode::OK);
                let response_body: PaymentAttemptResponse = serde_json::from_value(
                    response_body.unwrap_or_default(),
                )
                .unwrap_or_else(|_| PaymentAttemptResponse {
                    id: Uuid::nil(),
                    invoice_id,
                    amount_cents: 0,
                    status: "unknown".to_string(),
                    failure_code: None,
                    failure_message: None,
                    psp_ref: None,
                    created_at: chrono::Utc::now(),
                });
                return Ok((code, Json(response_body)));
            } else {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(ErrorResponse::new(
                        "idempotency_key_mismatch",
                        "Idempotency key already used with different payload",
                    )),
                ));
            }
        } else if status == "processing"
            && locked_until.is_some()
            && locked_until.unwrap() > chrono::Utc::now()
        {
            return Err((
                StatusCode::CONFLICT,
                Json(ErrorResponse::new(
                    "concurrent_request",
                    "Request is already being processed",
                )),
            ));
        }
    }

    // Insert idempotency key
    let locked_until = chrono::Utc::now() + chrono::Duration::try_seconds(30).unwrap();
    sqlx::query(
        "INSERT INTO idempotency_keys (business_id, key, status, request_hash, locked_until) 
         VALUES ($1, $2, 'processing', $3, $4)
         ON CONFLICT (business_id, key) DO UPDATE 
         SET status = 'processing', request_hash = $3, locked_until = $4",
    )
    .bind(auth.business_id)
    .bind(&idem_key)
    .bind(&request_hash)
    .bind(locked_until)
    .execute(&state.db)
    .await
    .map_err(|e| {
        error!("Database error: {:?}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(
                "internal_error",
                "Internal server error",
            )),
        )
    })?;

    let mut tx = state.db.begin().await.map_err(|e| {
        error!("Database error: {:?}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(
                "internal_error",
                "Internal server error",
            )),
        )
    })?;

    let invoice = sqlx::query_as::<_, Invoice>(
        "SELECT * FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE",
    )
    .bind(invoice_id)
    .bind(auth.business_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        error!("Database error: {:?}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(
                "internal_error",
                "Internal server error",
            )),
        )
    })?;

    let invoice = match invoice {
        Some(inv) => inv,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::new("not_found", "Invoice not found")),
            ));
        }
    };

    if invoice.status != "issued" {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse::state_transition(&invoice.status, "paid")),
        ));
    }

    let payment_attempt_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO payment_attempts (id, invoice_id, card_token, amount_cents, status, idempotency_key) 
         VALUES ($1, $2, $3, $4, 'pending', $5)"
    )
    .bind(payment_attempt_id)
    .bind(invoice_id)
    .bind(&body.card_token)
    .bind(invoice.total_amount_cents)
    .bind(&idem_key)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        error!("Database error: {:?}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("internal_error", "Internal server error")),
        )
    })?;

    let psp_result = state
        .psp_client
        .post(format!("{}/charge", state.psp_base_url))
        .json(&PspChargeRequest {
            card_token: body.card_token.clone(),
            amount_cents: invoice.total_amount_cents,
            idempotency_key: format!("{}-{}", invoice_id, idem_key),
        })
        .send()
        .await;

    let update_idempotency_key =
        |pool: &sqlx::PgPool,
         business_id: Uuid,
         key: &str,
         status_code: u16,
         response_body: &PaymentAttemptResponse| {
            let pool = pool.clone();
            let key = key.to_string();
            let body = serde_json::to_value(response_body).unwrap();
            async move {
                let _ = sqlx::query(
                "UPDATE idempotency_keys SET status = 'completed', response_code = $1, response_body = $2 WHERE business_id = $3 AND key = $4"
            )
            .bind(status_code as i32)
            .bind(body)
            .bind(business_id)
            .bind(key)
            .execute(&pool)
            .await;
            }
        };

    match psp_result {
        Ok(res) if res.status().is_success() => {
            let psp_res = res.json::<PspChargeResponse>().await;
            if let Ok(charge_res) = psp_res {
                if charge_res.status == "succeeded" {
                    sqlx::query(
                        "UPDATE payment_attempts SET status = 'succeeded', psp_ref = $1, updated_at = now() WHERE id = $2"
                    )
                    .bind(&charge_res.psp_ref)
                    .bind(payment_attempt_id)
                    .execute(&mut *tx)
                    .await
                    .unwrap();

                    sqlx::query(
                        "UPDATE invoices SET status = 'paid', updated_at = now() WHERE id = $1",
                    )
                    .bind(invoice_id)
                    .execute(&mut *tx)
                    .await
                    .unwrap();

                    if let Err(e) = enqueue_webhook(
                        &mut tx,
                        auth.business_id,
                        "invoice.paid",
                        &serde_json::json!({
                            "invoice_id": invoice_id,
                            "payment_attempt_id": payment_attempt_id,
                        }),
                    )
                    .await
                    {
                        error!("Failed to enqueue webhook: {:?}", e);
                    }

                    tx.commit().await.unwrap();

                    let response = PaymentAttemptResponse {
                        id: payment_attempt_id,
                        invoice_id,
                        amount_cents: invoice.total_amount_cents,
                        status: "succeeded".to_string(),
                        failure_code: None,
                        failure_message: None,
                        psp_ref: charge_res.psp_ref,
                        created_at: chrono::Utc::now(),
                    };

                    update_idempotency_key(&state.db, auth.business_id, &idem_key, 200, &response)
                        .await;
                    return Ok((StatusCode::OK, Json(response)));
                } else if charge_res.status == "failed" {
                    sqlx::query(
                        "UPDATE payment_attempts SET status = 'failed', failure_code = $1, psp_ref = $2, updated_at = now() WHERE id = $3"
                    )
                    .bind(&charge_res.code)
                    .bind(&charge_res.psp_ref)
                    .bind(payment_attempt_id)
                    .execute(&mut *tx)
                    .await
                    .unwrap();

                    if let Err(e) = enqueue_webhook(
                        &mut tx,
                        auth.business_id,
                        "invoice.payment_failed",
                        &serde_json::json!({
                            "invoice_id": invoice_id,
                            "payment_attempt_id": payment_attempt_id,
                            "failure_code": charge_res.code,
                        }),
                    )
                    .await
                    {
                        error!("Failed to enqueue webhook: {:?}", e);
                    }

                    tx.commit().await.unwrap();

                    let response = PaymentAttemptResponse {
                        id: payment_attempt_id,
                        invoice_id,
                        amount_cents: invoice.total_amount_cents,
                        status: "failed".to_string(),
                        failure_code: charge_res.code,
                        failure_message: None,
                        psp_ref: charge_res.psp_ref,
                        created_at: chrono::Utc::now(),
                    };

                    update_idempotency_key(&state.db, auth.business_id, &idem_key, 422, &response)
                        .await;
                    return Ok((StatusCode::UNPROCESSABLE_ENTITY, Json(response)));
                }
            }
        }
        Ok(res) if res.status().is_server_error() => {
            // treat as indeterminate / timeout
        }
        Err(_) | Ok(_) => {
            // network error or timeout
        }
    }

    // Indeterminate / Timeout case
    // transaction drops here (rollback)
    drop(tx);

    let pa_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO payment_attempts (id, invoice_id, card_token, amount_cents, status, idempotency_key) VALUES ($1, $2, $3, $4, 'pending_resolution', $5)"
    )
    .bind(pa_id)
    .bind(invoice_id)
    .bind(&body.card_token)
    .bind(invoice.total_amount_cents)
    .bind(&idem_key)
    .execute(&state.db)
    .await
    .unwrap();

    let response = PaymentAttemptResponse {
        id: pa_id,
        invoice_id,
        amount_cents: invoice.total_amount_cents,
        status: "pending_resolution".to_string(),
        failure_code: None,
        failure_message: None,
        psp_ref: None,
        created_at: chrono::Utc::now(),
    };

    update_idempotency_key(&state.db, auth.business_id, &idem_key, 504, &response).await;

    let mut err = ErrorResponse::new(
        "psp_timeout",
        "Payment processing timeout, state is pending resolution",
    );
    err.payment_attempt_id = Some(pa_id);
    Err((StatusCode::GATEWAY_TIMEOUT, Json(err)))
}
