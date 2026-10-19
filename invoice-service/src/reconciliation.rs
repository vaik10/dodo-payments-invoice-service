//! Resolution of payment attempts whose outcome is unknown.
//!
//! `/pay` commits a `pending` attempt before calling the PSP. If the PSP gives a
//! definite answer, the handler applies it with [`apply_outcome`]. If it times out
//! or errors, the attempt becomes `pending_resolution`, and the worker here asks the
//! PSP what happened (`GET /charges/{idempotency_key}`) until it gets an answer. A
//! `pending` attempt that is older than any request could be means the service died
//! mid-payment; the worker adopts it the same way.
//!
//! While an invoice has an unresolved attempt, new payments and state transitions
//! out of `issued` are refused, so a retry can't charge the card a second time.

use std::time::Duration;

use sqlx::{PgConnection, PgPool};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    models::{PaymentAttempt, PspChargeLookupResponse},
    webhooks::enqueue_webhook,
};

/// Delay before the first PSP lookup for a timed-out attempt. The PSP may still be
/// processing the charge; looking it up immediately would usually just say so.
pub const FIRST_LOOKUP_DELAY_SECS: f64 = 30.0;

/// Delays between later lookups while the PSP can't give an answer.
const LOOKUP_BACKOFF_SECS: [f64; 5] = [30.0, 60.0, 300.0, 900.0, 3600.0];

/// A `pending` attempt older than this was abandoned by a crashed request. It must
/// comfortably exceed the PSP client timeout (5s), or the worker would race the
/// handler that is still waiting on the PSP.
const STALE_PENDING_SECS: f64 = 120.0;

/// How long a claimed attempt is hidden from other workers while it is looked up.
const CLAIM_LEASE_SECS: f64 = 60.0;

const BATCH_SIZE: i64 = 20;
const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// After this many inconclusive lookups the attempt is logged at error level for
/// manual review. It keeps being retried, and the invoice stays blocked.
const ALERT_AFTER_LOOKUPS: i16 = 5;

/// A definite charge result from the PSP.
#[derive(Debug, Clone)]
pub enum PaymentOutcome {
    Succeeded {
        psp_ref: Option<String>,
    },
    Failed {
        code: Option<String>,
        psp_ref: Option<String>,
    },
}

/// Returns the id of an unresolved (`pending` or `pending_resolution`) attempt on the
/// invoice, if any. Callers must hold the invoice row lock for the answer to stay true.
pub async fn unresolved_payment_attempt(
    conn: &mut PgConnection,
    invoice_id: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM payment_attempts
         WHERE invoice_id = $1 AND status IN ('pending', 'pending_resolution')
         ORDER BY created_at
         LIMIT 1",
    )
    .bind(invoice_id)
    .fetch_optional(conn)
    .await
}

/// Records a definite outcome on an unresolved attempt. In the same transaction it
/// marks the invoice `paid` on success and enqueues the matching webhook.
///
/// Returns `false` if the attempt had already been resolved, in which case nothing
/// is changed.
pub async fn apply_outcome(
    db: &PgPool,
    attempt_id: Uuid,
    outcome: &PaymentOutcome,
) -> Result<bool, sqlx::Error> {
    let mut tx = db.begin().await?;

    // Lock the invoice before the attempt, the same order /pay uses.
    let (invoice_id, business_id, invoice_status): (Uuid, Uuid, String) = sqlx::query_as(
        "SELECT i.id, i.business_id, i.status FROM invoices i
         JOIN payment_attempts pa ON pa.invoice_id = i.id
         WHERE pa.id = $1
         FOR UPDATE OF i",
    )
    .bind(attempt_id)
    .fetch_one(&mut *tx)
    .await?;

    let updated = match outcome {
        PaymentOutcome::Succeeded { psp_ref } => {
            sqlx::query(
                "UPDATE payment_attempts
                 SET status = 'succeeded', psp_ref = $1, resolved_at = now(), next_resolution_at = NULL
                 WHERE id = $2 AND status IN ('pending', 'pending_resolution')",
            )
            .bind(psp_ref)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await?
        }
        PaymentOutcome::Failed { code, psp_ref } => {
            sqlx::query(
                "UPDATE payment_attempts
                 SET status = 'failed', failure_code = $1, psp_ref = $2, resolved_at = now(), next_resolution_at = NULL
                 WHERE id = $3 AND status IN ('pending', 'pending_resolution')",
            )
            .bind(code)
            .bind(psp_ref)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await?
        }
    };

    if updated.rows_affected() == 0 {
        return Ok(false);
    }

    match outcome {
        PaymentOutcome::Succeeded { .. } => {
            if invoice_status == "issued" {
                sqlx::query(
                    "UPDATE invoices SET status = 'paid', updated_at = now() WHERE id = $1",
                )
                .bind(invoice_id)
                .execute(&mut *tx)
                .await?;
                enqueue_webhook(
                    &mut tx,
                    business_id,
                    "invoice.paid",
                    &serde_json::json!({
                        "invoice_id": invoice_id,
                        "payment_attempt_id": attempt_id,
                    }),
                )
                .await?;
            } else {
                // Transitions out of `issued` are blocked while an attempt is
                // unresolved, so this indicates a bug. The money was taken, so the
                // attempt is still recorded as succeeded.
                error!(
                    %attempt_id,
                    %invoice_id,
                    invoice_status,
                    "PSP charge succeeded on an invoice that is no longer payable; refund required"
                );
            }
        }
        PaymentOutcome::Failed { code, .. } => {
            enqueue_webhook(
                &mut tx,
                business_id,
                "invoice.payment_failed",
                &serde_json::json!({
                    "invoice_id": invoice_id,
                    "payment_attempt_id": attempt_id,
                    "failure_code": code,
                }),
            )
            .await?;
        }
    }

    tx.commit().await?;
    Ok(true)
}

pub async fn reconciliation_worker(db: PgPool, client: reqwest::Client, psp_base_url: String) {
    info!("Payment reconciliation worker started");
    loop {
        match reconcile_due_attempts(&db, &client, &psp_base_url).await {
            Ok(0) => {}
            Ok(count) => info!(count, "Reconciled payment attempts"),
            Err(e) => error!(error = %e, "Error reconciling payment attempts"),
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Runs one reconciliation pass and returns how many attempts it looked at.
pub async fn reconcile_due_attempts(
    db: &PgPool,
    client: &reqwest::Client,
    psp_base_url: &str,
) -> Result<usize, sqlx::Error> {
    // Claiming with UPDATE ... RETURNING makes the lease visible to other workers
    // as soon as this statement commits, so replicas don't look up the same rows.
    let claimed: Vec<PaymentAttempt> = sqlx::query_as(
        "UPDATE payment_attempts
         SET status = 'pending_resolution', next_resolution_at = now() + make_interval(secs => $1)
         WHERE id IN (
             SELECT id FROM payment_attempts
             WHERE (status = 'pending_resolution'
                    AND (next_resolution_at IS NULL OR next_resolution_at <= now()))
                OR (status = 'pending'
                    AND created_at <= now() - make_interval(secs => $2))
             ORDER BY created_at
             LIMIT $3
             FOR UPDATE SKIP LOCKED
         )
         RETURNING *",
    )
    .bind(CLAIM_LEASE_SECS)
    .bind(STALE_PENDING_SECS)
    .bind(BATCH_SIZE)
    .fetch_all(db)
    .await?;

    let count = claimed.len();
    for attempt in claimed {
        if let Err(e) = reconcile_attempt(db, client, psp_base_url, &attempt).await {
            error!(attempt_id = %attempt.id, error = %e, "Failed to reconcile payment attempt");
        }
    }
    Ok(count)
}

enum ChargeLookup {
    Resolved(PaymentOutcome),
    /// The PSP has no charge under this key, so the card was never charged.
    NotFound,
    /// Still processing, PSP unavailable, or an unexpected response.
    Inconclusive(String),
}

async fn reconcile_attempt(
    db: &PgPool,
    client: &reqwest::Client,
    psp_base_url: &str,
    attempt: &PaymentAttempt,
) -> Result<(), sqlx::Error> {
    let lookup = match &attempt.psp_idempotency_key {
        Some(key) => lookup_charge(client, psp_base_url, key).await,
        None => ChargeLookup::Inconclusive("attempt has no PSP idempotency key".to_string()),
    };

    let outcome = match lookup {
        ChargeLookup::Resolved(outcome) => outcome,
        ChargeLookup::NotFound => PaymentOutcome::Failed {
            code: Some("psp_no_charge".to_string()),
            psp_ref: None,
        },
        ChargeLookup::Inconclusive(reason) => {
            return schedule_next_lookup(db, attempt, &reason).await;
        }
    };

    if apply_outcome(db, attempt.id, &outcome).await? {
        info!(attempt_id = %attempt.id, invoice_id = %attempt.invoice_id, ?outcome, "Resolved payment attempt");
    }
    Ok(())
}

async fn lookup_charge(client: &reqwest::Client, psp_base_url: &str, key: &str) -> ChargeLookup {
    let mut url = match reqwest::Url::parse(psp_base_url) {
        Ok(url) => url,
        Err(e) => return ChargeLookup::Inconclusive(format!("invalid PSP base URL: {e}")),
    };
    // path_segments_mut percent-encodes the key; idempotency keys are client-supplied.
    match url.path_segments_mut() {
        Ok(mut segments) => {
            segments.pop_if_empty().push("charges").push(key);
        }
        Err(()) => return ChargeLookup::Inconclusive("invalid PSP base URL".to_string()),
    }

    let response = match client.get(url).send().await {
        Ok(response) => response,
        Err(e) => return ChargeLookup::Inconclusive(format!("PSP lookup failed: {e}")),
    };

    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return ChargeLookup::NotFound;
    }
    if !status.is_success() {
        return ChargeLookup::Inconclusive(format!("PSP lookup returned HTTP {status}"));
    }

    match response.json::<PspChargeLookupResponse>().await {
        Ok(charge) => match charge.status.as_str() {
            "succeeded" => ChargeLookup::Resolved(PaymentOutcome::Succeeded {
                psp_ref: charge.psp_ref,
            }),
            "failed" => ChargeLookup::Resolved(PaymentOutcome::Failed {
                code: charge.code,
                psp_ref: charge.psp_ref,
            }),
            other => ChargeLookup::Inconclusive(format!("PSP reports charge as '{other}'")),
        },
        Err(e) => ChargeLookup::Inconclusive(format!("malformed PSP lookup response: {e}")),
    }
}

async fn schedule_next_lookup(
    db: &PgPool,
    attempt: &PaymentAttempt,
    reason: &str,
) -> Result<(), sqlx::Error> {
    let lookups = attempt.resolution_attempts.saturating_add(1);
    let delay = LOOKUP_BACKOFF_SECS
        .get(attempt.resolution_attempts as usize)
        .copied()
        .unwrap_or(LOOKUP_BACKOFF_SECS[LOOKUP_BACKOFF_SECS.len() - 1]);

    if lookups >= ALERT_AFTER_LOOKUPS {
        error!(attempt_id = %attempt.id, invoice_id = %attempt.invoice_id, lookups, reason, "Payment attempt still unresolved; needs manual review");
    } else {
        warn!(attempt_id = %attempt.id, lookups, next_lookup_secs = delay, reason, "Payment attempt still unresolved");
    }

    sqlx::query(
        "UPDATE payment_attempts
         SET resolution_attempts = $1, next_resolution_at = now() + make_interval(secs => $2)
         WHERE id = $3 AND status = 'pending_resolution'",
    )
    .bind(lookups)
    .bind(delay)
    .bind(attempt.id)
    .execute(db)
    .await?;
    Ok(())
}
