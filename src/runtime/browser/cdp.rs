//! The standard CDP subset used by this runtime. One owned page, one in-flight RPC.
use crate::runtime::browser::error::BrowserError;
use anyhow::{Result, ensure};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};

const MAX_CDP_BYTES: usize = 12 * 1024 * 1024;
pub(super) struct Cdp {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

#[derive(Deserialize)]
struct Response {
    id: Option<u64>,
    result: Option<Value>,
    error: Option<Value>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Target {
    target_id: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Navigation {
    error_text: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Evaluation {
    result: RemoteObject,
    exception_details: Option<Value>,
}
#[derive(Deserialize)]
struct RemoteObject {
    value: Option<Value>,
    #[serde(rename = "type")]
    kind: String,
}

impl Cdp {
    pub(super) async fn connect(endpoint: &str) -> Result<Self> {
        validate_endpoint(endpoint)?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_CDP_BYTES))
            .max_frame_size(Some(MAX_CDP_BYTES));
        let (socket, _) =
            tokio_tungstenite::connect_async_with_config(endpoint, Some(config), false)
                .await
                .map_err(|_| BrowserError::DriverFailure {
                    operation: "control connection",
                })?;
        Ok(Self { socket, next_id: 1 })
    }

    async fn request(&mut self, method: &'static str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(BrowserError::InvalidBridgeResponse)?;
        let message = serde_json::to_string(&json!({"id":id,"method":method,"params":params}))?;
        ensure!(
            message.len() <= MAX_CDP_BYTES,
            BrowserError::ResponseTooLarge
        );
        self.socket
            .send(Message::Text(message.into()))
            .await
            .map_err(|_| BrowserError::DriverFailure {
                operation: "control send",
            })?;
        while let Some(message) = self.socket.next().await {
            let message = message.map_err(|_| BrowserError::DriverFailure {
                operation: "control receive",
            })?;
            let Message::Text(text) = message else {
                if matches!(message, Message::Close(_)) {
                    return Err(BrowserError::DriverFailure {
                        operation: "control receive",
                    }
                    .into());
                }
                continue;
            };
            let response: Response =
                serde_json::from_str(&text).map_err(|_| BrowserError::InvalidBridgeResponse)?;
            let Some(response_id) = response.id else {
                continue;
            };
            ensure!(response_id == id, BrowserError::InvalidBridgeResponse);
            if response.error.is_some() {
                return Err(BrowserError::CommandFailed {
                    command: method.to_owned(),
                }
                .into());
            }
            return response
                .result
                .ok_or_else(|| BrowserError::InvalidBridgeResponse.into());
        }
        Err(BrowserError::DriverFailure {
            operation: "control receive",
        }
        .into())
    }

    pub(super) async fn create_target(&mut self) -> Result<String> {
        let target: Target = serde_json::from_value(
            self.request(
                "Target.createTarget",
                json!({"url":"about:blank","background":true}),
            )
            .await?,
        )
        .map_err(|_| BrowserError::InvalidBridgeResponse)?;
        ensure!(
            !target.target_id.is_empty()
                && target.target_id.len() <= 128
                && target
                    .target_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            BrowserError::InvalidBridgeResponse
        );
        Ok(target.target_id)
    }
    pub(super) async fn enable_page(&mut self) -> Result<()> {
        self.request("Page.enable", json!({})).await?;
        Ok(())
    }
    pub(super) async fn navigate(&mut self, url: &str) -> Result<()> {
        let navigation: Navigation =
            serde_json::from_value(self.request("Page.navigate", json!({"url":url})).await?)
                .map_err(|_| BrowserError::InvalidBridgeResponse)?;
        ensure!(
            navigation.error_text.is_none(),
            BrowserError::CommandFailed {
                command: "Page.navigate".to_owned()
            }
        );
        Ok(())
    }
    pub(super) async fn evaluate(&mut self, expression: &str) -> Result<Value> {
        let evaluation: Evaluation = serde_json::from_value(
            self.request(
                "Runtime.evaluate",
                json!({"expression":expression,"awaitPromise":true,"returnByValue":true}),
            )
            .await?,
        )
        .map_err(|_| BrowserError::InvalidBridgeResponse)?;
        ensure!(
            evaluation.exception_details.is_none(),
            BrowserError::CommandFailed {
                command: "Runtime.evaluate".to_owned()
            }
        );
        if evaluation.result.kind == "undefined" {
            return Ok(Value::Null);
        }
        evaluation
            .result
            .value
            .ok_or_else(|| BrowserError::InvalidBridgeResponse.into())
    }
    pub(super) async fn close_browser(&mut self) -> Result<()> {
        self.request("Browser.close", json!({})).await?;
        Ok(())
    }
}

pub(super) fn validate_endpoint(endpoint: &str) -> Result<url::Url> {
    let url = url::Url::parse(endpoint).map_err(|_| BrowserError::InvalidBridgeResponse)?;
    let target_id = url
        .path()
        .strip_prefix("/devtools/browser/")
        .or_else(|| url.path().strip_prefix("/devtools/page/"));
    let target_valid = target_id.is_some_and(|id| {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    });
    ensure!(
        url.scheme() == "ws"
            && url.host_str() == Some("127.0.0.1")
            && url.port().is_some_and(|port| port > 0)
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && target_valid,
        BrowserError::InvalidBridgeResponse
    );
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    #[tokio::test]
    async fn narrow_cdp_owns_background_target_and_never_focuses_it() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "ws://127.0.0.1:{}/devtools/browser/test",
            listener.local_addr().unwrap().port()
        );
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut methods = Vec::new();
            for expected in [
                "Target.createTarget",
                "Page.enable",
                "Page.navigate",
                "Runtime.evaluate",
                "Browser.close",
            ] {
                let message = socket.next().await.unwrap().unwrap();
                let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                assert_eq!(value["method"], expected);
                methods.push(expected.to_owned());
                let result = match expected {
                    "Target.createTarget" => {
                        assert_eq!(value["params"]["background"], true);
                        json!({"targetId":"our-page"})
                    }
                    "Page.navigate" => {
                        assert_eq!(value["params"]["url"], "about:blank");
                        json!({"frameId":"frame"})
                    }
                    "Runtime.evaluate" => {
                        assert_eq!(value["params"]["returnByValue"], true);
                        assert_eq!(value["params"]["awaitPromise"], true);
                        json!({"result":{"type":"number","value":42}})
                    }
                    _ => json!({}),
                };
                socket
                    .send(Message::Text(
                        json!({"method":"Page.lifecycleEvent","params":{}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                socket
                    .send(Message::Text(
                        json!({"id":value["id"],"result":result}).to_string().into(),
                    ))
                    .await
                    .unwrap();
            }
            methods
        });
        let mut cdp = Cdp::connect(&endpoint).await.unwrap();
        assert_eq!(cdp.create_target().await.unwrap(), "our-page");
        cdp.enable_page().await.unwrap();
        cdp.navigate("about:blank").await.unwrap();
        assert_eq!(cdp.evaluate("21 * 2").await.unwrap(), json!(42));
        cdp.close_browser().await.unwrap();
        assert!(
            !server
                .await
                .unwrap()
                .iter()
                .any(|method| method.contains("activate") || method.contains("bringToFront"))
        );
    }
    #[test]
    fn control_endpoint_is_loopback_ws_without_credentials() {
        for endpoint in [
            "ws://example.com:9222/devtools/browser/id",
            "wss://127.0.0.1:9222/devtools/browser/id",
            "ws://user@127.0.0.1:9222/devtools/browser/id",
            "ws://127.0.0.1:9222/devtools/browser/id?x=1",
            "ws://127.0.0.1:9222/json",
            "ws://127.0.0.1:9222/devtools/browser/",
            "ws://127.0.0.1:9222/devtools/browser/id/other",
            "ws://127.0.0.1:9222/devtools/browser/id%2Fother",
        ] {
            assert!(validate_endpoint(endpoint).is_err(), "{endpoint}");
        }
        assert!(validate_endpoint("ws://127.0.0.1:9222/devtools/browser/abc").is_ok());
    }
}
