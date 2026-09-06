use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use futures_util::SinkExt;
use serde_json::{Value, json};
use std::{
    fs::File,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

const DRIVER_VERSION: &str = "agent-browser 0.36.0";
const MAX_CLI_BYTES: usize = 12 * 1024 * 1024;
const HOME: &str = "https://www.ozon.ru/";

pub struct Browser {
    binary: PathBuf,
    profile: PathBuf,
    executable: Option<String>,
    headed: bool,
    city: String,
    runtime: tempfile::TempDir,
    _profile_lock: File,
    user_agent: Option<String>,
    cdp: Option<String>,
    ready: bool,
    started: bool,
    poisoned: bool,
    last_used: Instant,
}

impl Browser {
    pub async fn from_env() -> Result<Self> {
        let binary =
            std::env::var_os("OZON_AGENT_BROWSER_BIN").unwrap_or_else(|| "agent-browser".into());
        let binary = if PathBuf::from(&binary).components().count() > 1 {
            std::fs::canonicalize(binary).context("OZON_AGENT_BROWSER_BIN does not exist")?
        } else {
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|p| p.join(&binary))
                .find(|p| p.is_file())
                .context("agent-browser is missing; install 0.36.0 or set OZON_AGENT_BROWSER_BIN")?
        };
        let profile = std::env::var_os("OZON_USER_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                    .join(".ozon-mcp-rust-profile")
            });
        let mut directories = std::fs::DirBuilder::new();
        directories.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directories.mode(0o700);
        }
        directories
            .create(&profile)
            .context("Cannot create Ozon profile directory")?;
        let profile = std::fs::canonicalize(profile)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(profile.join(".ozon-mcp.lock"))?;
        lock.try_lock_exclusive()
            .context("Ozon profile is already owned by another MCP process")?;
        // A short private path is necessary for macOS Unix socket length limits.
        let runtime = tempfile::Builder::new()
            .prefix("ozon-")
            .tempdir_in(std::env::temp_dir())?;
        std::fs::write(runtime.path().join("config.json"), "{}")?;
        let browser = Self {
            binary,
            profile,
            runtime,
            _profile_lock: lock,
            executable: std::env::var("OZON_BROWSER_EXECUTABLE").ok(),
            headed: std::env::var("OZON_HEADLESS")
                .is_ok_and(|v| v.trim().eq_ignore_ascii_case("false")),
            city: std::env::var("OZON_CITY")
                .unwrap_or_default()
                .trim()
                .to_owned(),
            user_agent: None,
            cdp: None,
            ready: false,
            started: false,
            poisoned: false,
            last_used: Instant::now(),
        };
        let output = browser
            .command()
            .arg("--version")
            .output()
            .await
            .context("Cannot execute agent-browser")?;
        ensure!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim() == DRIVER_VERSION,
            "Expected agent-browser 0.36.0; set OZON_AGENT_BROWSER_BIN to the pinned binary"
        );
        if std::env::var_os("OZON_HIDE_WINDOW").is_some() {
            eprintln!(
                "OZON_HIDE_WINDOW is ignored; use OZON_HEADLESS=false for explicit visible mode"
            );
        }
        Ok(browser)
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.env_clear();
        for key in [
            "PATH",
            "HOME",
            "USER",
            "TMPDIR",
            "TEMP",
            "SystemRoot",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "XDG_RUNTIME_DIR",
            "DBUS_SESSION_BUS_ADDRESS",
        ] {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        cmd.current_dir(self.runtime.path())
            .env("AGENT_BROWSER_SOCKET_DIR", self.runtime.path())
            .env("AGENT_BROWSER_PLUGINS", "[]")
            .env("AGENT_BROWSER_DEFAULT_TIMEOUT", "40000")
            .env("LANG", "ru_RU.UTF-8")
            .args(["--config", self.runtime.path().join("config.json").to_str().unwrap()])
            .args(["--session", "ozon", "--profile", self.profile.to_str().unwrap()])
            .args(["--headed", if self.headed { "true" } else { "false" }, "--no-webmcp", "--idle-timeout", "10m"])
            .args(["--args", "--disable-blink-features=AutomationControlled,--mute-audio,--lang=ru-RU,--no-first-run,--no-default-browser-check,--disable-extensions,--disable-background-networking"])
            .arg("--json");
        if let Some(executable) = &self.executable {
            cmd.args(["--executable-path", executable]);
        }
        if let Some(ua) = &self.user_agent {
            cmd.args(["--user-agent", ua]);
        }
        cmd.kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    async fn raw(
        &self,
        args: &[&str],
        script: Option<&str>,
        cancel: &CancellationToken,
        deadline: Duration,
    ) -> Result<Value> {
        ensure!(!cancel.is_cancelled(), "Request cancelled");
        let mut cmd = self.command();
        cmd.args(args);
        if script.is_some() {
            cmd.stdin(Stdio::piped());
        }
        let mut child = cmd.spawn().context("Cannot start agent-browser command")?;
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let mut stdin = child.stdin.take();
        let work = async {
            let write = async {
                if let (Some(mut input), Some(text)) = (stdin.take(), script) {
                    input.write_all(text.as_bytes()).await?;
                    input.shutdown().await?;
                }
                Ok::<_, std::io::Error>(())
            };
            let read = async {
                let mut bytes = Vec::new();
                (&mut stdout)
                    .take((MAX_CLI_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
                    .await?;
                ensure!(
                    bytes.len() <= MAX_CLI_BYTES,
                    "agent-browser response exceeds size limit"
                );
                Ok::<_, anyhow::Error>(bytes)
            };
            let errors = async {
                // Drain without retaining page data or unbounded diagnostics.
                tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await?;
                Ok::<_, anyhow::Error>(())
            };
            let ((), bytes, ()) = tokio::try_join!(
                async { write.await.map_err(anyhow::Error::from) },
                read,
                errors
            )?;
            let status = child.wait().await?;
            let value: Value =
                serde_json::from_slice(&bytes).context("Invalid agent-browser JSON response")?;
            // Never return arbitrary CLI stderr or a raw page response as an error.
            ensure!(
                status.success() && value["success"] == true,
                "BROWSER_COMMAND_FAILED: agent-browser {} failed",
                args[0]
            );
            Ok(value["data"].clone())
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(anyhow::anyhow!("Request cancelled")),
            _ = tokio::time::sleep(deadline) => Err(anyhow::anyhow!("BROWSER_TIMEOUT: agent-browser command timed out")),
            result = work => result,
        };
        if result.is_err() {
            let _ = child.kill().await;
        }
        result
    }

    async fn run(&self, args: &[&str], cancel: &CancellationToken) -> Result<Value> {
        self.raw(args, None, cancel, Duration::from_secs(45)).await
    }

    async fn evaluate(&self, script: &str, cancel: &CancellationToken) -> Result<Value> {
        Ok(self
            .raw(
                &["eval", "--stdin"],
                Some(script),
                cancel,
                Duration::from_secs(40),
            )
            .await?["result"]
            .clone())
    }

    async fn remember_cdp(&mut self, cancel: &CancellationToken) -> Result<()> {
        let data = self
            .raw(&["get", "cdp-url"], None, cancel, Duration::from_secs(5))
            .await?;
        let endpoint = data["url"]
            .as_str()
            .or_else(|| data["value"].as_str())
            .or_else(|| data["cdpUrl"].as_str())
            .context("agent-browser did not return its CDP endpoint")?;
        let url = url::Url::parse(endpoint)?;
        ensure!(
            url.scheme() == "ws"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
                && url.path().starts_with("/devtools/browser/")
                && url.username().is_empty()
                && url.password().is_none(),
            "Expected a local private browser CDP endpoint"
        );
        self.cdp = Some(endpoint.to_owned());
        Ok(())
    }

    async fn launch(&mut self, cancel: &CancellationToken) -> Result<()> {
        ensure!(!cancel.is_cancelled(), "Request cancelled");
        self.started = true;
        // Acquire the private browser handle before honoring cancellation. Killing
        // the CLI halfway through startup can orphan its detached daemon before
        // we know which CDP endpoint to close. The acquisition itself is bounded.
        let acquiring = CancellationToken::new();
        self.raw(
            &["open", "about:blank"],
            None,
            &acquiring,
            Duration::from_secs(15),
        )
        .await?;
        self.remember_cdp(&acquiring).await?;
        ensure!(!cancel.is_cancelled(), "Request cancelled");
        Ok(())
    }

    async fn ensure_ready(&mut self, cancel: &CancellationToken) -> Result<()> {
        ensure!(
            !self.poisoned,
            "BROWSER_CLEANUP_FAILED: previous session could not be closed; restart after closing its browser"
        );
        if self.ready && self.last_used.elapsed() < Duration::from_secs(590) {
            return Ok(());
        }
        if self.started {
            self.shutdown().await?;
        }
        self.launch(cancel).await?;
        if !self.headed && self.user_agent.is_none() {
            let ua = self.evaluate("navigator.userAgent", cancel).await?;
            self.shutdown().await?;
            self.user_agent = Some(
                ua.as_str()
                    .context("Missing browser User-Agent")?
                    .replace("HeadlessChrome/", "Chrome/"),
            );
            self.launch(cancel).await?;
        }
        self.run(&["set", "viewport", "1920", "1080"], cancel)
            .await?;
        self.run(&["open", HOME], cancel).await?;
        self.run(&["wait", "12000"], cancel).await?;
        if !self.city.is_empty() {
            self.try_set_city(cancel).await?;
        }
        self.ready = true;
        self.last_used = Instant::now();
        Ok(())
    }

    async fn try_set_city(&self, cancel: &CancellationToken) -> Result<()> {
        let action = async {
            self.run(&["find", "first", "[data-widget*=locationSelector i], [data-widget*=region i], button[aria-label*=ород]", "click"], cancel).await?;
            self.run(
                &[
                    "find",
                    "first",
                    "[role=dialog] input[type=text], [role=dialog] input[placeholder*=ород i]",
                    "fill",
                    &self.city,
                ],
                cancel,
            )
            .await?;
            self.run(&["wait", "1500"], cancel).await?;
            self.run(
                &[
                    "find",
                    "first",
                    "[role=dialog] [role=option], [role=dialog] li, [role=dialog] [data-suggest]",
                    "click",
                ],
                cancel,
            )
            .await?;
            self.run(&["wait", "2500"], cancel).await?;
            Ok::<_, anyhow::Error>(())
        };
        // Never return prices from an unconfirmed requested region after UI failure.
        match tokio::time::timeout(Duration::from_secs(10), action).await {
            Ok(Ok(())) => {
                eprintln!("OZON_CITY selection completed; verify the saved region if prices matter")
            }
            _ if cancel.is_cancelled() => bail!("Request cancelled"),
            _ => bail!(
                "REGION_SELECTION_FAILED: set the region manually in the profile and unset OZON_CITY"
            ),
        }
        Ok(())
    }

    pub async fn fetch_json(&mut self, path: &str, cancel: &CancellationToken) -> Result<Value> {
        let result = self.fetch_once(path, cancel).await;
        if result
            .as_ref()
            .is_err_and(|e| e.to_string().starts_with("BROWSER_COMMAND_FAILED:"))
            && !cancel.is_cancelled()
        {
            // Read-only request: one fresh-session retry for driver failures only.
            // HTTP errors and challenges are returned without a restart loop.
            self.shutdown().await?;
            return self.fetch_once(path, cancel).await;
        }
        result
    }

    async fn fetch_once(&mut self, path: &str, cancel: &CancellationToken) -> Result<Value> {
        let target = url::Url::parse(HOME)?.join(path)?;
        ensure!(
            target.origin() == url::Url::parse(HOME)?.origin(),
            "Invalid Ozon origin"
        );
        self.ensure_ready(cancel).await?;
        let script = format!(
            "({})({})",
            include_str!("page.js"),
            json!({"mode":"fetch", "path":path})
        );
        let response = self.evaluate(&script, cancel).await?;
        self.last_used = Instant::now();
        if let Some(page) = response.get("page") {
            return Ok(page.clone());
        }
        if response["error"] == "RESPONSE_TOO_LARGE" {
            bail!("RESPONSE_TOO_LARGE: Ozon response exceeds 4 MiB");
        }
        // A direct page can still work when the composer endpoint or warm-up is blocked.
        let status = response["status"].as_u64();
        if status.is_some_and(|s| !matches!(s, 403 | 307)) {
            bail!("Ozon returned HTTP {}", status.unwrap());
        }
        self.run(&["open", target.as_str()], cancel).await?;
        self.run(&["wait", "1500"], cancel).await?;
        let navigation = self.evaluate("({url:location.href,status:performance.getEntriesByType('navigation')[0]?.responseStatus})", cancel).await?;
        let status = navigation["status"]
            .as_u64()
            .context("Ozon navigation HTTP status is unavailable")?;
        ensure!(
            (200..400).contains(&status),
            "Ozon page returned HTTP {status}"
        );
        ensure!(
            is_requested_page(
                target.as_str(),
                navigation["url"].as_str().unwrap_or_default()
            ),
            "Ozon redirected to a different page; requested data is unavailable"
        );
        let script = format!(
            "({})({})",
            include_str!("page.js"),
            json!({"mode":"widgets"})
        );
        let response = self.evaluate(&script, cancel).await?;
        self.last_used = Instant::now();
        if let Some(page) = response.get("page") {
            return Ok(page.clone());
        }
        if response["error"] == "RESPONSE_TOO_LARGE" {
            bail!("RESPONSE_TOO_LARGE: Ozon response exceeds 4 MiB");
        }
        bail!("CAPTCHA_OR_BLOCKED: Ozon did not provide public product data")
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        if !self.started {
            return Ok(());
        }
        self.ready = false;
        // agent-browser serializes commands, so `close` alone cannot interrupt eval.
        // This single standard CDP command only closes the private browser captured above.
        if let Some(endpoint) = self.cdp.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), async {
                let (mut ws, _) = tokio_tungstenite::connect_async(endpoint).await?;
                ws.send(tokio_tungstenite::tungstenite::Message::Text(
                    json!({"id":1,"method":"Browser.close"}).to_string().into(),
                ))
                .await?;
                Ok::<_, anyhow::Error>(())
            })
            .await;
        }
        let result = self
            .raw(
                &["close"],
                None,
                &CancellationToken::new(),
                Duration::from_secs(7),
            )
            .await;
        self.poisoned = result.is_err();
        if self.poisoned {
            bail!("BROWSER_CLEANUP_FAILED: could not confirm private browser shutdown");
        }
        self.started = false;
        Ok(())
    }
}

fn is_requested_page(requested: &str, actual: &str) -> bool {
    let (Ok(target), Ok(final_url)) = (url::Url::parse(requested), url::Url::parse(actual)) else {
        return false;
    };
    if target.origin() != final_url.origin() {
        return false;
    }
    if target.path() == "/search/" || target.path().starts_with("/category/") {
        if has_duplicate_query_key(&target, "sorting")
            || has_duplicate_query_key(&final_url, "sorting")
        {
            return false;
        }
        let predicted_category = target.path() == "/search/"
            && final_url.path().starts_with("/category/")
            && final_url
                .query_pairs()
                .any(|(key, value)| key == "category_was_predicted" && value == "true");
        let allowed_path = if target.path() == "/search/" {
            final_url.path() == "/search/" || predicted_category
        } else {
            target.path() == final_url.path()
        };
        let mut requested_query = semantic_query(&target);
        let actual_query = semantic_query(&final_url);
        if allowed_path && requested_query == actual_query {
            return true;
        }

        // Ozon canonicalizes a single numeric brand facet into the final category
        // path segment. Accept only that exact representation change.
        let Some(brand) = single_numeric_brand(&requested_query) else {
            return false;
        };
        let brand_pair = ("brand".to_owned(), brand.clone());
        requested_query.remove(&brand_pair);
        let category_path_matches = if target.path() == "/search/" {
            predicted_category
        } else {
            target.path() == final_url.path()
                || final_url
                    .path()
                    .trim_end_matches('/')
                    .strip_suffix(&format!("-{brand}"))
                    .and_then(|path| path.rsplit_once('/'))
                    .is_some_and(|(parent, _)| parent == target.path().trim_end_matches('/'))
        };
        return category_path_matches
            && category_path_has_brand(final_url.path(), &brand)
            && requested_query == actual_query;
    }
    let re = regex::Regex::new(r"^/product/(?:[^/]*-)?([0-9]+)/(reviews/)?$").unwrap();
    match (re.captures(target.path()), re.captures(final_url.path())) {
        (Some(a), Some(b)) => {
            a.get(1).map(|m| m.as_str()) == b.get(1).map(|m| m.as_str())
                && a.get(2).map(|m| m.as_str()) == b.get(2).map(|m| m.as_str())
        }
        _ => target.path() == final_url.path(),
    }
}

fn has_duplicate_query_key(url: &url::Url, expected: &str) -> bool {
    url.query_pairs()
        .filter(|(key, _)| key == expected)
        .nth(1)
        .is_some()
}

fn single_numeric_brand(
    query: &std::collections::BTreeMap<(String, String), usize>,
) -> Option<String> {
    let mut brands = query.iter().filter(|((key, _), _)| key == "brand");
    let ((_, brand), count) = brands.next()?;
    (brands.next().is_none()
        && *count == 1
        && !brand.is_empty()
        && brand.bytes().all(|byte| byte.is_ascii_digit()))
    .then(|| brand.clone())
}

fn category_path_has_brand(path: &str, brand: &str) -> bool {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .is_some_and(|segment| {
            segment == brand
                || segment
                    .strip_suffix(brand)
                    .is_some_and(|prefix| prefix.ends_with('-'))
        })
}

fn semantic_query(url: &url::Url) -> std::collections::BTreeMap<(String, String), usize> {
    let mut pairs = std::collections::BTreeMap::new();
    for (key, value) in url.query_pairs() {
        if is_navigation_metadata(&key) {
            continue;
        }
        if key == "sorting" && matches!(value.as_ref(), "score" | "popular") {
            continue;
        }
        let value = value.into_owned();
        *pairs.entry((key.into_owned(), value)).or_default() += 1;
    }
    pairs
}

fn is_navigation_metadata(key: &str) -> bool {
    matches!(
        key,
        "__rr" | "category_was_predicted" | "deny_category_prediction" | "from_global" | "at"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fallback_must_match_product_or_search() {
        assert!(is_requested_page(
            "https://www.ozon.ru/product/123/",
            "https://www.ozon.ru/product/item-123/"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/product/123/reviews/",
            "https://www.ozon.ru/product/123/"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse",
            "https://www.ozon.ru/search/?text=phone"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/product/123/",
            "https://evil.example/product/123/"
        ));
    }

    #[test]
    fn search_redirect_preserves_all_semantic_query_pairs() {
        let requested = "https://www.ozon.ru/search/?text=mouse&brand=logitech&delivery=tomorrow&page=2&search_page_state=abc";
        assert!(is_requested_page(
            requested,
            "https://www.ozon.ru/search/?search_page_state=abc&page=2&delivery=tomorrow&brand=logitech&text=mouse"
        ));
        for actual in [
            "https://www.ozon.ru/search/?text=mouse&delivery=tomorrow&page=2&search_page_state=abc",
            "https://www.ozon.ru/search/?text=mouse&brand=other&delivery=tomorrow&page=2&search_page_state=abc",
            "https://www.ozon.ru/search/?text=mouse&brand=logitech&page=2&search_page_state=abc",
            "https://www.ozon.ru/search/?text=mouse&brand=logitech&delivery=tomorrow&page=3&search_page_state=abc",
            "https://www.ozon.ru/search/?text=mouse&brand=logitech&delivery=tomorrow&page=2&search_page_state=changed",
            "https://www.ozon.ru/search/?text=mouse&brand=logitech&delivery=tomorrow&page=2&search_page_state=abc&color=black",
        ] {
            assert!(!is_requested_page(requested, actual), "accepted {actual}");
        }
    }

    #[test]
    fn search_redirect_compares_duplicate_query_pairs_as_a_multiset() {
        assert!(is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=a&brand=b",
            "https://www.ozon.ru/search/?brand=b&text=mouse&brand=a"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=a&brand=b",
            "https://www.ozon.ru/search/?text=mouse&brand=b&brand=b"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=a&brand=a",
            "https://www.ozon.ru/search/?text=mouse&brand=a"
        ));
    }

    #[test]
    fn search_redirect_allows_only_known_navigation_metadata() {
        assert!(is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=logitech",
            "https://www.ozon.ru/search/?brand=logitech&text=mouse&__rr=1&deny_category_prediction=true&from_global=true&at=analytics-token"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=logitech",
            "https://www.ozon.ru/search/?text=mouse&brand=logitech&tracking_token=123"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=logitech",
            "https://www.ozon.ru/search/?text=mouse&brand=logitech&utm_source=unknown"
        ));
    }

    #[test]
    fn search_redirect_normalizes_only_default_sort() {
        assert!(is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&sorting=popular",
            "https://www.ozon.ru/search/?text=mouse&sorting=score"
        ));
        assert!(is_requested_page(
            "https://www.ozon.ru/search/?text=mouse",
            "https://www.ozon.ru/search/?text=mouse&sorting=score"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse",
            "https://www.ozon.ru/search/?text=mouse&sorting=price"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&sorting=rating",
            "https://www.ozon.ru/search/?text=mouse"
        ));
        for actual in [
            "https://www.ozon.ru/search/?text=mouse&sorting=price&sorting=score",
            "https://www.ozon.ru/search/?text=mouse&sorting=score&sorting=price",
            "https://www.ozon.ru/search/?text=mouse&sorting=rating&sorting=popular",
            "https://www.ozon.ru/search/?text=mouse&sorting=popular&sorting=rating",
        ] {
            assert!(!is_requested_page(
                "https://www.ozon.ru/search/?text=mouse&sorting=price",
                actual
            ));
        }
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&sorting=price&sorting=score",
            "https://www.ozon.ru/search/?text=mouse&sorting=price"
        ));
    }

    #[test]
    fn search_prediction_requires_marker_and_matching_semantics() {
        assert!(is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=logitech",
            "https://www.ozon.ru/category/mice-15871/?brand=logitech&text=mouse&category_was_predicted=true"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=logitech",
            "https://www.ozon.ru/category/mice-15871/?brand=logitech&text=mouse"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=logitech",
            "https://www.ozon.ru/category/mice-15871/?brand=other&text=mouse&category_was_predicted=true"
        ));
        assert!(is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=26303256",
            "https://www.ozon.ru/category/mice-15871/logitech-26303256/?text=mouse&category_was_predicted=true"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=26303256&brand=other",
            "https://www.ozon.ru/category/mice-15871/logitech-26303256/?text=mouse&category_was_predicted=true"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=26303256",
            "https://www.ozon.ru/category/mice-15871/other-42/?text=mouse&category_was_predicted=true"
        ));
    }

    #[test]
    fn category_redirect_requires_exact_path_and_semantics() {
        assert!(is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=logitech&page=2",
            "https://www.ozon.ru/category/mice-15871/?page=2&brand=logitech&__rr=1"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=logitech&page=2",
            "https://www.ozon.ru/category/keyboards-15872/?brand=logitech&page=2"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=logitech&page=2",
            "https://www.ozon.ru/category/mice-15871/?brand=logitech&page=3"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=logitech&page=2",
            "https://www.ozon.ru/search/?brand=logitech&page=2"
        ));
        assert!(is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=26303256&page=2",
            "https://www.ozon.ru/category/mice-15871/logitech-26303256/?page=2"
        ));
        assert!(is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=26303256&sorting=score",
            "https://www.ozon.ru/category/mice-15871/logitech-26303256/"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=26303256&sorting=price",
            "https://www.ozon.ru/category/mice-15871/logitech-26303256/"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=26303256&sorting=rating",
            "https://www.ozon.ru/category/mice-15871/logitech-26303256/"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=26303256&page=2",
            "https://www.ozon.ru/category/keyboards-15872/logitech-26303256/?page=2"
        ));
    }

    #[tokio::test]
    #[ignore = "requires pinned agent-browser, Chrome, and an explicit disposable OZON_USER_DATA_DIR"]
    async fn cancelled_evaluation_closes_private_browser_and_can_restart() {
        assert!(
            std::env::var_os("OZON_USER_DATA_DIR").is_some(),
            "Use a disposable test profile"
        );
        let mut browser = Browser::from_env().await.unwrap();
        let cancel = CancellationToken::new();
        browser.launch(&cancel).await.unwrap();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            trigger.cancel();
        });
        let started = Instant::now();
        assert!(
            browser
                .evaluate("new Promise(() => {})", &cancel)
                .await
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        browser.shutdown().await.unwrap();
        let cancel = CancellationToken::new();
        browser.launch(&cancel).await.unwrap();
        assert_eq!(
            browser.evaluate("21 * 2", &cancel).await.unwrap(),
            json!(42)
        );
        browser.shutdown().await.unwrap();
    }
}
