CREATE TABLE businesses (
    id uuid PRIMARY KEY,
    name text NOT NULL
);
CREATE TABLE api_keys (
    key_hash text PRIMARY KEY,
    business_id uuid NOT NULL REFERENCES businesses(id),
    revoked_at timestamptz
);
CREATE TABLE customers (
    id uuid PRIMARY KEY,
    business_id uuid NOT NULL REFERENCES businesses(id),
    name text NOT NULL,
    email text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (business_id, id)
);
CREATE INDEX customers_list ON customers(business_id, created_at, id);
CREATE TABLE invoices (
    id uuid PRIMARY KEY,
    business_id uuid NOT NULL REFERENCES businesses(id),
    customer_id uuid NOT NULL,
    line_items jsonb NOT NULL,
    total_cents bigint NOT NULL CHECK (total_cents > 0),
    state text NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'processing', 'paid')),
    due_date date NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (business_id, customer_id) REFERENCES customers(business_id, id),
    UNIQUE (business_id, id)
);
CREATE INDEX invoices_list ON invoices(business_id, state, created_at, id);
CREATE TABLE payment_attempts (
    id uuid PRIMARY KEY,
    business_id uuid NOT NULL,
    invoice_id uuid NOT NULL,
    idempotency_key text NOT NULL,
    request_hash text NOT NULL,
    card_token text NOT NULL,
    status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'succeeded', 'failed')),
    response jsonb NOT NULL,
    psp_ref uuid,
    failure_code text,
    retries integer NOT NULL DEFAULT 0,
    next_run timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (business_id, invoice_id) REFERENCES invoices(business_id, id),
    UNIQUE (business_id, idempotency_key)
);
CREATE UNIQUE INDEX one_pending_payment ON payment_attempts(invoice_id) WHERE status = 'pending';
CREATE INDEX payment_jobs ON payment_attempts(next_run) WHERE status = 'pending';
CREATE INDEX invoice_attempts ON payment_attempts(invoice_id, created_at);
CREATE TABLE webhook_endpoints (
    id uuid PRIMARY KEY,
    business_id uuid NOT NULL REFERENCES businesses(id),
    url text NOT NULL,
    secret text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX endpoints_business ON webhook_endpoints(business_id);
CREATE TABLE webhook_deliveries (
    id uuid PRIMARY KEY,
    endpoint_id uuid NOT NULL REFERENCES webhook_endpoints(id),
    payload text NOT NULL,
    attempts integer NOT NULL DEFAULT 0,
    next_run timestamptz NOT NULL DEFAULT now(),
    delivered_at timestamptz,
    exhausted_at timestamptz,
    last_status text
);
CREATE INDEX webhook_jobs ON webhook_deliveries(next_run)
    WHERE delivered_at IS NULL AND exhausted_at IS NULL;
-- A separate namespace represents the external processor's durable ledger.
CREATE SCHEMA mock_psp;
CREATE TABLE mock_psp.charges (
    id uuid PRIMARY KEY,
    request jsonb NOT NULL,
    response jsonb NOT NULL,
    ready_at timestamptz NOT NULL,
    calls integer NOT NULL DEFAULT 1
);
