//! Short-lived workers only. Unlike a general thread pool, no idle worker is
//! retained after disk or socket work completes.
pub async fn run<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    let (tx, rx) = futures_channel::oneshot::channel();
    std::thread::Builder::new()
        .name("settings-io".into())
        .spawn(move || {
            let _ = tx.send(job());
        })
        .map_err(|e| e.to_string())?;
    rx.await.map_err(|_| "Settings worker stopped".into())
}
