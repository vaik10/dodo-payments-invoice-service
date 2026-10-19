use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

// ── Business ──

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Business {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ── API Key ──

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ApiKey {
    pub id: Uuid,
    pub business_id: Uuid,
    pub key_prefix: String,
    pub key_hash: String,
    pub revoked: bool,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

// ── Customer ──

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Customer {
    pub id: Uuid,
    pub business_id: Uuid,
    pub name: String,
    pub email: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateCustomerRequest {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Serialize)]
pub struct CustomerResponse {
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<Customer> for CustomerResponse {
    fn from(c: Customer) -> Self {
        Self {
            id: c.id,
            name: c.name,
            email: c.email,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }
    }
}

// ── Invoice ──

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceStatus {
    Draft,
    Issued,
    Paid,
    Void,
    Uncollectible,
}

impl InvoiceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Issued => "issued",
            Self::Paid => "paid",
            Self::Void => "void",
            Self::Uncollectible => "uncollectible",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "draft" => Some(Self::Draft),
            "issued" => Some(Self::Issued),
            "paid" => Some(Self::Paid),
            "void" => Some(Self::Void),
            "uncollectible" => Some(Self::Uncollectible),
            _ => None,
        }
    }

    /// Returns the list of states this state can transition to.
    pub fn valid_transitions(&self) -> &'static [InvoiceStatus] {
        match self {
            Self::Draft => &[Self::Issued, Self::Void],
            Self::Issued => &[Self::Paid, Self::Void, Self::Uncollectible],
            Self::Uncollectible => &[Self::Issued],
            Self::Paid | Self::Void => &[],
        }
    }

    pub fn can_transition_to(&self, target: &InvoiceStatus) -> bool {
        self.valid_transitions().contains(target)
    }
}

impl std::fmt::Display for InvoiceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Invoice {
    pub id: Uuid,
    pub business_id: Uuid,
    pub customer_id: Uuid,
    pub status: String,
    pub total_amount_cents: i64,
    pub due_date: NaiveDate,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct InvoiceResponse {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub status: String,
    pub total_amount_cents: i64,
    pub due_date: NaiveDate,
    pub line_items: Vec<LineItemResponse>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceRequest {
    pub customer_id: Uuid,
    pub due_date: NaiveDate,
    pub line_items: Vec<CreateLineItemRequest>,
}

#[derive(Debug, Deserialize)]
pub struct CreateLineItemRequest {
    pub description: String,
    pub quantity: i32,
    pub unit_amount_cents: i64,
}

// ── Line Item ──

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct LineItem {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub description: String,
    pub quantity: i32,
    pub unit_amount_cents: i64,
    pub line_total_cents: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct LineItemResponse {
    pub id: Uuid,
    pub description: String,
    pub quantity: i32,
    pub unit_amount_cents: i64,
    pub line_total_cents: i64,
}

impl From<LineItem> for LineItemResponse {
    fn from(li: LineItem) -> Self {
        Self {
            id: li.id,
            description: li.description,
            quantity: li.quantity,
            unit_amount_cents: li.unit_amount_cents,
            line_total_cents: li.line_total_cents,
        }
    }
}

// ── Payment Attempt ──

#[derive(Debug, Clone, FromRow)]
pub struct PaymentAttempt {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub card_token: String,
    pub amount_cents: i64,
    pub status: String,
    pub failure_code: Option<String>,
    pub failure_message: Option<String>,
    pub psp_ref: Option<String>,
    pub idempotency_key: Option<String>,
    pub psp_idempotency_key: Option<String>,
    pub resolution_attempts: i16,
    pub next_resolution_at: Option<DateTime<Utc>>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PaymentAttemptResponse {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub amount_cents: i64,
    pub status: String,
    pub failure_code: Option<String>,
    pub failure_message: Option<String>,
    pub psp_ref: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl From<PaymentAttempt> for PaymentAttemptResponse {
    fn from(pa: PaymentAttempt) -> Self {
        Self {
            id: pa.id,
            invoice_id: pa.invoice_id,
            amount_cents: pa.amount_cents,
            status: pa.status,
            failure_code: pa.failure_code,
            failure_message: pa.failure_message,
            psp_ref: pa.psp_ref,
            created_at: pa.created_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PayInvoiceRequest {
    pub card_token: String,
}

// ── Idempotency Key ──

#[derive(Debug, Clone, FromRow)]
pub struct IdempotencyKey {
    pub key: String,
    pub business_id: Uuid,
    pub request_hash: String,
    pub response_code: Option<i32>,
    pub response_body: Option<serde_json::Value>,
    pub status: String,
    pub locked_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

// ── Webhook Endpoint ──

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct WebhookEndpoint {
    pub id: Uuid,
    pub business_id: Uuid,
    pub url: String,
    pub signing_secret: String,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateWebhookEndpointRequest {
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct WebhookEndpointResponse {
    pub id: Uuid,
    pub url: String,
    pub active: bool,
    pub signing_secret: String,
    pub created_at: DateTime<Utc>,
}

impl From<WebhookEndpoint> for WebhookEndpointResponse {
    fn from(we: WebhookEndpoint) -> Self {
        Self {
            id: we.id,
            url: we.url,
            active: we.active,
            signing_secret: we.signing_secret,
            created_at: we.created_at,
        }
    }
}

// ── Webhook Delivery ──

#[derive(Debug, Clone, FromRow)]
pub struct WebhookDelivery {
    pub id: Uuid,
    pub webhook_endpoint_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub status: String,
    pub attempt_count: i16,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
}

// ── PSP types ──

#[derive(Debug, Serialize)]
pub struct PspChargeRequest {
    pub card_token: String,
    pub amount_cents: i64,
    pub idempotency_key: String,
}

#[derive(Debug, Deserialize)]
pub struct PspChargeResponse {
    pub status: String,
    #[serde(default)]
    pub psp_ref: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
}

/// Body of the PSP's `GET /charges/{idempotency_key}`.
#[derive(Debug, Deserialize)]
pub struct PspChargeLookupResponse {
    /// `succeeded`, `failed` or `processing`.
    pub status: String,
    #[serde(default)]
    pub psp_ref: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
}

// ── Error response ──

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment_attempt_id: Option<Uuid>,
}

impl ErrorResponse {
    pub fn new(error: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            message: message.into(),
            current_state: None,
            requested_state: None,
            payment_attempt_id: None,
        }
    }

    pub fn state_transition(current: &str, requested: &str) -> Self {
        Self {
            error: "invalid_state_transition".to_string(),
            message: format!(
                "Cannot transition invoice from '{}' to '{}'",
                current, requested
            ),
            current_state: Some(current.to_string()),
            requested_state: Some(requested.to_string()),
            payment_attempt_id: None,
        }
    }

    pub fn payment_pending(payment_attempt_id: Uuid) -> Self {
        Self {
            payment_attempt_id: Some(payment_attempt_id),
            ..Self::new(
                "payment_pending",
                "A payment attempt for this invoice has not been resolved yet",
            )
        }
    }
}

// ── List query params ──

#[derive(Debug, Deserialize)]
pub struct ListInvoicesQuery {
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ListCustomersQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}
