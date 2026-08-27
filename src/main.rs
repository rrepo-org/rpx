fn main() -> miette::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Tokio runtime should initialize");
    let outcome = runtime.block_on(rpx::run())?;
    drop(runtime);
    outcome.finish()
}
