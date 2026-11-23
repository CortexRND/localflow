//! Cancellable HTTP operations driven on a caller-owned worker thread.
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err("Operation cancelled.".into())
    } else {
        Ok(())
    }
}

pub fn run<T>(
    cancel: &AtomicBool,
    operation: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    check_cancel(cancel)?;
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?.block_on(async {
        tokio::select! {
            biased;
            _ = async { loop { if cancel.load(Ordering::Acquire) { break; } tokio::time::sleep(Duration::from_millis(25)).await; } } => Err("Operation cancelled.".into()),
            result = operation => { check_cancel(cancel)?; result }
        }
    })
}

pub fn client(endpoint: &str) -> Result<reqwest::Client, String> {
    let url = reqwest::Url::parse(endpoint).map_err(|e| e.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Expected an HTTP(S) endpoint.".into());
    }
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30));
    if matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost")) {
        builder = builder.no_proxy();
    }
    builder.build().map_err(|e| error(&e))
}

pub fn error(error: &(dyn std::error::Error + 'static)) -> String {
    crate::http::error_text(error)
}
pub async fn pause() {
    tokio::time::sleep(Duration::from_millis(25)).await;
}
/// Yield once to the runtime. Only meaningful inside the top-level future of
/// [`run`]: its current-thread `block_on` parks on the I/O/signal driver and
/// then wakes deferred tasks between root polls, so a signal that arrived
/// during blocking work wakes its listener before the caller resumes and
/// commits the next step. Tokio's `yield_now` makes no such guarantee for
/// other runtimes or for callers nested in arbitrary combinators.
pub async fn checkpoint() {
    tokio::task::yield_now().await;
}
