//! SimpleClaw binary entrypoint.

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    simpleclaw::run().await
}
