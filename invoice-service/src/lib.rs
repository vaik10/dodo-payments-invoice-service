// Handlers return `(StatusCode, Json<ErrorResponse>)` as their error type by design.
#![allow(clippy::result_large_err)]

pub mod auth;
pub mod models;
pub mod reconciliation;
pub mod routes;
pub mod webhooks;

use axum::{
    Router,
    routing::{get, post},
};
use sqlx::postgres::PgPoolOptions;
use std::net::SocketAddr;
use std::time::Duration;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub db: sqlx::PgPool,
    pub psp_client: reqwest::Client,
    pub psp_base_url: String,
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://dodo:dodo_secret@localhost:5432/dodo_payments".to_string());
    let psp_base_url =
        std::env::var("PSP_BASE_URL").unwrap_or_else(|_| "http://localhost:9090".to_string());
    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse::<u16>()?;

    let db = PgPoolOptions::new()
        .max_connections(20)
        .connect(&database_url)
        .await?;

    // Run migrations
    tracing::info!("Running database migrations...");
    sqlx::migrate!("./migrations").run(&db).await?;
    tracing::info!("Migrations complete.");

    let psp_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    // Spawn background workers
    tokio::spawn(webhooks::webhook_delivery_worker(db.clone()));
    tokio::spawn(reconciliation::reconciliation_worker(
        db.clone(),
        psp_client.clone(),
        psp_base_url.clone(),
    ));

    let state = AppState {
        db: db.clone(),
        psp_client,
        psp_base_url,
    };

    let app = build_app(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("Listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { "OK" }))
        .route("/businesses", post(routes::businesses::create_business))
        .route(
            "/businesses/{business_id}/api-keys",
            post(routes::businesses::create_api_key),
        )
        .route(
            "/webhook-endpoints",
            post(routes::webhook_endpoints::create_webhook_endpoint),
        )
        .route(
            "/customers",
            get(routes::customers::list_customers).post(routes::customers::create_customer),
        )
        .route("/customers/{id}", get(routes::customers::get_customer))
        .route(
            "/invoices",
            get(routes::invoices::list_invoices).post(routes::invoices::create_invoice),
        )
        .route("/invoices/{id}", get(routes::invoices::get_invoice))
        .route(
            "/invoices/{id}/finalize",
            post(routes::invoices::finalize_invoice),
        )
        .route("/invoices/{id}/void", post(routes::invoices::void_invoice))
        .route(
            "/invoices/{id}/mark-uncollectible",
            post(routes::invoices::mark_uncollectible),
        )
        .route(
            "/invoices/{id}/reopen",
            post(routes::invoices::reopen_invoice),
        )
        .route("/invoices/{id}/pay", post(routes::payments::pay_invoice))
        .route(
            "/payment-attempts/{id}",
            get(routes::payments::get_payment_attempt),
        )
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}
