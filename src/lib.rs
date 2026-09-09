pub mod api;
pub mod jobs;
pub mod psp;

use reqwest::Client;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone)]
pub struct App {
    pub db: PgPool,
    pub http: Client,
    pub psp_url: String,
}

pub fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

pub async fn database(url: &str) -> Result<PgPool, sqlx::Error> {
    let db = PgPoolOptions::new()
        .max_connections(12)
        .connect(url)
        .await?;
    sqlx::migrate!("./migrations").run(&db).await?;
    Ok(db)
}

impl App {
    pub fn new(db: PgPool, psp_url: String) -> Self {
        Self {
            db,
            http: Client::builder()
                .timeout(Duration::from_secs(2))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("HTTP client"),
            psp_url,
        }
    }
}

pub async fn bootstrap(db: &PgPool, key: &str) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    let id = Uuid::from_u128(1);
    sqlx::query(
        "INSERT INTO businesses(id, name) VALUES ($1, 'Local business') ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO api_keys(key_hash, business_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(digest(key))
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/*
Line-by-line explanation of lib.rs:

1. `pub mod api;`
   This exposes the `api` module from `src/api.rs`.
   That file contains the public HTTP routes like:
   - create customer,
   - create invoice,
   - pay invoice,
   - register webhook endpoint.

2. `pub mod jobs;`
   This exposes the `jobs` module from `src/jobs.rs`.
   That file contains the background workers:
   - payment reconciliation worker,
   - webhook delivery worker.

3. `pub mod psp;`
   This exposes the `psp` module from `src/psp.rs`.
   That file contains the mock payment processor and the demo webhook receiver.

4. `use reqwest::Client;`
   Imports the reqwest HTTP client.
   The app uses this client to call the mock PSP and to send webhooks.

5. `use sha2::{Digest, Sha256};`
   Imports SHA-256 hashing tools.
   SHA-256 is used to hash API keys and payment request fingerprints.

6. `use sqlx::{PgPool, postgres::PgPoolOptions};`
   Imports SQLx Postgres types.
   `PgPool` is a reusable pool of database connections.
   `PgPoolOptions` configures and creates that pool.

7. `use std::time::Duration;`
   Imports `Duration`, used to configure the HTTP timeout.

8. `use uuid::Uuid;`
   Imports UUID support.
   The project uses UUIDs for businesses, customers, invoices, payment attempts,
   webhooks, and PSP references.

9. `#[derive(Clone)]`
   Makes `App` cloneable.
   This matters because the same `App` state is shared between the API server and
   background workers.

10. `pub struct App`
    This is the shared application state.
    Instead of passing database pools and URLs separately everywhere, handlers and
    workers receive one `App`.

11. `pub db: PgPool`
    This is the Postgres connection pool.
    A pool lets many requests reuse database connections efficiently.

12. `pub http: Client`
    This is the shared reqwest HTTP client.
    It is used for outgoing HTTP calls, mainly:
    - calling the PSP `/charges` endpoint,
    - sending webhook deliveries.

13. `pub psp_url: String`
    This stores the base URL of the mock payment processor.
    In Docker Compose, it is normally `http://mock-psp:8081`.

14. `pub fn digest(value: &str) -> String`
    This helper hashes a string with SHA-256 and returns the result as hex text.
    It is used for API keys and idempotency request hashes.

15. `hex::encode(Sha256::digest(value.as_bytes()))`
    Converts the input string into bytes, hashes it with SHA-256, and converts the
    binary hash into readable hex format.

16. `pub async fn database(url: &str) -> Result<PgPool, sqlx::Error>`
    This function connects to Postgres and returns a connection pool.
    It is async because database connection work uses network I/O.

17. `PgPoolOptions::new().max_connections(12)`
    Creates database pool settings.
    `max_connections(12)` means the app can open up to 12 database connections in
    this pool.

18. `.connect(url).await?`
    Connects to Postgres using the provided database URL.
    `.await` waits for the async connection.
    `?` returns the error if connection fails.

19. `sqlx::migrate!("./migrations").run(&db).await?;`
    Runs database migrations from the `migrations/` folder.
    This creates tables like `customers`, `invoices`, `payment_attempts`, and
    `webhook_deliveries` if they are not already applied.

20. `Ok(db)`
    Returns the ready-to-use database pool.

21. `impl App`
    This block defines functions attached to `App`.

22. `pub fn new(db: PgPool, psp_url: String) -> Self`
    Constructor for `App`.
    It receives the database pool and PSP URL, creates the HTTP client, and returns
    the full shared app state.

23. `Client::builder()`
    Starts building a configured reqwest HTTP client.

24. `.timeout(Duration::from_secs(2))`
    Sets a two-second timeout for outgoing HTTP calls.
    This is important for the timeout failure mode: `tok_timeout` makes the PSP
    wait longer than the client timeout, so the worker treats the result as unknown
    and retries later.

25. `.redirect(reqwest::redirect::Policy::none())`
    Disables HTTP redirects.
    This is safer for webhook delivery because the service should not silently
    follow an endpoint to a different URL.

26. `.build().expect("HTTP client")`
    Builds the HTTP client.
    `expect` is acceptable here because failing to build the client is a startup
    programming/configuration failure.

27. `pub async fn bootstrap(db: &PgPool, key: &str) -> Result<(), sqlx::Error>`
    Seeds local demo data.
    It receives the database pool and the raw bootstrap API key.

28. `let mut tx = db.begin().await?;`
    Starts a database transaction.
    Both the business row and API key row should be created together.

29. `let id = Uuid::from_u128(1);`
    Creates a fixed UUID for the local demo business.
    This keeps local setup deterministic.

30. `INSERT INTO businesses ... ON CONFLICT DO NOTHING`
    Inserts the local business if it does not already exist.
    If it already exists, the query does nothing.

31. `.bind(id)`
    Safely passes the UUID value into the SQL query.
    Binding avoids manually building SQL strings.

32. `INSERT INTO api_keys ...`
    Inserts the API key hash for the local business.
    The raw key is not stored; only `digest(key)` is stored.

33. `.bind(digest(key))`
    Hashes the bootstrap API key before saving it.
    Later, authentication hashes the incoming bearer key and compares hashes.

34. `tx.commit().await`
    Commits the transaction.
    After this, the local business and API key are durable in the database.

Simple summary:

`lib.rs` exposes the main modules, defines shared app state, creates the database
pool, creates the HTTP client, hashes sensitive values, and seeds the local demo
business/API key.
*/
