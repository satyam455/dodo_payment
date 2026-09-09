use crate::{
    api::{Body, Error},
    jobs::verify_signature,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub fn router(db: PgPool) -> Router {
    Router::new()
        .route("/charges", post(charge))
        .route("/webhooks", post(receive))
        .with_state(db)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Charge {
    card_token: String,
    amount_cents: i64,
    currency: String,
}
async fn charge(
    State(db): State<PgPool>,
    headers: HeaderMap,
    Body(input): Body<Charge>,
) -> Result<Response, Error> {
    let id = headers
        .get("idempotency-key")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| {
            Error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Idempotency-Key UUID required".into(),
            )
        })?;
    if input.amount_cents <= 0 || input.currency != "USD" {
        return Err(Error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Positive USD cents required".into(),
        ));
    }
    let outcome = match input.card_token.as_str() {
        "tok_success" | "tok_timeout" => json!({"status":"succeeded", "psp_ref": Uuid::new_v4()}),
        "tok_insufficient_funds" => json!({"status":"failed", "code":"insufficient_funds"}),
        "tok_card_declined" => json!({"status":"failed", "code":"card_declined"}),
        "tok_network_error" => {
            json!({"error":{"code":"processor_unavailable","message":"Simulated network failure"}})
        }
        _ => {
            return Err(Error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Unknown card token".into(),
            ));
        }
    };
    let delay_ms = if input.card_token == "tok_timeout" {
        30_000_i64
    } else {
        100
    };
    let request = json!(input);
    let row = sqlx::query("INSERT INTO mock_psp.charges(id,request,response,ready_at) VALUES ($1,$2,$3,now()+($4::bigint * interval '1 millisecond')) ON CONFLICT (id) DO UPDATE SET calls=mock_psp.charges.calls+1 RETURNING request,response,ready_at")
        .bind(id).bind(&request).bind(outcome).bind(delay_ms).fetch_one(&db).await?;
    if row.get::<Value, _>("request") != request {
        return Err(Error(
            StatusCode::CONFLICT,
            "idempotency_key_reused",
            "Processor request does not match original".into(),
        ));
    }
    let ready: DateTime<Utc> = row.get("ready_at");
    if let Ok(delay) = (ready - Utc::now()).to_std() {
        tokio::time::sleep(delay).await;
    }
    let status = if input.card_token == "tok_network_error" {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    };
    Ok((status, Json(row.get::<Value, _>("response"))).into_response())
}

async fn receive(headers: HeaderMap, body: Bytes) -> StatusCode {
    let timestamp = headers
        .get("webhook-timestamp")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let signature = headers
        .get("webhook-signature")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("v1="))
        .unwrap_or("");
    let secret = std::env::var("DEMO_WEBHOOK_SECRET")
        .unwrap_or_else(|_| "local-webhook-secret-change-me".into());
    if !verify_signature(&secret, timestamp, &body, signature) {
        return StatusCode::UNAUTHORIZED;
    }
    tracing::info!(payload=%String::from_utf8_lossy(&body), "verified demo webhook");
    StatusCode::NO_CONTENT
}
