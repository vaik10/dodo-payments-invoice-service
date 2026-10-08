use axum::{
    extract::FromRequestParts,
    http::{StatusCode, header, request::Parts},
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

/// Authenticated business context, extracted from the Authorization header.
#[derive(Debug, Clone)]
pub struct AuthenticatedBusiness {
    pub business_id: Uuid,
    pub api_key_id: Uuid,
}

impl<S> FromRequestParts<S> for AuthenticatedBusiness
where
    S: Send + Sync,
    crate::AppState: axum::extract::FromRef<S>,
{
    type Rejection = (StatusCode, axum::Json<crate::models::ErrorResponse>);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = <crate::AppState as axum::extract::FromRef<S>>::from_ref(state);

        let auth_header = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(crate::models::ErrorResponse::new(
                        "unauthorized",
                        "Missing Authorization header",
                    )),
                )
            })?;

        let raw_key = auth_header.strip_prefix("Bearer ").ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                axum::Json(crate::models::ErrorResponse::new(
                    "unauthorized",
                    "Authorization header must use Bearer scheme",
                )),
            )
        })?;

        authenticate_api_key(&app_state.db, raw_key)
            .await
            .map_err(|_| {
                (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(crate::models::ErrorResponse::new(
                        "unauthorized",
                        "Invalid API key",
                    )),
                )
            })
    }
}

/// Validate an API key: extract the prefix, look up the row, verify the hash.
async fn authenticate_api_key(db: &PgPool, raw_key: &str) -> Result<AuthenticatedBusiness, ()> {
    // Key format: dodo_live_XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX
    // Prefix for lookup: first 8 chars of the random part
    let stripped = raw_key.strip_prefix("dodo_live_").ok_or(())?;
    if stripped.len() < 8 {
        return Err(());
    }
    let prefix = &stripped[..8];

    // Hash the full key
    let mut hasher = Sha256::new();
    hasher.update(raw_key.as_bytes());
    let key_hash = hex::encode(hasher.finalize());

    // Look up by prefix, verify hash
    let row = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        "SELECT id, business_id, key_hash FROM api_keys WHERE key_prefix = $1 AND revoked = false",
    )
    .bind(prefix)
    .fetch_optional(db)
    .await
    .map_err(|_| ())?
    .ok_or(())?;

    if row.2 != key_hash {
        return Err(());
    }

    Ok(AuthenticatedBusiness {
        business_id: row.1,
        api_key_id: row.0,
    })
}

/// Generate a new API key. Returns (raw_key, key_prefix, key_hash).
pub fn generate_api_key() -> (String, String, String) {
    use rand::Rng;

    let charset: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    let random_part: String = (0..32)
        .map(|_| {
            let idx = rng.random_range(0..charset.len());
            charset[idx] as char
        })
        .collect();

    let raw_key = format!("dodo_live_{}", random_part);
    let prefix = random_part[..8].to_string();

    let mut hasher = Sha256::new();
    hasher.update(raw_key.as_bytes());
    let key_hash = hex::encode(hasher.finalize());

    (raw_key, prefix, key_hash)
}
