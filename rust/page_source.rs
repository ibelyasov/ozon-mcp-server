//! Internal seam shared by marketplace scenarios and their scripted tests.
use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use tokio_util::sync::CancellationToken;

pub trait PageSource: Send {
    fn fetch_json(
        &mut self,
        path: &str,
        cancel: &CancellationToken,
    ) -> impl Future<Output = Result<Value>> + Send;

    /// Confirm cleanup before allowing another request to use this source.
    fn shutdown(&mut self) -> impl Future<Output = Result<()>> + Send;
}
