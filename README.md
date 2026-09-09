# Invoice & Payment Service

A small Rust service built with Axum and PostgreSQL. An invoice is either open, processing, or paid. Payment admission, processor reconciliation, and webhook delivery have separate transaction boundaries; [DESIGN.md](DESIGN.md) explains why.

## Run

```sh
docker compose up
```

This builds both Rust binaries, starts PostgreSQL and the mock processor, applies migrations, and seeds a local business key. The first build downloads Rust dependencies and takes a few minutes. The API listens on `http://localhost:8080`. Database contents survive restarts in a Docker volume. Use `docker compose up --build` after changing source.

The checked-in key and database password are local demo credentials. No account creation or manual database setup is required. [API.md](API.md) documents every route, response, error, and mock token.

## Try it

The following four examples use Bash, curl, and jq. Set these once in your terminal:

```sh
export API=http://localhost:8080
export KEY=local-business-key-change-me
```

### 1. Register the demo webhook and create a customer

```sh
curl -fsS "$API/webhook-endpoints" \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"url":"http://mock-psp:8081/webhooks","secret":"local-webhook-secret-change-me"}'

CUSTOMER_ID=$(curl -fsS "$API/customers" \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"name":"Mira Shah","email":"mira@example.com"}' | jq -r .id)
```

Register once before creating invoices to receive their events. Repeating registration creates another subscription.

### 2. Create an invoice

```sh
INVOICE_ID=$(curl -fsS "$API/invoices" \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d "{\"customer_id\":\"$CUSTOMER_ID\",\"due_date\":\"2026-12-31\",\"line_items\":[{\"description\":\"API integration work\",\"quantity\":2,\"unit_amount_cents\":1250}]}" \
  | jq -r .id)
```

The server computes `2500` cents. There is no client total field.

### 3. Attempt a declined payment

```sh
curl -fsS "$API/invoices/$INVOICE_ID/pay" \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -H "Idempotency-Key: decline-$INVOICE_ID" \
  -d '{"card_token":"tok_card_declined"}'

curl -fsS "$API/invoices/$INVOICE_ID" -H "Authorization: Bearer $KEY" | jq
```

The POST returns a `202` pending receipt. Repeat the GET until state is `open` and the attempt says `failed` with `card_declined` (normally within a second). Then continue.

### 4. Pay successfully with a new key

```sh
curl -fsS "$API/invoices/$INVOICE_ID/pay" \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -H "Idempotency-Key: success-$INVOICE_ID" \
  -d '{"card_token":"tok_success"}'

curl -fsS "$API/invoices/$INVOICE_ID" -H "Authorization: Bearer $KEY" | jq
```

Repeat GET until `paid`. Repeating the POST returns the same original receipt and does not call the PSP again. The GET holds the current outcome.

Watch signed deliveries with:

```sh
docker compose logs -f app mock-psp
```

The mock logs `verified demo webhook` for invoice.created, invoice.payment_failed, and invoice.paid. To explore a timeout, create another invoice and use `tok_timeout`: admission is immediate, processing persists during retries, and payment normally resolves in roughly 38 seconds. `tok_network_error` stays pending with scheduled retries because a 500 cannot prove a payment failed. No new payment is admitted until the original outcome is known.

## Tests

The integration tests run actual HTTP servers and use a separate SQLx-created database per test. They need PostgreSQL credentials with permission to create databases. None of the three required tests is skipped.

With Rust and a reachable local PostgreSQL:

```sh
DATABASE_URL=postgres://invoices:invoices@localhost:5432/invoices cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

Compose deliberately does not publish a database port. To run the suite using only Docker and the Compose database:

```sh
docker compose up -d db
docker build --target build -t invoice-service-tests .
docker run --rm --network "container:$(docker compose ps -q db)" \
  -e DATABASE_URL=postgres://invoices:invoices@127.0.0.1:5432/invoices \
  invoice-service-tests cargo test --all-targets
```

Tests cover 16 concurrent payment requests and one processor charge; exact idempotent receipt replay; the real 30-second timeout and eventual success; network-error reconciliation scheduling; a lost PSP success response; decline followed by success; signed webhook delivery and retry exhaustion; and cross-business isolation. Two unit tests cover money boundaries and signature tampering/freshness. The timeout test takes about 38 seconds after compilation. Crash recovery is simulated at the lost-response boundary, not by killing a container.

## Repository guide

- `src/api.rs`: authentication, customer/invoice routes, payment admission.
- `src/jobs.rs`: processor reconciliation, transactional events, webhook signing and delivery.
- `src/psp.rs`: durable mock processor and signature-verifying demo receiver.
- `migrations/0001_initial.sql`: business data, jobs, and the processor's separate schema.
- `tests/payments.rs`: HTTP and PostgreSQL correctness tests.
- [DESIGN.md](DESIGN.md): data model, state machine, failure modes, and deliberate omissions.
- [AI_USAGE.md](AI_USAGE.md): specific AI contribution and remaining candidate input.

## Grading coverage

The assignment says grading is split evenly across design judgment, core correctness, operational sense, and communication. The project addresses those areas here:

- Design judgment: [DESIGN.md](DESIGN.md) sections 1, 2, and 6 explain the data model, state machine, and deliberate omissions.
- Core correctness: `tests/payments.rs` covers concurrent payment admission, idempotent replay, timeout/network behavior, webhook retries, and tenant isolation.
- Operational sense: [DESIGN.md](DESIGN.md) sections 3, 4, 5, and 7 cover failure recovery, webhook retry budget, key handling, and production gaps.
- Communication: [README.md](README.md), [API.md](API.md), [DESIGN.md](DESIGN.md), and [AI_USAGE.md](AI_USAGE.md) give run commands, curl examples, API behavior, design tradeoffs, and AI disclosure.

## Demo Video

**Pending: add your accessible 5-10 minute recording link before submission.**

The assignment requires your own unscripted explanations. Record these in order:

1. Architecture, data model, and request flow (1-2 minutes).
2. Start Compose, create a customer and invoice, demonstrate successful and declined payments on separate invoices, and show webhook logs (2-3 minutes).
3. Explain the state diagram and your reasoning in your own words (1-2 minutes).
4. Explain one failure mode while showing its implementation; `payment_once` in `src/jobs.rs` handles timeout and lost-response recovery (1-2 minutes).

The code and documentation are AI-assisted. The video and three genuine independent decisions in AI_USAGE.md still need your contribution; the repository should not be submitted as complete until those are supplied.
