#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fastsearch_ingest_worker::run_from_env().await
}
