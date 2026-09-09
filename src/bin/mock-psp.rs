#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();
    let db = invoice_service::database(&std::env::var("DATABASE_URL")?).await?;
    let listener = tokio::net::TcpListener::bind(
        std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".into()),
    )
    .await?;
    axum::serve(listener, invoice_service::psp::router(db)).await?;
    Ok(())
}
