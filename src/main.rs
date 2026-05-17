#![deny(clippy::all)]

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    mailsubsystem_daemon::run().await
}
