use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use uuid::Uuid;

use crate::{
    AppState,
    auth::AuthenticatedBusiness,
    models::{
        CreateCustomerRequest, Customer, CustomerResponse, ErrorResponse, ListCustomersQuery,
    },
};

pub async fn create_customer(
    State(state): State<AppState>,
    auth: AuthenticatedBusiness,
    Json(body): Json<CreateCustomerRequest>,
) -> Result<(StatusCode, Json<CustomerResponse>), (StatusCode, Json<ErrorResponse>)> {
    if body.name.trim().is_empty() || body.email.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "validation_error",
                "Name and email cannot be empty",
            )),
        ));
    }

    let result = sqlx::query_as::<_, Customer>(
        "INSERT INTO customers (id, business_id, name, email) VALUES (gen_random_uuid(), $1, $2, $3) RETURNING *"
    )
    .bind(auth.business_id)
    .bind(body.name)
    .bind(body.email)
    .fetch_one(&state.db)
    .await;

    match result {
        Ok(customer) => Ok((StatusCode::CREATED, Json(customer.into()))),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse::new(
                "customer_already_exists",
                "Customer with this email already exists",
            )),
        )),
        Err(_) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(
                "internal_error",
                "Failed to create customer",
            )),
        )),
    }
}

pub async fn get_customer(
    State(state): State<AppState>,
    auth: AuthenticatedBusiness,
    Path(customer_id): Path<Uuid>,
) -> Result<Json<CustomerResponse>, (StatusCode, Json<ErrorResponse>)> {
    let result =
        sqlx::query_as::<_, Customer>("SELECT * FROM customers WHERE id = $1 AND business_id = $2")
            .bind(customer_id)
            .bind(auth.business_id)
            .fetch_optional(&state.db)
            .await;

    match result {
        Ok(Some(customer)) => Ok(Json(customer.into())),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("not_found", "Customer not found")),
        )),
        Err(_) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(
                "internal_error",
                "Failed to fetch customer",
            )),
        )),
    }
}

pub async fn list_customers(
    State(state): State<AppState>,
    auth: AuthenticatedBusiness,
    Query(params): Query<ListCustomersQuery>,
) -> Result<Json<Vec<CustomerResponse>>, (StatusCode, Json<ErrorResponse>)> {
    let limit = params.limit.unwrap_or(50);
    let offset = params.offset.unwrap_or(0);

    let result = sqlx::query_as::<_, Customer>(
        "SELECT * FROM customers WHERE business_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3"
    )
    .bind(auth.business_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await;

    match result {
        Ok(customers) => {
            let responses = customers.into_iter().map(|c| c.into()).collect();
            Ok(Json(responses))
        }
        Err(_) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(
                "internal_error",
                "Failed to list customers",
            )),
        )),
    }
}
