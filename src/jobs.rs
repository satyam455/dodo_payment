use crate::App;
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::{Postgres, Row, Transaction};
use std::time::Duration;
use uuid::Uuid;

pub async fn enqueue_event(
    tx: &mut Transaction<'_, Postgres>,
    business: Uuid,
    kind: &str,
    invoice: &Value,
) -> Result<(), sqlx::Error> {
    let payload = json!({"id": Uuid::new_v4(), "type": kind, "created_at": Utc::now(), "data": {"invoice": invoice}}).to_string();
    sqlx::query("INSERT INTO webhook_deliveries(id,endpoint_id,payload) SELECT gen_random_uuid(),id,$2 FROM webhook_endpoints WHERE business_id=$1")
        .bind(business).bind(payload).execute(&mut **tx).await?;
    Ok(())
}

pub fn signature(secret: &str, timestamp: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

pub fn verify_signature(secret: &str, timestamp: &str, body: &[u8], signature: &str) -> bool {
    let Ok(time) = timestamp.parse::<i64>() else {
        return false;
    };
    if Utc::now().timestamp().abs_diff(time) > 300 {
        return false;
    }
    let Ok(bytes) = hex::decode(signature) else {
        return false;
    };
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    mac.verify_slice(&bytes).is_ok()
}

pub async fn run(app: App) {
    let payments = async {
        loop {
            match payment_once(&app).await {
                Ok(true) => continue,
                Ok(false) => (),
                Err(error) => tracing::error!(%error, "payment worker failed"),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    let webhooks = async {
        loop {
            match webhook_once(&app).await {
                Ok(true) => continue,
                Ok(false) => (),
                Err(error) => tracing::error!(%error, "webhook worker failed"),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::join!(payments, webhooks);
}

pub async fn payment_once(app: &App) -> Result<bool, sqlx::Error> {
    // Commit the lease before network I/O; no row lock spans a PSP call.
    let job = sqlx::query(
        r#"UPDATE payment_attempts
        SET next_run = now() + interval '10 seconds', retries = retries + 1
        WHERE id = (
            SELECT id FROM payment_attempts
            WHERE status = 'pending' AND next_run <= now()
            ORDER BY next_run FOR UPDATE SKIP LOCKED LIMIT 1
        )
        RETURNING id, invoice_id, business_id, card_token, retries"#,
    )
    .fetch_optional(&app.db)
    .await?;
    let Some(job) = job else {
        return Ok(false);
    };
    let attempt: Uuid = job.get("id");
    let invoice: Uuid = job.get("invoice_id");
    let retries: i32 = job.get("retries");
    let total: i64 = sqlx::query_scalar("SELECT total_cents FROM invoices WHERE id=$1")
        .bind(invoice)
        .fetch_one(&app.db)
        .await?;
    let result = app.http.post(format!("{}/charges", app.psp_url))
        .header("Idempotency-Key", attempt.to_string())
        .json(&json!({"card_token": job.get::<String,_>("card_token"), "amount_cents": total, "currency": "USD"}))
        .send().await;
    let body = match result {
        Ok(response) if response.status().is_success() => response.json::<Value>().await.ok(),
        Ok(response) => {
            tracing::warn!(%attempt, status=%response.status(), "PSP result unknown");
            None
        }
        Err(error) => {
            tracing::warn!(%attempt, %error, "PSP result unknown");
            None
        }
    };
    // Only an explicit processor outcome can release the invoice for another try.
    let outcome = body
        .as_ref()
        .and_then(|body| match body["status"].as_str()? {
            "succeeded" => Some((
                true,
                Some(Uuid::parse_str(body["psp_ref"].as_str()?).ok()?),
                None,
            )),
            "failed" => match body["code"].as_str()? {
                "insufficient_funds" | "card_declined" => {
                    Some((false, None, Some(body["code"].as_str()?.to_owned())))
                }
                _ => None,
            },
            _ => None,
        });
    let Some((success, psp_ref, failure)) = outcome else {
        let seconds = 2_i32.pow((retries as u32).min(5)).min(30);
        sqlx::query("UPDATE payment_attempts SET next_run=now()+make_interval(secs=>$2::integer::double precision) WHERE id=$1 AND status='pending' AND retries=$3")
            .bind(attempt).bind(seconds).bind(retries).execute(&app.db).await?;
        return Ok(true);
    };
    let mut tx = app.db.begin().await?;
    sqlx::query("SELECT id FROM invoices WHERE id=$1 FOR UPDATE")
        .bind(invoice)
        .execute(&mut *tx)
        .await?;
    let changed = sqlx::query("UPDATE payment_attempts SET status=$2,psp_ref=$3,failure_code=$4 WHERE id=$1 AND status='pending'")
        .bind(attempt).bind(if success { "succeeded" } else { "failed" }).bind(psp_ref).bind(failure)
        .execute(&mut *tx).await?.rows_affected();
    if changed == 1 {
        let value: Value = sqlx::query_scalar("UPDATE invoices SET state=$2 WHERE id=$1 AND state='processing' RETURNING to_jsonb(invoices)-'business_id'")
            .bind(invoice).bind(if success { "paid" } else { "open" }).fetch_one(&mut *tx).await?;
        enqueue_event(
            &mut tx,
            job.get("business_id"),
            if success {
                "invoice.paid"
            } else {
                "invoice.payment_failed"
            },
            &value,
        )
        .await?;
        tracing::info!(%attempt, %invoice, success, "payment resolved");
    }
    tx.commit().await?;
    Ok(true)
}

pub async fn webhook_once(app: &App) -> Result<bool, sqlx::Error> {
    sqlx::query("UPDATE webhook_deliveries SET exhausted_at=now(),last_status=COALESCE(last_status,'worker interrupted') WHERE attempts>=6 AND next_run<=now() AND delivered_at IS NULL AND exhausted_at IS NULL")
        .execute(&app.db).await?;
    let row = sqlx::query(
        r#"WITH claimed AS (
            UPDATE webhook_deliveries
            SET attempts = attempts + 1, next_run = now() + interval '10 seconds'
            WHERE id = (
                SELECT id FROM webhook_deliveries
                WHERE delivered_at IS NULL AND exhausted_at IS NULL
                    AND attempts < 6 AND next_run <= now()
                ORDER BY next_run FOR UPDATE SKIP LOCKED LIMIT 1
            )
            RETURNING *
        )
        SELECT c.*, e.url, e.secret FROM claimed c
        JOIN webhook_endpoints e ON e.id = c.endpoint_id"#,
    )
    .fetch_optional(&app.db)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let id: Uuid = row.get("id");
    let attempt: i32 = row.get("attempts");
    let body: String = row.get("payload");
    let timestamp = Utc::now().timestamp().to_string();
    let sig = signature(&row.get::<String, _>("secret"), &timestamp, body.as_bytes());
    let response = app
        .http
        .post(row.get::<String, _>("url"))
        .header("Content-Type", "application/json")
        .header("Webhook-Timestamp", timestamp)
        .header("Webhook-Signature", format!("v1={sig}"))
        .body(body)
        .send()
        .await;
    let (ok, status) = match response {
        Ok(r) => (r.status().is_success(), r.status().as_u16().to_string()),
        Err(_) => (false, "transport_error".to_owned()),
    };
    let delay = [5_i32, 30, 120, 600, 1800, 0][(attempt - 1) as usize];
    sqlx::query(
        r#"UPDATE webhook_deliveries
        SET delivered_at = CASE WHEN $2 THEN now() ELSE NULL END,
            exhausted_at = CASE WHEN NOT $2 AND attempts >= 6 THEN now() ELSE NULL END,
            next_run = now() + make_interval(secs => $3::integer::double precision),
            last_status = $4
        WHERE id = $1 AND attempts = $5 AND delivered_at IS NULL"#,
    )
    .bind(id)
    .bind(ok)
    .bind(delay)
    .bind(status)
    .bind(attempt)
    .execute(&app.db)
    .await?;
    tracing::info!(%id, attempt, ok, "webhook delivery");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signature_checks_body_and_freshness() {
        let now = Utc::now().timestamp().to_string();
        let sig = signature("secret", &now, b"body");
        assert!(verify_signature("secret", &now, b"body", &sig));
        assert!(!verify_signature("secret", &now, b"changed", &sig));
        assert!(!verify_signature("wrong", &now, b"body", &sig));
        assert!(!verify_signature(
            "secret",
            "0",
            b"body",
            &signature("secret", "0", b"body")
        ));
    }
}
