//! The three mandatory payment tests: concurrency, idempotency, PSP failure.
//!
//! Each test gets a fresh database from `#[sqlx::test]` (needs `DATABASE_URL`)
//! and a `wiremock` server standing in for the PSP, so we can count exactly
//! how many charge calls reached the processor.

mod common;

use std::time::Duration;

use common::TestApp;
use reqwest::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn mock_psp_success(psp: &MockServer, expected_calls: u64) {
    Mock::given(method("POST"))
        .and(path("/charge"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"status": "succeeded", "psp_ref": "psp_ref_123"}))
                // Long enough that the concurrent requests genuinely overlap.
                .set_delay(Duration::from_millis(200)),
        )
        .expect(expected_calls)
        .mount(psp)
        .await;
}

#[sqlx::test]
async fn concurrent_payments_charge_exactly_once(pool: PgPool) {
    const N: usize = 8;

    let psp = MockServer::start().await;
    mock_psp_success(&psp, 1).await;
    let app = TestApp::spawn(pool, &psp).await;
    let invoice_id = app.issued_invoice().await;

    // Distinct idempotency keys, so the idempotency layer can't dedupe them:
    // only the row lock on the invoice stands between these and a double charge.
    let keys: Vec<String> = (0..N).map(|i| format!("concurrent-{i}")).collect();
    let responses = futures::future::join_all(
        keys.iter()
            .map(|key| app.pay(&invoice_id, key, "tok_success")),
    )
    .await;

    let statuses: Vec<StatusCode> = responses.iter().map(|r| r.status()).collect();
    let ok = statuses.iter().filter(|s| **s == StatusCode::OK).count();
    let conflict = statuses
        .iter()
        .filter(|s| **s == StatusCode::CONFLICT)
        .count();
    assert_eq!(
        ok, 1,
        "exactly one payment should succeed, got {statuses:?}"
    );
    assert_eq!(
        conflict,
        N - 1,
        "all others should be rejected, got {statuses:?}"
    );

    assert_eq!(app.invoice_status(&invoice_id).await, "paid");
    assert_eq!(app.attempt_statuses(&invoice_id).await, vec!["succeeded"]);
    // No double charge: the PSP saw exactly one charge request.
    psp.verify().await;
}

#[sqlx::test]
async fn retried_payment_with_same_key_replays_response(pool: PgPool) {
    let psp = MockServer::start().await;
    mock_psp_success(&psp, 1).await;
    let app = TestApp::spawn(pool, &psp).await;
    let invoice_id = app.issued_invoice().await;

    let first = app.pay(&invoice_id, "idem-key-1", "tok_success").await;
    assert_eq!(first.status(), StatusCode::OK);
    let first: Value = first.json().await.unwrap();
    assert_eq!(first["status"], "succeeded");

    let retry = app.pay(&invoice_id, "idem-key-1", "tok_success").await;
    assert_eq!(retry.status(), StatusCode::OK);
    let retry: Value = retry.json().await.unwrap();
    assert_eq!(first, retry, "retry must replay the original response");

    // Same key, different body: rejected, not replayed and not charged.
    let mismatch = app
        .pay(&invoice_id, "idem-key-1", "tok_card_declined")
        .await;
    assert_eq!(mismatch.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let mismatch: Value = mismatch.json().await.unwrap();
    assert_eq!(mismatch["error"], "idempotency_key_mismatch");

    assert_eq!(app.attempt_statuses(&invoice_id).await, vec!["succeeded"]);
    // The retry and the mismatch never reached the PSP.
    psp.verify().await;
}

/// Shared assertions for PSP failures where the charge outcome is unknown.
async fn assert_payment_left_pending_resolution(
    app: &TestApp,
    invoice_id: &str,
    resp: reqwest::Response,
) {
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "psp_timeout");
    assert!(
        body["payment_attempt_id"].is_string(),
        "caller needs an attempt id to follow up: {body}"
    );

    // Invoice is untouched and still payable; the unknown outcome is recorded, not guessed.
    assert_eq!(app.invoice_status(invoice_id).await, "issued");
    assert_eq!(
        app.attempt_statuses(invoice_id).await,
        vec!["pending_resolution"]
    );
}

#[sqlx::test]
async fn psp_timeout_leaves_invoice_issued(pool: PgPool) {
    let psp = MockServer::start().await;
    // Mirrors tok_timeout: the PSP answers far later than our client will wait.
    Mock::given(method("POST"))
        .and(path("/charge"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"status": "succeeded", "psp_ref": "late"}))
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&psp)
        .await;
    let app = TestApp::spawn(pool, &psp).await;
    let invoice_id = app.issued_invoice().await;

    let started = std::time::Instant::now();
    let resp = app.pay(&invoice_id, "timeout-key", "tok_timeout").await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "endpoint must not hang for the PSP's full 30s"
    );

    assert_payment_left_pending_resolution(&app, &invoice_id, resp).await;
}

#[sqlx::test]
async fn psp_network_error_leaves_invoice_issued(pool: PgPool) {
    let psp = MockServer::start().await;
    // Mirrors tok_network_error.
    Mock::given(method("POST"))
        .and(path("/charge"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "internal_error"})))
        .mount(&psp)
        .await;
    let app = TestApp::spawn(pool, &psp).await;
    let invoice_id = app.issued_invoice().await;

    let resp = app
        .pay(&invoice_id, "network-error-key", "tok_network_error")
        .await;

    assert_payment_left_pending_resolution(&app, &invoice_id, resp).await;
}
