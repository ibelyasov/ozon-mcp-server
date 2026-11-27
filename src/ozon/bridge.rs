//! Internal seam shared by marketplace scenarios and their scripted tests.
use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use tokio_util::sync::CancellationToken;

pub trait PageSource: Send {
    fn seller_replies(
        &mut self,
        _review_uuid: &str,
        _sku: &str,
        _root_sku: &str,
        _cancel: &CancellationToken,
    ) -> impl Future<Output = Result<Value>> + Send {
        async { Err(crate::runtime::browser::error::BrowserError::InvalidBridgeResponse.into()) }
    }
    fn fetch_json(
        &mut self,
        path: &str,
        cancel: &CancellationToken,
    ) -> impl Future<Output = Result<Value>> + Send;
}
