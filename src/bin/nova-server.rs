#[tokio::main]
async fn main() -> windows::core::Result<()> {
    nova_server::run().await
}
