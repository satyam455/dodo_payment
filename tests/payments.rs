use axum::{Router, http::StatusCode};
use invoice_service::{App, api, bootstrap, jobs, psp};
use reqwest::Client;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::Barrier, task::JoinHandle};
use uuid::Uuid;

const KEY: &str = "integration-test-business-key";
struct Fixture {
    app: App,
    url: String,
    http: Client,
    tasks: Vec<JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
async fn serve(router: Router) -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (url, task)
}
impl Fixture {
    async fn new(db: PgPool, workers: bool) -> Self {
        bootstrap(&db, KEY).await.unwrap();
        let (psp_url, psp_task) = serve(psp::router(db.clone())).await;
        let app = App::new(db, psp_url);
        let (url, api_task) = serve(api::router(app.clone())).await;
        let mut tasks = vec![psp_task, api_task];
        if workers {
            tasks.push(tokio::spawn(jobs::run(app.clone())));
        }
        Self {
            app,
            url,
            http: Client::new(),
            tasks,
        }
    }
    async fn post(&self, path: &str, value: Value) -> reqwest::Response {
        self.http
            .post(format!("{}{path}", self.url))
            .bearer_auth(KEY)
            .json(&value)
            .send()
            .await
            .unwrap()
    }
    async fn invoice(&self) -> Uuid {
        let customer = self
            .post(
                "/customers",
                json!({"name":"A customer", "email":"customer@example.test"}),
            )
            .await;
        assert_eq!(customer.status(), StatusCode::CREATED);
        let customer: Value = customer.json().await.unwrap();
        let invoice = self.post("/invoices",json!({"customer_id":customer["id"],"due_date":"2026-12-31","line_items":[{"description":"Backend work","quantity":2,"unit_amount_cents":1250}]})).await;
        assert_eq!(invoice.status(), StatusCode::CREATED);
        let invoice: Value = invoice.json().await.unwrap();
        assert_eq!(invoice["total_cents"], 2500);
        Uuid::parse_str(invoice["id"].as_str().unwrap()).unwrap()
    }
    async fn pay(&self, invoice: Uuid, key: &str, token: &str) -> reqwest::Response {
        self.http
            .post(format!("{}/invoices/{invoice}/pay", self.url))
            .bearer_auth(KEY)
            .header("Idempotency-Key", key)
            .json(&json!({"card_token": token}))
            .send()
            .await
            .unwrap()
    }
    async fn get(&self, invoice: Uuid) -> Value {
        self.http
            .get(format!("{}/invoices/{invoice}", self.url))
            .bearer_auth(KEY)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn wait_for(&self, invoice: Uuid, state: &str, budget: Duration) -> Value {
        let deadline = Instant::now() + budget;
        loop {
            let value = self.get(invoice).await;
            if value["state"] == state {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "invoice never reached {state}: {value}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[sqlx::test]
async fn concurrent_payments_make_one_charge(pool: PgPool) {
    let f = Fixture::new(pool, true).await;
    let invoice = f.invoice().await;
    let barrier = Arc::new(Barrier::new(16));
    let mut calls = Vec::new();
    for _ in 0..16 {
        let http = f.http.clone();
        let url = format!("{}/invoices/{invoice}/pay", f.url);
        let barrier = barrier.clone();
        calls.push(tokio::spawn(async move {
            barrier.wait().await;
            http.post(url)
                .bearer_auth(KEY)
                .header("Idempotency-Key", Uuid::new_v4().to_string())
                .json(&json!({"card_token":"tok_success"}))
                .send()
                .await
                .unwrap()
                .status()
        }));
    }
    let mut accepted = 0;
    for call in calls {
        let status = call.await.unwrap();
        assert!([StatusCode::ACCEPTED, StatusCode::CONFLICT].contains(&status));
        accepted += usize::from(status == StatusCode::ACCEPTED);
    }
    assert_eq!(accepted, 1);
    let result = f.wait_for(invoice, "paid", Duration::from_secs(5)).await;
    assert_eq!(result["payment_attempts"].as_array().unwrap().len(), 1);
    assert_eq!(result["payment_attempts"][0]["status"], "succeeded");
    let charges: i64 = sqlx::query_scalar("SELECT count(*) FROM mock_psp.charges")
        .fetch_one(&f.app.db)
        .await
        .unwrap();
    assert_eq!(charges, 1);
    let calls: i64 = sqlx::query_scalar("SELECT sum(calls)::bigint FROM mock_psp.charges")
        .fetch_one(&f.app.db)
        .await
        .unwrap();
    assert_eq!(calls, 1);
}

#[sqlx::test]
async fn same_key_replays_receipt_without_second_psp_call(pool: PgPool) {
    let f = Fixture::new(pool, true).await;
    let invoice = f.invoice().await;
    let first = f.pay(invoice, "stable-key", "tok_success").await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let original = first.text().await.unwrap();
    let retry = f.pay(invoice, "stable-key", "tok_success").await;
    assert_eq!(retry.status(), StatusCode::ACCEPTED);
    assert_eq!(retry.text().await.unwrap(), original);
    f.wait_for(invoice, "paid", Duration::from_secs(5)).await;
    let retry = f.pay(invoice, "stable-key", "tok_success").await;
    assert_eq!(retry.status(), StatusCode::ACCEPTED);
    assert_eq!(retry.text().await.unwrap(), original);
    assert_eq!(
        f.pay(invoice, "stable-key", "tok_card_declined")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.pay(invoice, "new-key", "tok_success").await.status(),
        StatusCode::CONFLICT
    );
    let calls: i64 = sqlx::query_scalar("SELECT sum(calls)::bigint FROM mock_psp.charges")
        .fetch_one(&f.app.db)
        .await
        .unwrap();
    assert_eq!(calls, 1);
}

#[sqlx::test]
async fn slow_psp_returns_promptly_and_eventually_resolves(pool: PgPool) {
    let f = Fixture::new(pool, true).await;
    let invoice = f.invoice().await;
    let start = Instant::now();
    let response = f.pay(invoice, "slow-key", "tok_timeout").await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(start.elapsed() < Duration::from_secs(2));
    tokio::time::sleep(Duration::from_secs(3)).await;
    let pending = f.get(invoice).await;
    assert_eq!(pending["state"], "processing");
    assert_eq!(pending["payment_attempts"][0]["status"], "pending");
    assert_eq!(
        f.pay(invoice, "another-key", "tok_success").await.status(),
        StatusCode::CONFLICT
    );
    let paid = f.wait_for(invoice, "paid", Duration::from_secs(50)).await;
    assert_eq!(paid["payment_attempts"][0]["status"], "succeeded");
    let charges: i64 = sqlx::query_scalar("SELECT count(*) FROM mock_psp.charges")
        .fetch_one(&f.app.db)
        .await
        .unwrap();
    assert_eq!(charges, 1);
}

#[sqlx::test]
async fn network_error_stays_pending_and_keeps_reconciliation_scheduled(pool: PgPool) {
    let f = Fixture::new(pool, false).await;
    let invoice = f.invoice().await;
    assert_eq!(
        f.pay(invoice, "network-key", "tok_network_error")
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    assert!(jobs::payment_once(&f.app).await.unwrap());
    let value = f.get(invoice).await;
    assert_eq!(value["state"], "processing");
    assert_eq!(value["payment_attempts"][0]["status"], "pending");
    let scheduled: bool = sqlx::query_scalar(
        "SELECT next_run>now() AND retries=1 FROM payment_attempts WHERE invoice_id=$1",
    )
    .bind(invoice)
    .fetch_one(&f.app.db)
    .await
    .unwrap();
    assert!(scheduled);
    assert_eq!(
        f.pay(invoice, "replacement-key", "tok_success")
            .await
            .status(),
        StatusCode::CONFLICT
    );
}

#[sqlx::test]
async fn crash_after_psp_success_reuses_the_durable_charge(pool: PgPool) {
    let f = Fixture::new(pool, false).await;
    let invoice = f.invoice().await;
    let receipt: Value = f
        .pay(invoice, "crash-key", "tok_success")
        .await
        .json()
        .await
        .unwrap();
    // Simulate the externally committed charge whose response was lost before DB finalization.
    let charge: Value = f
        .http
        .post(format!("{}/charges", f.app.psp_url))
        .header("Idempotency-Key", receipt["attempt_id"].as_str().unwrap())
        .json(&json!({"card_token":"tok_success","amount_cents":2500,"currency":"USD"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(f.get(invoice).await["state"], "processing");
    assert!(jobs::payment_once(&f.app).await.unwrap());
    let paid = f.get(invoice).await;
    assert_eq!(paid["state"], "paid");
    assert_eq!(paid["payment_attempts"][0]["psp_ref"], charge["psp_ref"]);
    let charges: i64 = sqlx::query_scalar("SELECT count(*) FROM mock_psp.charges")
        .fetch_one(&f.app.db)
        .await
        .unwrap();
    assert_eq!(charges, 1);
}

#[sqlx::test]
async fn declined_invoice_reopens_and_webhooks_are_signed(pool: PgPool) {
    let f = Fixture::new(pool, false).await;
    let endpoint = f.post("/webhook-endpoints",json!({"url":format!("{}/webhooks",f.app.psp_url),"secret":"local-webhook-secret-change-me"})).await;
    assert_eq!(endpoint.status(), StatusCode::CREATED);
    let invoice = f.invoice().await;
    assert_eq!(
        f.pay(invoice, "decline-key", "tok_card_declined")
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    jobs::payment_once(&f.app).await.unwrap();
    let failed = f.get(invoice).await;
    assert_eq!(failed["state"], "open");
    assert_eq!(
        failed["payment_attempts"][0]["failure_code"],
        "card_declined"
    );
    assert_eq!(
        f.pay(invoice, "success-key", "tok_success").await.status(),
        StatusCode::ACCEPTED
    );
    jobs::payment_once(&f.app).await.unwrap();
    assert_eq!(f.get(invoice).await["state"], "paid");
    while jobs::webhook_once(&f.app).await.unwrap() {}
    let delivered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM webhook_deliveries WHERE delivered_at IS NOT NULL",
    )
    .fetch_one(&f.app.db)
    .await
    .unwrap();
    assert_eq!(delivered, 3);
}

#[sqlx::test]
async fn failed_webhooks_retry_then_exhaust(pool: PgPool) {
    let f = Fixture::new(pool, false).await;
    f.post(
        "/webhook-endpoints",
        json!({"url":format!("{}/webhooks",f.app.psp_url),"secret":"intentionally-wrong-secret"}),
    )
    .await;
    f.invoice().await;
    for expected in 1..=6 {
        assert!(jobs::webhook_once(&f.app).await.unwrap());
        let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM webhook_deliveries")
            .fetch_one(&f.app.db)
            .await
            .unwrap();
        assert_eq!(attempts, expected);
        assert!(!jobs::webhook_once(&f.app).await.unwrap());
        sqlx::query("UPDATE webhook_deliveries SET next_run=now()")
            .execute(&f.app.db)
            .await
            .unwrap();
    }
    let exhausted: bool = sqlx::query_scalar("SELECT exhausted_at IS NOT NULL AND delivered_at IS NULL AND last_status='401' FROM webhook_deliveries")
        .fetch_one(&f.app.db).await.unwrap();
    assert!(exhausted);
}

#[sqlx::test]
async fn other_business_cannot_read_or_pay_invoice(pool: PgPool) {
    let f = Fixture::new(pool, false).await;
    let invoice = f.invoice().await;
    let business = Uuid::new_v4();
    sqlx::query("INSERT INTO businesses(id,name) VALUES ($1,'Other')")
        .bind(business)
        .execute(&f.app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO api_keys(key_hash,business_id) VALUES ($1,$2)")
        .bind(invoice_service::digest("other-key"))
        .bind(business)
        .execute(&f.app.db)
        .await
        .unwrap();
    let response = f
        .http
        .get(format!("{}/invoices/{invoice}", f.url))
        .bearer_auth("other-key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = f
        .http
        .post(format!("{}/invoices/{invoice}/pay", f.url))
        .bearer_auth("other-key")
        .header("Idempotency-Key", "other-pay")
        .json(&json!({"card_token":"tok_success"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
