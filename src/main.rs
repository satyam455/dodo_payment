use invoice_service::{App, api, bootstrap, database, jobs};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let db = database(&std::env::var("DATABASE_URL")?).await?;
    bootstrap(&db, &std::env::var("BOOTSTRAP_API_KEY")?).await?;
    let app = App::new(db, std::env::var("PSP_URL")?);
    tokio::spawn(jobs::run(app.clone()));
    let listener = tokio::net::TcpListener::bind(
        std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
    )
    .await?;
    axum::serve(listener, api::router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/*
Line-by-line explanation of main.rs:

1. `use invoice_service::{App, api, bootstrap, database, jobs};`
   This imports the main pieces from our own crate.
   - `App` is the shared application state.
   - `api` contains all HTTP routes.
   - `bootstrap` seeds the local demo business and API key.
   - `database` connects to Postgres and runs migrations.
   - `jobs` contains background workers for payments and webhooks.

2. `#[tokio::main]`
   This starts the Tokio async runtime.
   In Rust, normal `main` cannot directly use `.await`.
   This macro creates the runtime so this app can run async database queries,
   async HTTP calls, background workers, and the Axum web server.

3. `async fn main() -> Result<(), Box<dyn std::error::Error>>`
   This means `main` is asynchronous and can return errors.
   `Box<dyn std::error::Error>` is a flexible error type, so different startup
   errors can be returned using `?`.

4. `tracing_subscriber::fmt()`
   This configures logging output.
   The project uses the `tracing` crate for logs, for example:
   `tracing::info!`, `tracing::warn!`, and `tracing::error!`.
   A "subscriber" receives those tracing events and prints them.

5. `.with_env_filter(tracing_subscriber::EnvFilter::from_default_env())`
   This reads log filtering from environment variables like `RUST_LOG`.
   In Docker Compose, `RUST_LOG=invoice_service=info`, so logs from this service
   are printed at info level.

6. `.init();`
   This activates the tracing subscriber.
   After this line, tracing logs will actually appear in the terminal.

7. `let db = database(&std::env::var("DATABASE_URL")?).await?;`
   This reads `DATABASE_URL` from the environment.
   Then it calls `database(...)`, which connects to Postgres, creates a database
   connection pool, and runs the SQL migrations.
   The first `?` handles a missing environment variable.
   `.await` waits for the async database setup.
   The second `?` returns if database setup fails.

8. `bootstrap(&db, &std::env::var("BOOTSTRAP_API_KEY")?).await?;`
   This reads the local demo API key from `BOOTSTRAP_API_KEY`.
   `bootstrap` inserts the fixed local business and stores the SHA-256 hash of
   the API key if they do not already exist.
   This is why the demo key works immediately after `docker compose up`.

9. `let app = App::new(db, std::env::var("PSP_URL")?);`
   This creates shared application state.
   `App` stores:
   - the Postgres connection pool,
   - the reqwest HTTP client,
   - the PSP URL.
   `PSP_URL` points to the mock payment processor, usually
   `http://mock-psp:8081` inside Docker Compose.

10. `tokio::spawn(jobs::run(app.clone()));`
    This starts the background jobs in a separate async task.
    `jobs::run` loops forever and runs:
    - payment reconciliation,
    - webhook delivery.
    `tokio::spawn` means this work runs in the background while the API server
    continues accepting HTTP requests.
    `app.clone()` is cheap because the database pool and HTTP client are shared
    handles internally.

11. `tokio::net::TcpListener::bind(...)`
    This opens a TCP socket for incoming connections.
    It reads `LISTEN_ADDR` from the environment.
    If `LISTEN_ADDR` is not set, it uses `0.0.0.0:8080`.
    `0.0.0.0` means listen on all network interfaces inside the container.
    Port `8080` is the API port.

12. `.await?;` after `TcpListener::bind`
    Binding the port is async, so we wait for it.
    If the port is already in use or cannot be opened, `?` returns the error.

13. `axum::serve(listener, api::router(app))`
    This starts the Axum HTTP server.
    `listener` is the socket receiving connections.
    `api::router(app)` builds the route table from `src/api.rs`.
    For example, it maps:
    - `POST /customers`
    - `POST /invoices`
    - `POST /invoices/{id}/pay`
    to their handler functions.

14. `.with_graceful_shutdown(async { ... })`
    This tells Axum how to shut down cleanly.
    The server waits for a shutdown signal instead of just exiting suddenly.

15. `let _ = tokio::signal::ctrl_c().await;`
    This waits until the user presses Ctrl+C.
    `let _ =` means we ignore the exact result; we only care that the signal
    happened.

16. `.await?;` after `axum::serve`
    This runs the web server.
    The server keeps running here until shutdown or an error happens.

17. `Ok(())`
    This means the program finished without an unhandled error.

Simple summary:

`main.rs` starts logging, connects to the database, seeds the demo API key,
creates shared app state, starts background workers, opens port 8080, and then
serves the Axum API routes.
*/
