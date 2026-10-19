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
        ErrorResponse, Invoice, PayInvoiceRequest, PaymentAttempt, PaymentAttemptResponse,
        PspChargeRequest, PspChargeResponse,
    },
    reconciliation::{self, PaymentOutcome},
};

type ApiError = (StatusCode, Json<ErrorResponse>);

fn internal_error(e: impl std::fmt::Debug) -> ApiError {
    error!("Database error: {:?}", e);
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new(
            "internal_error",
            "Internal server error",
        )),
    )
}

pub async fn pay_invoice(
    State(state): State<AppState>,
    auth: AuthenticatedBusiness,
    Path(invoice_id): Path<Uuid>,
    headers: HeaderMap,
    Json(body): Json<PayInvoiceRequest>,
) -> Result<(StatusCode, Json<PaymentAttemptResponse>), ApiError> {
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
    .map_err(internal_error)?;

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
    .map_err(internal_error)?;

    // Claim the invoice: under its row lock, check it is payable and has no charge in
    // flight, then commit a `pending` attempt *before* calling the PSP. Once committed,
    // a concurrent request sees the attempt and backs off, and a crash during the PSP
    // call leaves a durable record for the reconciler instead of no trace.
    let mut tx = state.db.begin().await.map_err(internal_error)?;

    let invoice = sqlx::query_as::<_, Invoice>(
        "SELECT * FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE",
    )
    .bind(invoice_id)
    .bind(auth.business_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(internal_error)?;

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

    if let Some(unresolved_id) = reconciliation::unresolved_payment_attempt(&mut tx, invoice_id)
        .await
        .map_err(internal_error)?
    {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse::payment_pending(unresolved_id)),
        ));
    }

    let payment_attempt_id = Uuid::new_v4();
    let psp_idempotency_key = format!("{}-{}", invoice_id, idem_key);
    sqlx::query(
        "INSERT INTO payment_attempts (id, invoice_id, card_token, amount_cents, status, idempotency_key, psp_idempotency_key)
         VALUES ($1, $2, $3, $4, 'pending', $5, $6)",
    )
    .bind(payment_attempt_id)
    .bind(invoice_id)
    .bind(&body.card_token)
    .bind(invoice.total_amount_cents)
    .bind(&idem_key)
    .bind(&psp_idempotency_key)
    .execute(&mut *tx)
    .await
    .map_err(internal_error)?;

    tx.commit().await.map_err(internal_error)?;

    let psp_result = state
        .psp_client
        .post(format!("{}/charge", state.psp_base_url))
        .json(&PspChargeRequest {
            card_token: body.card_token.clone(),
            amount_cents: invoice.total_amount_cents,
            idempotency_key: psp_idempotency_key,
        })
        .send()
        .await;

    let outcome = match psp_result {
        Ok(res) if res.status().is_success() => match res.json::<PspChargeResponse>().await {
            Ok(charge) if charge.status == "succeeded" => Some(PaymentOutcome::Succeeded {
                psp_ref: charge.psp_ref,
            }),
            Ok(charge) if charge.status == "failed" => Some(PaymentOutcome::Failed {
                code: charge.code,
                psp_ref: charge.psp_ref,
            }),
            // Malformed or unexpected body: the charge may or may not have happened.
            _ => None,
        },
        // Timeout, network error or non-2xx: outcome unknown.
        _ => None,
    };

    let Some(outcome) = outcome else {
        return Err(leave_pending_resolution(
            &state,
            auth.business_id,
            &idem_key,
            &invoice,
            payment_attempt_id,
        )
        .await);
    };

    let server_error = |e: sqlx::Error| {
        // The attempt is still `pending` in the database, so the reconciler resolves
        // it once it is stale. The idempotency key is left `processing`, so a retry
        // with it gets `payment_pending` until then rather than a stored 500.
        error!(%payment_attempt_id, error = ?e, "Failed to record PSP outcome");
        let mut err = ErrorResponse::new("internal_error", "Internal server error");
        err.payment_attempt_id = Some(payment_attempt_id);
        (StatusCode::INTERNAL_SERVER_ERROR, Json(err))
    };

    // `false` means the reconciler resolved the attempt first; either way the row
    // now holds the final outcome, so the response is built from it.
    reconciliation::apply_outcome(&state.db, payment_attempt_id, &outcome)
        .await
        .map_err(server_error)?;

    let attempt =
        sqlx::query_as::<_, PaymentAttempt>("SELECT * FROM payment_attempts WHERE id = $1")
            .bind(payment_attempt_id)
            .fetch_one(&state.db)
            .await
            .map_err(server_error)?;

    let code = match attempt.status.as_str() {
        "succeeded" => StatusCode::OK,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    };
    let response = PaymentAttemptResponse::from(attempt);
    complete_idempotency_key(&state.db, auth.business_id, &idem_key, code, &response).await;
    Ok((code, Json(response)))
}

/// Hands an attempt with an unknown outcome to the reconciler and builds the 504.
async fn leave_pending_resolution(
    state: &AppState,
    business_id: Uuid,
    idem_key: &str,
    invoice: &Invoice,
    payment_attempt_id: Uuid,
) -> ApiError {
    if let Err(e) = sqlx::query(
        "UPDATE payment_attempts
         SET status = 'pending_resolution', next_resolution_at = now() + make_interval(secs => $1)
         WHERE id = $2 AND status = 'pending'",
    )
    .bind(reconciliation::FIRST_LOOKUP_DELAY_SECS)
    .bind(payment_attempt_id)
    .execute(&state.db)
    .await
    {
        // Still `pending`: the reconciler adopts it once it is stale.
        error!(%payment_attempt_id, error = ?e, "Failed to mark attempt pending_resolution");
    }

    let response = PaymentAttemptResponse {
        id: payment_attempt_id,
        invoice_id: invoice.id,
        amount_cents: invoice.total_amount_cents,
        status: "pending_resolution".to_string(),
        failure_code: None,
        failure_message: None,
        psp_ref: None,
        created_at: chrono::Utc::now(),
    };
    complete_idempotency_key(
        &state.db,
        business_id,
        idem_key,
        StatusCode::GATEWAY_TIMEOUT,
        &response,
    )
    .await;

    let mut err = ErrorResponse::new(
        "psp_timeout",
        "Payment processing timeout, state is pending resolution",
    );
    err.payment_attempt_id = Some(payment_attempt_id);
    (StatusCode::GATEWAY_TIMEOUT, Json(err))
}

async fn complete_idempotency_key(
    db: &sqlx::PgPool,
    business_id: Uuid,
    key: &str,
    status_code: StatusCode,
    response_body: &PaymentAttemptResponse,
) {
    let body = match serde_json::to_value(response_body) {
        Ok(body) => body,
        Err(e) => {
            error!(error = ?e, "Failed to serialize idempotent response");
            return;
        }
    };
    if let Err(e) = sqlx::query(
        "UPDATE idempotency_keys SET status = 'completed', response_code = $1, response_body = $2 WHERE business_id = $3 AND key = $4",
    )
    .bind(status_code.as_u16() as i32)
    .bind(body)
    .bind(business_id)
    .bind(key)
    .execute(db)
    .await
    {
        error!(error = ?e, "Failed to store idempotent response");
    }
}

pub async fn get_payment_attempt(
    State(state): State<AppState>,
    auth: AuthenticatedBusiness,
    Path(attempt_id): Path<Uuid>,
) -> Result<Json<PaymentAttemptResponse>, ApiError> {
    let attempt = sqlx::query_as::<_, PaymentAttempt>(
        "SELECT pa.* FROM payment_attempts pa
         JOIN invoices i ON i.id = pa.invoice_id
         WHERE pa.id = $1 AND i.business_id = $2",
    )
    .bind(attempt_id)
    .bind(auth.business_id)
    .fetch_optional(&state.db)
    .await
    .map_err(internal_error)?;

    match attempt {
        Some(attempt) => Ok(Json(attempt.into())),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("not_found", "Payment attempt not found")),
        )),
    }
}
