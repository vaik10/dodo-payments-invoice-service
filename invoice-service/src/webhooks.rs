use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::PgPool;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::models::WebhookDelivery;

type HmacSha256 = Hmac<Sha256>;

/// Enqueue a webhook delivery for all active endpoints of a business.
/// Must be called within a transaction to ensure atomicity with the state change.
pub async fn enqueue_webhook(
    conn: &mut sqlx::PgConnection,
    business_id: Uuid,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    // Find all active webhook endpoints for this business
    let endpoints: Vec<(Uuid,)> =
        sqlx::query_as("SELECT id FROM webhook_endpoints WHERE business_id = $1 AND active = true")
            .bind(business_id)
            .fetch_all(&mut *conn)
            .await?;

    for (endpoint_id,) in endpoints {
        sqlx::query(
            "INSERT INTO webhook_deliveries (id, webhook_endpoint_id, event_type, payload, status, attempt_count, next_retry_at)
             VALUES (gen_random_uuid(), $1, $2, $3, 'pending', 0, now())"
        )
        .bind(endpoint_id)
        .bind(event_type)
        .bind(payload)
        .execute(&mut *conn)
        .await?;
    }

    Ok(())
}

/// Compute HMAC-SHA256 signature for a webhook payload.
/// Format: `t={timestamp},v1={hex_signature}`
/// Signed content: `{timestamp}.{payload_json}`
pub fn compute_signature(signing_secret: &str, timestamp: i64, payload: &str) -> String {
    let signed_content = format!("{}.{}", timestamp, payload);
    let mut mac = HmacSha256::new_from_slice(signing_secret.as_bytes())
        .expect("HMAC can take key of any size");
    mac.update(signed_content.as_bytes());
    let result = mac.finalize();
    let signature = hex::encode(result.into_bytes());
    format!("t={},v1={}", timestamp, signature)
}

/// Background worker that polls for pending webhook deliveries and sends them.
/// Retry intervals: [0, 30s, 2min, 15min, 1h, 4h]
const RETRY_INTERVALS_SECS: [i64; 6] = [0, 30, 120, 900, 3600, 14400];
const MAX_ATTEMPTS: i16 = 6;

pub async fn webhook_delivery_worker(db: PgPool) {
    info!("Webhook delivery worker started");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("Failed to build HTTP client");

    loop {
        match process_pending_webhooks(&db, &client).await {
            Ok(count) => {
                if count > 0 {
                    info!(count, "Processed webhook deliveries");
                }
            }
            Err(e) => {
                error!(error = %e, "Error processing webhook deliveries");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn process_pending_webhooks(
    db: &PgPool,
    client: &reqwest::Client,
) -> Result<usize, sqlx::Error> {
    // Fetch pending deliveries that are due for retry
    let deliveries: Vec<WebhookDelivery> = sqlx::query_as(
        "SELECT wd.* FROM webhook_deliveries wd
         WHERE wd.status = 'pending'
           AND (wd.next_retry_at IS NULL OR wd.next_retry_at <= now())
         ORDER BY wd.created_at ASC
         LIMIT 50
         FOR UPDATE SKIP LOCKED",
    )
    .fetch_all(db)
    .await?;

    let count = deliveries.len();

    for delivery in deliveries {
        deliver_webhook(db, client, delivery).await;
    }

    Ok(count)
}

async fn deliver_webhook(db: &PgPool, client: &reqwest::Client, delivery: WebhookDelivery) {
    // Fetch the endpoint details
    let endpoint: Option<(String, String)> = sqlx::query_as(
        "SELECT url, signing_secret FROM webhook_endpoints WHERE id = $1 AND active = true",
    )
    .bind(delivery.webhook_endpoint_id)
    .fetch_optional(db)
    .await
    .unwrap_or(None);

    let Some((url, signing_secret)) = endpoint else {
        // Endpoint no longer active, mark delivery as failed
        let _ = sqlx::query(
            "UPDATE webhook_deliveries SET status = 'failed', last_error = 'Endpoint inactive' WHERE id = $1"
        )
        .bind(delivery.id)
        .execute(db)
        .await;
        return;
    };

    let timestamp = Utc::now().timestamp();
    let payload_json = serde_json::to_string(&delivery.payload).unwrap_or_default();
    let signature = compute_signature(&signing_secret, timestamp, &payload_json);

    let result = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Dodo-Signature", &signature)
        .header("Dodo-Webhook-Id", delivery.id.to_string())
        .header("Dodo-Webhook-Timestamp", timestamp.to_string())
        .body(payload_json)
        .send()
        .await;

    let new_attempt_count = delivery.attempt_count + 1;

    match result {
        Ok(response) if response.status().is_success() => {
            info!(
                delivery_id = %delivery.id,
                endpoint_url = %url,
                "Webhook delivered successfully"
            );
            let _ = sqlx::query(
                "UPDATE webhook_deliveries SET status = 'delivered', attempt_count = $1, delivered_at = now() WHERE id = $2"
            )
            .bind(new_attempt_count)
            .bind(delivery.id)
            .execute(db)
            .await;
        }
        Ok(response) => {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            warn!(
                delivery_id = %delivery.id,
                endpoint_url = %url,
                status = %status,
                "Webhook delivery failed with non-2xx status"
            );
            schedule_retry_or_fail(
                db,
                &delivery,
                new_attempt_count,
                &format!("HTTP {}: {}", status, error_body),
            )
            .await;
        }
        Err(e) => {
            warn!(
                delivery_id = %delivery.id,
                endpoint_url = %url,
                error = %e,
                "Webhook delivery failed with network error"
            );
            schedule_retry_or_fail(db, &delivery, new_attempt_count, &e.to_string()).await;
        }
    }
}

async fn schedule_retry_or_fail(
    db: &PgPool,
    delivery: &WebhookDelivery,
    new_attempt_count: i16,
    error_msg: &str,
) {
    if new_attempt_count >= MAX_ATTEMPTS {
        warn!(
            delivery_id = %delivery.id,
            "Webhook delivery exhausted all retries, marking as failed"
        );
        let _ = sqlx::query(
            "UPDATE webhook_deliveries SET status = 'failed', attempt_count = $1, last_error = $2 WHERE id = $3"
        )
        .bind(new_attempt_count)
        .bind(error_msg)
        .bind(delivery.id)
        .execute(db)
        .await;
    } else {
        let interval_secs = RETRY_INTERVALS_SECS
            .get(new_attempt_count as usize)
            .copied()
            .unwrap_or(14400);

        info!(
            delivery_id = %delivery.id,
            next_retry_secs = interval_secs,
            attempt = new_attempt_count,
            "Scheduling webhook retry"
        );
        let _ = sqlx::query(
            "UPDATE webhook_deliveries SET attempt_count = $1, last_error = $2, next_retry_at = now() + make_interval(secs => $3::float8) WHERE id = $4"
        )
        .bind(new_attempt_count)
        .bind(error_msg)
        .bind(interval_secs as f64)
        .bind(delivery.id)
        .execute(db)
        .await;
    }
}
