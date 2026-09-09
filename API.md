# API

Base URL: `http://localhost:8080`. JSON requests use `Content-Type: application/json` and `Authorization: Bearer local-business-key-change-me`. Every route is scoped to that key's business. All money is integer USD cents. Unknown JSON fields are rejected. The default JSON body limit is 2 MiB. UUIDs are strings; dates use `YYYY-MM-DD`, timestamps use RFC 3339.

## Errors

```json
{"error":{"code":"invalid_invoice_state","message":"Only an open invoice can start a payment"}}
```

| HTTP status | Codes / meaning |
| --- | --- |
| 400 | `invalid_request`: invalid values, path ID, query, or malformed JSON |
| 401 | `unauthorized`: missing, invalid, or revoked key |
| 404 | `not_found`: absent resource, another business's resource, or unknown route |
| 405 | `method_not_allowed` |
| 409 | `invalid_invoice_state` or `idempotency_key_reused` |
| 413 / 415 / 422 | `invalid_request`: oversized body / wrong content type / JSON shape or type mismatch |
| 500 | `internal_error`: database failure; no database details exposed |

## Customers

`POST /customers` -> **201**

```json
{"name":"Mira Shah","email":"mira@example.com"}
```

Name: 1-200 bytes after rejecting whitespace-only values. Email: up to 320 bytes, nonempty and containing `@` (basic validation, not deliverability validation).

Response:

```json
{"id":"<uuid>","name":"Mira Shah","email":"mira@example.com","created_at":"<timestamp>"}
```

`GET /customers/{id}` -> **200**, same shape.

`GET /customers?limit=50&offset=0` -> **200**, `{"data":[<customer>, ...]}`. Limit 1-100 (default 50); offset nonnegative (default 0). Ordered by creation time, then ID. No state filter.

## Invoices

`POST /invoices` -> **201**

```json
{
  "customer_id":"<uuid>",
  "due_date":"2026-12-31",
  "line_items":[{"description":"API implementation","quantity":2,"unit_amount_cents":1250}]
}
```

Provide 1-100 items. Description: nonblank, up to 500 bytes. Quantity: positive signed 64-bit integer. Unit amount: nonnegative signed 64-bit integer. Total must be positive and fit signed 64-bit arithmetic. Past due dates are permitted. Client-supplied total, currency, and state fields are rejected.

Response (the invoice shape):

```json
{
  "id":"<uuid>",
  "customer_id":"<uuid>",
  "line_items":[{"description":"API implementation","quantity":2,"unit_amount_cents":1250}],
  "total_cents":2500,
  "state":"open",
  "due_date":"2026-12-31",
  "created_at":"<timestamp>"
}
```

`GET /invoices?state=open&limit=50&offset=0` -> **200**, `{"data":[<invoice>, ...]}`. State optional: `open`, `processing`, `paid`. Pagination matches customers. Lists omit attempts.

`GET /invoices/{id}` -> **200**, invoice shape plus:

```json
{
  "payment_attempts":[
    {"id":"<uuid>","status":"succeeded","psp_ref":"<uuid>","failure_code":null,"created_at":"<timestamp>"}
  ]
}
```

Attempts are ordered by creation time and ID. Status: `pending`, `succeeded`, or `failed`. `psp_ref` is null until success; `failure_code` is null except confirmed declines (`card_declined`, `insufficient_funds`). Invoice and attempts use one database snapshot.

## Payments

`POST /invoices/{id}/pay` with `Idempotency-Key: <1-200 byte nonblank key>` -> **202**

```json
{"card_token":"tok_success"}
```

Response:

```json
{"attempt_id":"<uuid>","invoice_id":"<uuid>","status":"pending","status_url":"/invoices/<uuid>"}
```

Poll `status_url` using the same API authentication, or receive webhooks. Same business/key/invoice/token always replays the original 202 body, even after completion. It is an admission receipt, not a live result. Different valid input with a saved key returns 409. Keys apply across all invoices within one business and do not expire. Invalid or rejected requests are not reserved.

A fresh key on a paid or processing invoice returns 409. After a confirmed decline, use a fresh key to try another token.

| Token | Processor behavior | Invoice behavior |
| --- | --- | --- |
| `tok_success` | Success after about 100 ms | processing -> paid |
| `tok_insufficient_funds` | Failed after about 100 ms | processing -> open |
| `tok_card_declined` | Failed after about 100 ms | processing -> open |
| `tok_timeout` | Success available after 30 seconds | Pending until worker recovers result |
| `tok_network_error` | HTTP 500 on every call | Pending; periodically retries, never automatically reopens |

## Webhook endpoints

`POST /webhook-endpoints` -> **201**

```json
{"url":"http://mock-psp:8081/webhooks","secret":"local-webhook-secret-change-me"}
```

Response: `{"id":"<uuid>","url":"http://mock-psp:8081/webhooks"}`. URL must be HTTP(S), up to 2,048 bytes, without credentials or fragment. Secret: 16-256 bytes; choose a random secret outside the demo. It is not returned. Only endpoints registered before an event receive it.

Webhook payload:

```json
{"id":"<event-uuid>","type":"invoice.paid","created_at":"<timestamp>","data":{"invoice":{ "id":"<uuid>","customer_id":"<uuid>","line_items":[],"total_cents":2500,"state":"paid","due_date":"2026-12-31","created_at":"<timestamp>"}}}
```

The embedded invoice has the full invoice shape; the empty items above abbreviate it. Types: `invoice.created`, `invoice.paid`, `invoice.payment_failed`. Headers: `Webhook-Timestamp: <unix-seconds>`, `Webhook-Signature: v1=<hex-HMAC-SHA256>`. Verify HMAC over timestamp, a literal period, and unmodified body bytes. Reject timestamps outside +/-300 seconds and deduplicate the event ID durably. Return 2xx after saving the event. Retries and ordering limits are in DESIGN.md.

## Internal mock processor

`POST http://mock-psp:8081/charges`, `Idempotency-Key: <attempt UUID>`, body:

```json
{"card_token":"tok_success","amount_cents":2500,"currency":"USD"}
```

Response **200**: `{"status":"succeeded","psp_ref":"<uuid>"}` or `{"status":"failed","code":"card_declined"}` / `insufficient_funds`. `tok_network_error` returns **500** with the error envelope. Invalid parameters return 400; changed input for an existing key returns 409. Replays preserve the original result and reference. Timeout replays wait only until the original readiness time.

The mock also exposes `POST /webhooks` as a signature-verifying demo sink (204 on valid signature, 401 otherwise). Neither internal route uses business authentication; the mock is not published on a host port.
