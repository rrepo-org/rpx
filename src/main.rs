#[tokio::main]
async fn main() -> miette::Result<()> {
    rpx::run().await
}
