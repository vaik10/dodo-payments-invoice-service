//! Payment attempts with an unknown outcome: blocking further charges until they
//! are resolved, and the reconciler that resolves them by asking the PSP.

mod common;

use common::TestApp;
use reqwest::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The PSP fails `/charge` with a 500, so the outcome is unknown.
async fn mock_charge_error(psp: &MockServer, expected_calls: u64) {
    Mock::given(method("POST"))
        .and(path("/charge"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "internal_error"})))
        .up_to_n_times(expected_calls)
        .expect(expected_calls)
        .mount(psp)
        .await;
}

async fn mock_charge_lookup(psp: &MockServer, psp_key: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(format!("/charges/{psp_key}")))
        .respond_with(response)
        .mount(psp)
        .await;
}

/// Pays with a PSP that errors and returns the `pending_resolution` attempt id.
async fn pay_with_unknown_outcome(app: &TestApp, invoice_id: &str, key: &str) -> String {
    let resp = app.pay(invoice_id, key, "tok_network_error").await;
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    let body: Value = resp.json().await.unwrap();
    body["payment_attempt_id"].as_str().unwrap().to_string()
}

#[sqlx::test]
async fn unresolved_attempt_blocks_new_payments_and_transitions(pool: PgPool) {
    let psp = MockServer::start().await;
    // The second payment must never reach the PSP.
    mock_charge_error(&psp, 1).await;
    let app = TestApp::spawn(pool, &psp).await;
    let invoice_id = app.issued_invoice().await;

    let attempt_id = pay_with_unknown_outcome(&app, &invoice_id, "key-1").await;

    // A new idempotency key used to mean a second charge. Now it is refused.
    let retry = app.pay(&invoice_id, "key-2", "tok_success").await;
    assert_eq!(retry.status(), StatusCode::CONFLICT);
    let retry: Value = retry.json().await.unwrap();
    assert_eq!(retry["error"], "payment_pending");
    assert_eq!(retry["payment_attempt_id"], attempt_id.as_str());

    // The charge may still succeed, so the invoice can't leave `issued` either.
    for action in ["void", "mark-uncollectible"] {
        let resp = app
            .post(&format!("/invoices/{invoice_id}/{action}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT, "{action}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"], "payment_pending", "{action}");
    }

    assert_eq!(app.invoice_status(&invoice_id).await, "issued");
    assert_eq!(
        app.attempt_statuses(&invoice_id).await,
        vec!["pending_resolution"]
    );
    psp.verify().await;
}

#[sqlx::test]
async fn reconciler_marks_invoice_paid_when_psp_charged(pool: PgPool) {
    let psp = MockServer::start().await;
    mock_charge_error(&psp, 1).await;
    let app = TestApp::spawn(pool, &psp).await;
    app.register_webhook_endpoint().await;
    let invoice_id = app.issued_invoice().await;
    mock_charge_lookup(
        &psp,
        &format!("{invoice_id}-key-1"),
        ResponseTemplate::new(200)
            .set_body_json(json!({"status": "succeeded", "psp_ref": "psp_late_ref"})),
    )
    .await;

    let attempt_id = pay_with_unknown_outcome(&app, &invoice_id, "key-1").await;

    // The first lookup waits, giving the PSP time to finish processing.
    assert_eq!(app.reconcile().await, 0);
    assert_eq!(app.invoice_status(&invoice_id).await, "issued");

    app.make_unresolved_attempts_due().await;
    assert_eq!(app.reconcile().await, 1);

    assert_eq!(app.invoice_status(&invoice_id).await, "paid");
    let attempt: Value = app
        .get(&format!("/payment-attempts/{attempt_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(attempt["status"], "succeeded");
    assert_eq!(attempt["psp_ref"], "psp_late_ref");
    assert_eq!(
        app.webhook_events().await,
        vec!["invoice.created", "invoice.issued", "invoice.paid"]
    );

    // Resolved attempts are not looked up again.
    app.make_unresolved_attempts_due().await;
    assert_eq!(app.reconcile().await, 0);
}

#[sqlx::test]
async fn reconciler_fails_attempt_when_psp_has_no_charge(pool: PgPool) {
    let psp = MockServer::start().await;
    mock_charge_error(&psp, 1).await;
    let app = TestApp::spawn(pool, &psp).await;
    app.register_webhook_endpoint().await;
    let invoice_id = app.issued_invoice().await;
    mock_charge_lookup(
        &psp,
        &format!("{invoice_id}-key-1"),
        ResponseTemplate::new(404).set_body_json(json!({"error": "charge_not_found"})),
    )
    .await;

    pay_with_unknown_outcome(&app, &invoice_id, "key-1").await;
    app.make_unresolved_attempts_due().await;
    assert_eq!(app.reconcile().await, 1);

    let (status, failure_code): (String, Option<String>) = sqlx::query_as(
        "SELECT status, failure_code FROM payment_attempts WHERE invoice_id = $1::uuid",
    )
    .bind(&invoice_id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(failure_code.as_deref(), Some("psp_no_charge"));
    assert_eq!(app.invoice_status(&invoice_id).await, "issued");
    assert_eq!(
        app.webhook_events().await.last().map(String::as_str),
        Some("invoice.payment_failed")
    );

    // Nothing was charged, so the invoice is payable again.
    Mock::given(method("POST"))
        .and(path("/charge"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"status": "succeeded", "psp_ref": "psp_ref_2"})),
        )
        .expect(1)
        .mount(&psp)
        .await;
    let resp = app.pay(&invoice_id, "key-2", "tok_success").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(app.invoice_status(&invoice_id).await, "paid");
}

#[sqlx::test]
async fn reconciler_backs_off_while_psp_cannot_answer(pool: PgPool) {
    let psp = MockServer::start().await;
    mock_charge_error(&psp, 1).await;
    let app = TestApp::spawn(pool, &psp).await;
    let invoice_id = app.issued_invoice().await;
    mock_charge_lookup(
        &psp,
        &format!("{invoice_id}-key-1"),
        ResponseTemplate::new(200).set_body_json(json!({"status": "processing"})),
    )
    .await;

    pay_with_unknown_outcome(&app, &invoice_id, "key-1").await;
    app.make_unresolved_attempts_due().await;
    assert_eq!(app.reconcile().await, 1);

    let (status, lookups, waits_long_enough): (String, i16, bool) = sqlx::query_as(
        "SELECT status, resolution_attempts, next_resolution_at > now() + interval '20 seconds'
         FROM payment_attempts WHERE invoice_id = $1::uuid",
    )
    .bind(&invoice_id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(status, "pending_resolution");
    assert_eq!(lookups, 1);
    assert!(waits_long_enough, "next lookup should be backed off");
    assert_eq!(app.invoice_status(&invoice_id).await, "issued");

    // Not due yet, so the next pass leaves it alone.
    assert_eq!(app.reconcile().await, 0);
}

#[sqlx::test]
async fn reconciler_adopts_attempt_abandoned_by_crash(pool: PgPool) {
    let psp = MockServer::start().await;
    let app = TestApp::spawn(pool, &psp).await;
    let crashed_invoice = app.issued_invoice().await;
    let in_flight_invoice = app.issued_invoice().await;

    // A `pending` attempt is committed before the PSP call. One that is minutes
    // old belongs to a request that died; a fresh one may still be waiting on the PSP.
    for (invoice_id, age) in [
        (&crashed_invoice, "5 minutes"),
        (&in_flight_invoice, "0 seconds"),
    ] {
        sqlx::query(
            "INSERT INTO payment_attempts
                 (invoice_id, card_token, amount_cents, status, idempotency_key, psp_idempotency_key, created_at)
             VALUES ($1::uuid, 'tok_success', 5000, 'pending', 'k', $1 || '-k', now() - $2::interval)",
        )
        .bind(invoice_id)
        .bind(age)
        .execute(&app.db)
        .await
        .unwrap();
    }
    mock_charge_lookup(
        &psp,
        &format!("{crashed_invoice}-k"),
        ResponseTemplate::new(200)
            .set_body_json(json!({"status": "succeeded", "psp_ref": "psp_ref_crash"})),
    )
    .await;

    assert_eq!(app.reconcile().await, 1);

    assert_eq!(app.invoice_status(&crashed_invoice).await, "paid");
    assert_eq!(
        app.attempt_statuses(&crashed_invoice).await,
        vec!["succeeded"]
    );
    assert_eq!(app.invoice_status(&in_flight_invoice).await, "issued");
    assert_eq!(
        app.attempt_statuses(&in_flight_invoice).await,
        vec!["pending"]
    );
}
