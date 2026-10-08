#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    invoice_service::run().await
}
