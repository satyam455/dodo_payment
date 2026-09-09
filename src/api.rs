use crate::{App, digest, jobs::enqueue_event};
use axum::{
    Extension, Json, Router,
    extract::{FromRequest, Path, Query, Request, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

pub struct Error(pub StatusCode, pub &'static str, pub String);
impl Error {
    fn bad(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, "invalid_request", message.into())
    }
    fn conflict(code: &'static str, message: &str) -> Self {
        Self(StatusCode::CONFLICT, code, message.into())
    }
    fn missing() -> Self {
        Self(
            StatusCode::NOT_FOUND,
            "not_found",
            "Resource not found".into(),
        )
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(json!({"error": {"code": self.1, "message": self.2}})),
        )
            .into_response()
    }
}
impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(%error, "database operation failed");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Database operation failed".into(),
        )
    }
}

pub struct Body<T>(pub T);
impl<T, S> FromRequest<S> for Body<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = Error;
    async fn from_request(req: Request, state: &S) -> Result<Self, Error> {
        Json::<T>::from_request(req, state)
            .await
            .map(|Json(v)| Self(v))
            .map_err(|e: JsonRejection| Error(e.status(), "invalid_request", e.body_text()))
    }
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/customers", post(create_customer).get(list_customers))
        .route("/customers/{id}", get(get_customer))
        .route("/invoices", post(create_invoice).get(list_invoices))
        .route("/invoices/{id}", get(get_invoice))
        .route("/invoices/{id}/pay", post(pay))
        .route("/webhook-endpoints", post(register_webhook))
        .fallback(|| async { Error::missing() })
        .method_not_allowed_fallback(|| async {
            Error(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "Method not allowed".into(),
            )
        })
        .layer(middleware::from_fn_with_state(app.clone(), authenticate))
        .with_state(app)
}

async fn authenticate(
    State(app): State<App>,
    mut req: Request,
    next: Next,
) -> Result<Response, Error> {
    let key = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or_else(|| {
            Error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "A valid Bearer API key is required".into(),
            )
        })?;
    let business: Option<Uuid> = sqlx::query_scalar(
        "SELECT business_id FROM api_keys WHERE key_hash = $1 AND revoked_at IS NULL",
    )
    .bind(digest(key))
    .fetch_optional(&app.db)
    .await?;
    let business = business.ok_or_else(|| {
        Error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "A valid Bearer API key is required".into(),
        )
    })?;
    req.extensions_mut().insert(business);
    Ok(next.run(req).await)
}

fn id(value: &str) -> Result<Uuid, Error> {
    Uuid::parse_str(value).map_err(|_| Error::bad("ID must be a UUID"))
}
fn bounded(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CustomerInput {
    name: String,
    email: String,
}
async fn create_customer(
    State(app): State<App>,
    Extension(business_id): Extension<Uuid>,
    Body(input): Body<CustomerInput>,
) -> Result<impl IntoResponse, Error> {
    if !bounded(&input.name, 200) || !bounded(&input.email, 320) || !input.email.contains('@') {
        return Err(Error::bad(
            "name (1-200 bytes) and a valid email (up to 320 bytes) are required",
        ));
    }
    let value: Value = sqlx::query_scalar("INSERT INTO customers(id, business_id, name, email) VALUES ($1,$2,$3,$4) RETURNING to_jsonb(customers) - 'business_id'")
        .bind(Uuid::new_v4()).bind(business_id).bind(input.name).bind(input.email).fetch_one(&app.db).await?;
    Ok((StatusCode::CREATED, Json(value)))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Page {
    limit: Option<i64>,
    offset: Option<i64>,
    state: Option<String>,
}
fn page(
    query: Result<Query<Page>, axum::extract::rejection::QueryRejection>,
) -> Result<Page, Error> {
    let p = query.map_err(|e| Error::bad(e.body_text()))?.0;
    if !(1..=100).contains(&p.limit.unwrap_or(50)) || p.offset.unwrap_or(0) < 0 {
        return Err(Error::bad(
            "limit must be 1-100; offset must be nonnegative",
        ));
    }
    Ok(p)
}
async fn list_customers(
    State(app): State<App>,
    Extension(business_id): Extension<Uuid>,
    q: Result<Query<Page>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, Error> {
    let p = page(q)?;
    if p.state.is_some() {
        return Err(Error::bad("Customers have no state filter"));
    }
    let items: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(c) - 'business_id' FROM customers c WHERE business_id=$1 ORDER BY created_at, id LIMIT $2 OFFSET $3")
        .bind(business_id).bind(p.limit.unwrap_or(50)).bind(p.offset.unwrap_or(0)).fetch_all(&app.db).await?;
    Ok(Json(json!({"data": items})))
}
async fn get_customer(
    State(app): State<App>,
    Extension(business_id): Extension<Uuid>,
    Path(raw): Path<String>,
) -> Result<Json<Value>, Error> {
    let value = sqlx::query_scalar::<_, Value>(
        "SELECT to_jsonb(c) - 'business_id' FROM customers c WHERE business_id=$1 AND id=$2",
    )
    .bind(business_id)
    .bind(id(&raw)?)
    .fetch_optional(&app.db)
    .await?
    .ok_or_else(Error::missing)?;
    Ok(Json(value))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LineItem {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InvoiceInput {
    customer_id: Uuid,
    line_items: Vec<LineItem>,
    due_date: NaiveDate,
}
pub fn total(items: &[LineItem]) -> Result<i64, Error> {
    if items.is_empty() || items.len() > 100 {
        return Err(Error::bad("Provide 1-100 line items"));
    }
    let mut total = 0_i64;
    for item in items {
        if !bounded(&item.description, 500) || item.quantity <= 0 || item.unit_amount_cents < 0 {
            return Err(Error::bad(
                "Each item needs a description, positive quantity and nonnegative unit_amount_cents",
            ));
        }
        total = item
            .quantity
            .checked_mul(item.unit_amount_cents)
            .and_then(|amount| total.checked_add(amount))
            .ok_or_else(|| Error::bad("Invoice total exceeds integer range"))?;
    }
    if total == 0 {
        return Err(Error::bad("Invoice total must be positive"));
    }
    Ok(total)
}
async fn create_invoice(
    State(app): State<App>,
    Extension(business_id): Extension<Uuid>,
    Body(input): Body<InvoiceInput>,
) -> Result<impl IntoResponse, Error> {
    let total = total(&input.line_items)?;
    let mut tx = app.db.begin().await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM customers WHERE business_id=$1 AND id=$2)")
            .bind(business_id)
            .bind(input.customer_id)
            .fetch_one(&mut *tx)
            .await?;
    if !exists {
        return Err(Error::missing());
    }
    let invoice: Value = sqlx::query_scalar("INSERT INTO invoices(id,business_id,customer_id,line_items,total_cents,due_date) VALUES ($1,$2,$3,$4,$5,$6) RETURNING to_jsonb(invoices) - 'business_id'")
        .bind(Uuid::new_v4()).bind(business_id).bind(input.customer_id).bind(json!(input.line_items)).bind(total).bind(input.due_date)
        .fetch_one(&mut *tx).await?;
    enqueue_event(&mut tx, business_id, "invoice.created", &invoice).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(invoice)))
}
async fn list_invoices(
    State(app): State<App>,
    Extension(business_id): Extension<Uuid>,
    q: Result<Query<Page>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, Error> {
    let p = page(q)?;
    if p.state
        .as_deref()
        .is_some_and(|s| !["open", "processing", "paid"].contains(&s))
    {
        return Err(Error::bad("state must be open, processing or paid"));
    }
    let items: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(i) - 'business_id' FROM invoices i WHERE business_id=$1 AND ($2::text IS NULL OR state=$2) ORDER BY created_at,id LIMIT $3 OFFSET $4")
        .bind(business_id).bind(p.state).bind(p.limit.unwrap_or(50)).bind(p.offset.unwrap_or(0)).fetch_all(&app.db).await?;
    Ok(Json(json!({"data": items})))
}
async fn get_invoice(
    State(app): State<App>,
    Extension(business_id): Extension<Uuid>,
    Path(raw): Path<String>,
) -> Result<Json<Value>, Error> {
    // One SQL snapshot keeps invoice state and attempts mutually consistent.
    let value: Option<Value> = sqlx::query_scalar(
        r#"SELECT (to_jsonb(i) - 'business_id') || jsonb_build_object(
            'payment_attempts', COALESCE((
                SELECT jsonb_agg(jsonb_build_object(
                    'id', p.id, 'status', p.status, 'psp_ref', p.psp_ref,
                    'failure_code', p.failure_code, 'created_at', p.created_at
                ) ORDER BY p.created_at, p.id)
                FROM payment_attempts p WHERE p.invoice_id = i.id
            ), '[]'::jsonb)
        )
        FROM invoices i WHERE business_id = $1 AND id = $2"#,
    )
    .bind(business_id)
    .bind(id(&raw)?)
    .fetch_optional(&app.db)
    .await?;
    Ok(Json(value.ok_or_else(Error::missing)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PayInput {
    card_token: String,
}
fn replay(row: &sqlx::postgres::PgRow, hash: &str) -> Result<(StatusCode, Json<Value>), Error> {
    if row.get::<String, _>("request_hash") != hash {
        return Err(Error::conflict(
            "idempotency_key_reused",
            "This key was used for a different invoice or card token",
        ));
    }
    Ok((StatusCode::ACCEPTED, Json(row.get("response"))))
}
async fn pay(
    State(app): State<App>,
    Extension(business_id): Extension<Uuid>,
    Path(raw): Path<String>,
    headers: HeaderMap,
    Body(input): Body<PayInput>,
) -> Result<(StatusCode, Json<Value>), Error> {
    let invoice_id = id(&raw)?;
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|v| bounded(v, 200))
        .ok_or_else(|| Error::bad("Idempotency-Key (1-200 bytes) is required"))?;
    if ![
        "tok_success",
        "tok_insufficient_funds",
        "tok_card_declined",
        "tok_timeout",
        "tok_network_error",
    ]
    .contains(&input.card_token.as_str())
    {
        return Err(Error::bad("Unknown mock card token"));
    }
    let hash =
        digest(&json!({"invoice_id": invoice_id, "card_token": input.card_token}).to_string());
    let mut tx = app.db.begin().await?;
    // Serialize the invoice, including concurrent retries of its first attempt.
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM invoices WHERE business_id=$1 AND id=$2 FOR UPDATE")
            .bind(business_id)
            .bind(invoice_id)
            .fetch_optional(&mut *tx)
            .await?;
    let state = state.ok_or_else(Error::missing)?;
    if let Some(row) = sqlx::query("SELECT request_hash,response FROM payment_attempts WHERE business_id=$1 AND idempotency_key=$2")
        .bind(business_id).bind(key).fetch_optional(&mut *tx).await? {
        return replay(&row, &hash);
    }
    if state != "open" {
        return Err(Error::conflict(
            "invalid_invoice_state",
            "Only an open invoice can start a payment",
        ));
    }
    let attempt_id = Uuid::new_v4();
    let response = json!({"attempt_id": attempt_id, "invoice_id": invoice_id, "status": "pending", "status_url": format!("/invoices/{invoice_id}")});
    let inserted = sqlx::query("INSERT INTO payment_attempts(id,business_id,invoice_id,idempotency_key,request_hash,card_token,response) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (business_id,idempotency_key) DO NOTHING")
        .bind(attempt_id).bind(business_id).bind(invoice_id).bind(key).bind(&hash).bind(input.card_token).bind(&response)
        .execute(&mut *tx).await?.rows_affected();
    if inserted == 0 {
        return Err(Error::conflict(
            "idempotency_key_reused",
            "This key was used for a different invoice",
        ));
    }
    sqlx::query("UPDATE invoices SET state='processing' WHERE id=$1")
        .bind(invoice_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::ACCEPTED, Json(response)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookInput {
    url: String,
    secret: String,
}
async fn register_webhook(
    State(app): State<App>,
    Extension(business_id): Extension<Uuid>,
    Body(input): Body<WebhookInput>,
) -> Result<impl IntoResponse, Error> {
    let url = reqwest::Url::parse(&input.url).map_err(|_| Error::bad("Invalid endpoint URL"))?;
    if !["http", "https"].contains(&url.scheme())
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || input.url.len() > 2048
        || !(16..=256).contains(&input.secret.len())
    {
        return Err(Error::bad(
            "Provide an http(s) URL without credentials or fragment and a 16-256 byte signing secret",
        ));
    }
    let endpoint_id = Uuid::new_v4();
    sqlx::query("INSERT INTO webhook_endpoints(id,business_id,url,secret) VALUES ($1,$2,$3,$4)")
        .bind(endpoint_id)
        .bind(business_id)
        .bind(&input.url)
        .bind(input.secret)
        .execute(&app.db)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"id": endpoint_id, "url": input.url})),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn money_rejects_overflow_and_zero() {
        let item = |q, u| LineItem {
            description: "Work".into(),
            quantity: q,
            unit_amount_cents: u,
        };
        assert_eq!(total(&[item(3, 125), item(1, 25)]).ok(), Some(400));
        assert!(total(&[item(i64::MAX, 2)]).is_err());
        assert!(total(&[item(1, i64::MAX), item(1, 1)]).is_err());
        assert!(total(&[item(0, 100)]).is_err());
        assert!(total(&[item(1, 0)]).is_err());
    }
}
