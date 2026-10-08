use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde_json::json;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthenticatedBusiness;
use crate::models::{
    CreateInvoiceRequest, ErrorResponse, Invoice, InvoiceResponse, InvoiceStatus, LineItem,
    LineItemResponse, ListInvoicesQuery,
};
use crate::webhooks::enqueue_webhook;

pub async fn create_invoice(
    State(state): State<AppState>,
    auth: AuthenticatedBusiness,
    Json(body): Json<CreateInvoiceRequest>,
) -> Result<(StatusCode, Json<InvoiceResponse>), (StatusCode, Json<ErrorResponse>)> {
    if body.line_items.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "validation_error",
                "Invoice must have at least one line item",
            )),
        ));
    }

    let mut total_amount_cents: i64 = 0;
    for item in &body.line_items {
        if item.quantity <= 0 || item.unit_amount_cents < 0 {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "validation_error",
                    "Invalid line item amounts",
                )),
            ));
        }
        total_amount_cents = total_amount_cents
            .checked_add(
                (item.quantity as i64)
                    .checked_mul(item.unit_amount_cents)
                    .unwrap(),
            )
            .unwrap();
    }

    // Verify customer belongs to business
    let customer_exists: (bool,) =
        sqlx::query_as("SELECT EXISTS(SELECT 1 FROM customers WHERE id = $1 AND business_id = $2)")
            .bind(body.customer_id)
            .bind(auth.business_id)
            .fetch_one(&state.db)
            .await
            .unwrap_or((false,));

    if !customer_exists.0 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "validation_error",
                "Invalid customer ID",
            )),
        ));
    }

    let mut tx = state.db.begin().await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(
                "internal_error",
                "Failed to start transaction",
            )),
        )
    })?;

    let invoice = sqlx::query_as::<_, Invoice>(
        "INSERT INTO invoices (id, business_id, customer_id, status, total_amount_cents, due_date)
         VALUES (gen_random_uuid(), $1, $2, 'draft', $3, $4)
         RETURNING *",
    )
    .bind(auth.business_id)
    .bind(body.customer_id)
    .bind(total_amount_cents)
    .bind(body.due_date)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!("Failed to insert invoice: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("internal_error", "Database error")),
        )
    })?;

    let mut line_items_resp = Vec::new();

    for item in &body.line_items {
        let line_total = (item.quantity as i64) * item.unit_amount_cents;
        let db_item = sqlx::query_as::<_, LineItem>(
            "INSERT INTO line_items (id, invoice_id, description, quantity, unit_amount_cents, line_total_cents)
             VALUES (gen_random_uuid(), $1, $2, $3, $4, $5)
             RETURNING *"
        )
        .bind(invoice.id)
        .bind(&item.description)
        .bind(item.quantity)
        .bind(item.unit_amount_cents)
        .bind(line_total)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            tracing::error!("Failed to insert line item: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new("internal_error", "Database error")),
            )
        })?;

        line_items_resp.push(LineItemResponse::from(db_item));
    }

    let payload = json!({
        "event": "invoice.created",
        "invoice_id": invoice.id,
        "status": invoice.status,
        "total_amount_cents": invoice.total_amount_cents
    });

    enqueue_webhook(&mut tx, auth.business_id, "invoice.created", &payload)
        .await
        .map_err(|e| {
            tracing::error!("Failed to enqueue webhook: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new("internal_error", "Database error")),
            )
        })?;

    tx.commit().await.map_err(|e| {
        tracing::error!("Failed to commit transaction: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("internal_error", "Database error")),
        )
    })?;

    let response = InvoiceResponse {
        id: invoice.id,
        customer_id: invoice.customer_id,
        status: invoice.status,
        total_amount_cents: invoice.total_amount_cents,
        due_date: invoice.due_date,
        line_items: line_items_resp,
        created_at: invoice.created_at,
        updated_at: invoice.updated_at,
    };

    Ok((StatusCode::CREATED, Json(response)))
}

pub async fn get_invoice(
    State(state): State<AppState>,
    auth: AuthenticatedBusiness,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<InvoiceResponse>, (StatusCode, Json<ErrorResponse>)> {
    let invoice =
        sqlx::query_as::<_, Invoice>("SELECT * FROM invoices WHERE id = $1 AND business_id = $2")
            .bind(invoice_id)
            .bind(auth.business_id)
            .fetch_optional(&state.db)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse::new("internal_error", "Database error")),
                )
            })?
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse::new("not_found", "Invoice not found")),
                )
            })?;

    let line_items = sqlx::query_as::<_, LineItem>(
        "SELECT * FROM line_items WHERE invoice_id = $1 ORDER BY created_at ASC",
    )
    .bind(invoice_id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("internal_error", "Database error")),
        )
    })?;

    Ok(Json(InvoiceResponse {
        id: invoice.id,
        customer_id: invoice.customer_id,
        status: invoice.status,
        total_amount_cents: invoice.total_amount_cents,
        due_date: invoice.due_date,
        line_items: line_items.into_iter().map(|li| li.into()).collect(),
        created_at: invoice.created_at,
        updated_at: invoice.updated_at,
    }))
}

pub async fn list_invoices(
    State(state): State<AppState>,
    auth: AuthenticatedBusiness,
    Query(params): Query<ListInvoicesQuery>,
) -> Result<Json<Vec<InvoiceResponse>>, (StatusCode, Json<ErrorResponse>)> {
    let limit = params.limit.unwrap_or(50).clamp(1, 100);
    let offset = params.offset.unwrap_or(0).max(0);

    let invoices = if let Some(status_str) = params.status {
        if InvoiceStatus::parse(&status_str).is_none() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new("validation_error", "Invalid status parameter")),
            ));
        }

        sqlx::query_as::<_, Invoice>(
            "SELECT * FROM invoices WHERE business_id = $1 AND status = $2 ORDER BY created_at DESC LIMIT $3 OFFSET $4"
        )
        .bind(auth.business_id)
        .bind(&status_str)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await
    } else {
        sqlx::query_as::<_, Invoice>(
            "SELECT * FROM invoices WHERE business_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3"
        )
        .bind(auth.business_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await
    }.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("internal_error", "Database error")),
        )
    })?;

    let responses = invoices
        .into_iter()
        .map(|inv| InvoiceResponse {
            id: inv.id,
            customer_id: inv.customer_id,
            status: inv.status,
            total_amount_cents: inv.total_amount_cents,
            due_date: inv.due_date,
            line_items: vec![], // Omitted in list view
            created_at: inv.created_at,
            updated_at: inv.updated_at,
        })
        .collect();

    Ok(Json(responses))
}

async fn transition_invoice_status(
    db: &sqlx::PgPool,
    business_id: Uuid,
    invoice_id: Uuid,
    target_status: InvoiceStatus,
) -> Result<InvoiceResponse, (StatusCode, Json<ErrorResponse>)> {
    let mut tx = db.begin().await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(
                "internal_error",
                "Failed to start transaction",
            )),
        )
    })?;

    // We use FOR UPDATE to prevent race conditions during state transitions
    let invoice = sqlx::query_as::<_, Invoice>(
        "SELECT * FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE",
    )
    .bind(invoice_id)
    .bind(business_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("internal_error", "Database error")),
        )
    })?
    .ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("not_found", "Invoice not found")),
        )
    })?;

    let current_status =
        InvoiceStatus::parse(&invoice.status).expect("Database has invalid status");

    if !current_status.can_transition_to(&target_status) {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse::state_transition(
                current_status.as_str(),
                target_status.as_str(),
            )),
        ));
    }

    let updated_invoice = sqlx::query_as::<_, Invoice>(
        "UPDATE invoices SET status = $1, updated_at = now() WHERE id = $2 RETURNING *",
    )
    .bind(target_status.as_str())
    .bind(invoice_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("internal_error", "Database error")),
        )
    })?;

    // Fetch line items for response
    let line_items = sqlx::query_as::<_, LineItem>(
        "SELECT * FROM line_items WHERE invoice_id = $1 ORDER BY created_at ASC",
    )
    .bind(invoice_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("internal_error", "Database error")),
        )
    })?;

    // Fire webhook if transitioning to 'issued'
    if target_status == InvoiceStatus::Issued {
        let payload = json!({
            "event": "invoice.issued",
            "invoice_id": updated_invoice.id,
            "status": updated_invoice.status,
            "total_amount_cents": updated_invoice.total_amount_cents
        });
        if let Err(e) = enqueue_webhook(&mut tx, business_id, "invoice.issued", &payload).await {
            tracing::error!("Failed to enqueue webhook: {}", e);
        }
    }

    tx.commit().await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("internal_error", "Database error")),
        )
    })?;

    Ok(InvoiceResponse {
        id: updated_invoice.id,
        customer_id: updated_invoice.customer_id,
        status: updated_invoice.status,
        total_amount_cents: updated_invoice.total_amount_cents,
        due_date: updated_invoice.due_date,
        line_items: line_items.into_iter().map(|li| li.into()).collect(),
        created_at: updated_invoice.created_at,
        updated_at: updated_invoice.updated_at,
    })
}

pub async fn finalize_invoice(
    State(state): State<AppState>,
    auth: AuthenticatedBusiness,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<InvoiceResponse>, (StatusCode, Json<ErrorResponse>)> {
    let res = transition_invoice_status(
        &state.db,
        auth.business_id,
        invoice_id,
        InvoiceStatus::Issued,
    )
    .await?;
    Ok(Json(res))
}

pub async fn void_invoice(
    State(state): State<AppState>,
    auth: AuthenticatedBusiness,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<InvoiceResponse>, (StatusCode, Json<ErrorResponse>)> {
    let res =
        transition_invoice_status(&state.db, auth.business_id, invoice_id, InvoiceStatus::Void)
            .await?;
    Ok(Json(res))
}

pub async fn mark_uncollectible(
    State(state): State<AppState>,
    auth: AuthenticatedBusiness,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<InvoiceResponse>, (StatusCode, Json<ErrorResponse>)> {
    let res = transition_invoice_status(
        &state.db,
        auth.business_id,
        invoice_id,
        InvoiceStatus::Uncollectible,
    )
    .await?;
    Ok(Json(res))
}

pub async fn reopen_invoice(
    State(state): State<AppState>,
    auth: AuthenticatedBusiness,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<InvoiceResponse>, (StatusCode, Json<ErrorResponse>)> {
    let res = transition_invoice_status(
        &state.db,
        auth.business_id,
        invoice_id,
        InvoiceStatus::Issued,
    )
    .await?;
    Ok(Json(res))
}
