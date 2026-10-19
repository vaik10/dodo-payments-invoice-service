//! Test harness shared by the integration tests: a real server on a random port,
//! backed by the `#[sqlx::test]` database, with a `wiremock` PSP.

#![allow(dead_code)]

use std::time::Duration;

use invoice_service::{AppState, build_app, reconciliation};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wiremock::MockServer;

/// Client-side PSP timeout used in tests. Production uses 5s (see `run()`).
pub const TEST_PSP_TIMEOUT: Duration = Duration::from_secs(1);

pub struct TestApp {
    pub base_url: String,
    pub client: Client,
    pub api_key: String,
    pub db: PgPool,
    psp_client: Client,
    psp_base_url: String,
}

impl TestApp {
    pub async fn spawn(db: PgPool, psp: &MockServer) -> Self {
        let psp_client = Client::builder().timeout(TEST_PSP_TIMEOUT).build().unwrap();
        let state = AppState {
            db: db.clone(),
            psp_client: psp_client.clone(),
            psp_base_url: psp.uri(),
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, build_app(state)).await.unwrap();
        });

        let client = Client::new();

        let business: Value = client
            .post(format!("{base_url}/businesses"))
            .json(&json!({"name": "Test Business"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let business_id = business["id"].as_str().unwrap();

        let key: Value = client
            .post(format!("{base_url}/businesses/{business_id}/api-keys"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let api_key = key["raw_key"].as_str().unwrap().to_string();

        Self {
            base_url,
            client,
            api_key,
            db,
            psp_client,
            psp_base_url: psp.uri(),
        }
    }

    pub fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.api_key)
    }

    pub fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .get(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.api_key)
    }

    /// Creates a customer and a 2 x 2500 = 5000 cent invoice, then finalizes it to `issued`.
    pub async fn issued_invoice(&self) -> String {
        let customer: Value = self
            .post("/customers")
            .json(&json!({"name": "John Doe", "email": format!("{}@example.com", uuid::Uuid::new_v4())}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let invoice: Value = self
            .post("/invoices")
            .json(&json!({
                "customer_id": customer["id"],
                "due_date": "2030-12-31",
                "line_items": [
                    {"description": "Test Item", "quantity": 2, "unit_amount_cents": 2500}
                ]
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let invoice_id = invoice["id"].as_str().unwrap().to_string();

        let resp = self
            .post(&format!("/invoices/{invoice_id}/finalize"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        invoice_id
    }

    pub async fn pay(
        &self,
        invoice_id: &str,
        idempotency_key: &str,
        card_token: &str,
    ) -> reqwest::Response {
        self.post(&format!("/invoices/{invoice_id}/pay"))
            .header("Idempotency-Key", idempotency_key)
            .json(&json!({"card_token": card_token}))
            .send()
            .await
            .unwrap()
    }

    pub async fn invoice_status(&self, invoice_id: &str) -> String {
        let invoice: Value = self
            .get(&format!("/invoices/{invoice_id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        invoice["status"].as_str().unwrap().to_string()
    }

    /// Payment attempt statuses recorded for an invoice, oldest first.
    pub async fn attempt_statuses(&self, invoice_id: &str) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT status FROM payment_attempts WHERE invoice_id = $1::uuid ORDER BY created_at",
        )
        .bind(invoice_id)
        .fetch_all(&self.db)
        .await
        .unwrap()
    }

    /// Registers a webhook endpoint so state changes write outbox rows. No webhook
    /// worker runs in tests, so the rows stay `pending` and can be inspected.
    pub async fn register_webhook_endpoint(&self) {
        let resp = self
            .post("/webhook-endpoints")
            .json(&json!({"url": "http://127.0.0.1:9/hook"}))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{}", resp.status());
    }

    /// Webhook event types enqueued so far, oldest first.
    pub async fn webhook_events(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT event_type FROM webhook_deliveries ORDER BY created_at")
            .fetch_all(&self.db)
            .await
            .unwrap()
    }

    /// Skips the first-lookup delay so the reconciler picks attempts up now.
    pub async fn make_unresolved_attempts_due(&self) {
        sqlx::query(
            "UPDATE payment_attempts SET next_resolution_at = now() WHERE status = 'pending_resolution'",
        )
        .execute(&self.db)
        .await
        .unwrap();
    }

    /// Runs one reconciliation pass and returns how many attempts it looked at.
    pub async fn reconcile(&self) -> usize {
        reconciliation::reconcile_due_attempts(&self.db, &self.psp_client, &self.psp_base_url)
            .await
            .unwrap()
    }
}
