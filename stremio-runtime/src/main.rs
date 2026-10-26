#[tokio::main(worker_threads = 32)]
async fn main() -> anyhow::Result<()> {
    stremio_runtime::run_standalone().await
}
