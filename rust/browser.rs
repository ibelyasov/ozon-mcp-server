use crate::browser_error::BrowserError;
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use futures_util::SinkExt;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

const DRIVER_VERSION: &str = "agent-browser 0.36.0";
const MAX_CLI_BYTES: usize = 12 * 1024 * 1024;
const MAX_PROFILE_SLOTS: usize = 1024;
pub struct BrowserSession {
    binary: PathBuf,
    profile: PathBuf,
    executable: Option<String>,
    headed: bool,
    runtime: tempfile::TempDir,
    _profile_lock: File,
    user_agent: Option<String>,
    state: SessionState,
}

#[derive(Debug)]
enum SessionState {
    Idle,
    Acquiring,
    Running { cdp: String },
    Poisoned,
}

impl BrowserSession {
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
        let base_profile = std::env::var_os("OZON_USER_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                    .join(".ozon-mcp-rust-profile")
            });
        let (profile, lock) = lease_profile(&base_profile)?;
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
            user_agent: None,
            state: SessionState::Idle,
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
        let mut child = cmd.spawn().map_err(|_| BrowserError::DriverFailure {
            operation: "process startup",
        })?;
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let mut stdin = child.stdin.take();
        let work = async {
            let write = async {
                if let (Some(mut input), Some(text)) = (stdin.take(), script) {
                    input.write_all(text.as_bytes()).await.map_err(|_| {
                        BrowserError::DriverFailure {
                            operation: "stdin write",
                        }
                    })?;
                    input
                        .shutdown()
                        .await
                        .map_err(|_| BrowserError::DriverFailure {
                            operation: "stdin shutdown",
                        })?;
                }
                Ok::<_, anyhow::Error>(())
            };
            let read = async {
                let mut bytes = Vec::new();
                (&mut stdout)
                    .take((MAX_CLI_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|_| BrowserError::DriverFailure {
                        operation: "stdout read",
                    })?;
                if bytes.len() > MAX_CLI_BYTES {
                    return Err(BrowserError::DriverFailure {
                        operation: "response size check",
                    }
                    .into());
                }
                Ok::<_, anyhow::Error>(bytes)
            };
            let errors = async {
                // Drain without retaining page data or unbounded diagnostics.
                tokio::io::copy(&mut stderr, &mut tokio::io::sink())
                    .await
                    .map_err(|_| BrowserError::DriverFailure {
                        operation: "stderr drain",
                    })?;
                Ok::<_, anyhow::Error>(())
            };
            let ((), bytes, ()) = tokio::try_join!(write, read, errors)?;
            let status = child
                .wait()
                .await
                .map_err(|_| BrowserError::DriverFailure {
                    operation: "process wait",
                })?;
            let value: Value =
                serde_json::from_slice(&bytes).map_err(|_| BrowserError::InvalidBridgeResponse)?;
            // Never return arbitrary CLI stderr or a raw page response as an error.
            if !status.success() || value["success"] != true {
                return Err(BrowserError::CommandFailed {
                    command: args[0].to_owned(),
                }
                .into());
            }
            Ok(value["data"].clone())
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(BrowserError::Cancelled.into()),
            _ = tokio::time::sleep(deadline) => Err(BrowserError::CommandTimeout.into()),
            result = work => result,
        };
        if result.is_err() {
            let _ = child.kill().await;
        }
        result
    }

    pub(crate) async fn run(&self, args: &[&str], cancel: &CancellationToken) -> Result<Value> {
        self.raw(args, None, cancel, Duration::from_secs(45)).await
    }

    pub(crate) async fn evaluate(&self, script: &str, cancel: &CancellationToken) -> Result<Value> {
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

    async fn read_cdp(&self, cancel: &CancellationToken) -> Result<String> {
        let data = self
            .raw(&["get", "cdp-url"], None, cancel, Duration::from_secs(5))
            .await?;
        let endpoint = data["url"]
            .as_str()
            .or_else(|| data["value"].as_str())
            .or_else(|| data["cdpUrl"].as_str())
            .ok_or(BrowserError::DriverFailure {
                operation: "CDP endpoint discovery",
            })?;
        let url = url::Url::parse(endpoint).map_err(|_| BrowserError::DriverFailure {
            operation: "CDP endpoint validation",
        })?;
        if url.scheme() != "ws"
            || !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
            || !url.path().starts_with("/devtools/browser/")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(BrowserError::DriverFailure {
                operation: "CDP endpoint validation",
            }
            .into());
        }
        Ok(endpoint.to_owned())
    }

    async fn launch(&mut self, cancel: &CancellationToken) -> Result<()> {
        if cancel.is_cancelled() {
            return Err(BrowserError::Cancelled.into());
        }
        if matches!(self.state, SessionState::Poisoned) {
            return Err(BrowserError::SessionPoisoned.into());
        }
        self.state = SessionState::Acquiring;
        // Acquire the private browser handle before honoring cancellation. Killing
        // the CLI halfway through startup can orphan its detached daemon before
        // we know which CDP endpoint to close. The acquisition itself is bounded.
        let acquiring = CancellationToken::new();
        let acquisition = async {
            self.raw(
                &["open", "about:blank"],
                None,
                &acquiring,
                Duration::from_secs(15),
            )
            .await?;
            self.read_cdp(&acquiring).await
        }
        .await;
        let cdp = match acquisition {
            Ok(cdp) => cdp,
            Err(error) => {
                let closed = self
                    .raw(
                        &["close"],
                        None,
                        &CancellationToken::new(),
                        Duration::from_secs(7),
                    )
                    .await
                    .is_ok();
                self.state = if closed {
                    SessionState::Idle
                } else {
                    SessionState::Poisoned
                };
                return Err(error);
            }
        };
        self.state = SessionState::Running { cdp };
        if cancel.is_cancelled() {
            return Err(BrowserError::Cancelled.into());
        }
        Ok(())
    }

    pub(crate) async fn ensure_running(&mut self, cancel: &CancellationToken) -> Result<()> {
        if matches!(self.state, SessionState::Poisoned) {
            return Err(BrowserError::SessionPoisoned.into());
        }
        if matches!(self.state, SessionState::Running { .. }) {
            return Ok(());
        }
        self.launch(cancel).await?;
        if !self.headed && self.user_agent.is_none() {
            let ua = self.evaluate("navigator.userAgent", cancel).await?;
            self.shutdown().await?;
            self.user_agent = Some(
                ua.as_str()
                    .ok_or(BrowserError::DriverFailure {
                        operation: "User-Agent discovery",
                    })?
                    .replace("HeadlessChrome/", "Chrome/"),
            );
            self.launch(cancel).await?;
        }
        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        let cdp = match &self.state {
            SessionState::Idle => return Ok(()),
            SessionState::Running { cdp } => Some(cdp.clone()),
            SessionState::Acquiring => None,
            SessionState::Poisoned => return Err(BrowserError::SessionPoisoned.into()),
        };
        // agent-browser serializes commands, so `close` alone cannot interrupt eval.
        // This single standard CDP command only closes the private browser captured above.
        if let Some(endpoint) = cdp {
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
        if result.is_err() {
            self.state = SessionState::Poisoned;
            return Err(BrowserError::CleanupFailed.into());
        }
        self.state = SessionState::Idle;
        Ok(())
    }
}

fn lease_profile(base: &Path) -> Result<(PathBuf, File)> {
    ensure_private_directory(base).context("Cannot create Ozon profile directory")?;
    let base = std::fs::canonicalize(base).context("Cannot resolve Ozon profile directory")?;
    if let Some(lock) = try_lock_profile(&base)
        .with_context(|| format!("Cannot lock Ozon profile directory {}", base.display()))?
    {
        return Ok((base, lock));
    }

    let pool = base.join(".ozon-mcp-profiles");
    ensure_private_pool_directory(&pool).context("Cannot create Ozon profile pool")?;
    for slot in 1..=MAX_PROFILE_SLOTS {
        let profile = pool.join(slot.to_string());
        ensure_private_pool_directory(&profile)
            .with_context(|| format!("Cannot create Ozon profile slot {slot}"))?;
        let profile = std::fs::canonicalize(&profile)
            .with_context(|| format!("Cannot resolve Ozon profile slot {slot}"))?;
        if let Some(lock) = try_lock_profile(&profile)
            .with_context(|| format!("Cannot lock Ozon profile slot {slot}"))?
        {
            return Ok((profile, lock));
        }
    }
    bail!("All {MAX_PROFILE_SLOTS} Ozon profile slots are already in use")
}

fn ensure_private_directory(path: &Path) -> std::io::Result<()> {
    let mut directories = std::fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories.create(path)?;
    Ok(())
}

fn ensure_private_pool_directory(path: &Path) -> std::io::Result<()> {
    let existed = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "profile pool path must not be a symbolic link",
            ));
        }
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    ensure_private_directory(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "profile pool path must not be a symbolic link",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if existed && metadata.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::new(
                ErrorKind::PermissionDenied,
                "existing profile pool directory must have private permissions",
            ));
        }
        if !existed {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

fn try_lock_profile(profile: &Path) -> std::io::Result<Option<File>> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(profile.join(".ozon-mcp.lock"))?;
    match lock.try_lock_exclusive() {
        Ok(()) => Ok(Some(lock)),
        Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // Process creation can temporarily inherit flock descriptors on macOS.
    // Serialize this fault-injection test with lease/release assertions only.
    static PROFILE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn profile_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
        PROFILE_TEST_LOCK.blocking_lock()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_failure_poisons_session_and_blocks_further_driver_work() {
        let _guard = PROFILE_TEST_LOCK.lock().await;
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        std::fs::create_dir(&profile).unwrap();
        let profile_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(profile.join(".ozon-mcp.lock"))
            .unwrap();
        let runtime = tempfile::Builder::new()
            .prefix("runtime-")
            .tempdir_in(root.path())
            .unwrap();
        std::fs::write(runtime.path().join("config.json"), "{}").unwrap();
        let mut session = BrowserSession {
            binary: root.path().join("missing-agent-browser"),
            profile,
            executable: None,
            headed: false,
            runtime,
            _profile_lock: profile_lock,
            user_agent: Some("test".to_owned()),
            state: SessionState::Acquiring,
        };

        let cleanup = session.shutdown().await.unwrap_err();
        assert!(matches!(
            cleanup.downcast_ref::<BrowserError>(),
            Some(BrowserError::CleanupFailed)
        ));
        assert!(matches!(session.state, SessionState::Poisoned));

        let ensure = session
            .ensure_running(&CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(
            ensure.downcast_ref::<BrowserError>(),
            Some(BrowserError::SessionPoisoned)
        ));
        let shutdown = session.shutdown().await.unwrap_err();
        assert!(matches!(
            shutdown.downcast_ref::<BrowserError>(),
            Some(BrowserError::SessionPoisoned)
        ));
    }

    #[test]
    fn profile_leases_use_distinct_slots_and_reuse_released_slot() {
        let _guard = profile_test_guard();
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("profile");
        let (base_profile, base_lock) = lease_profile(&base).unwrap();
        let (first_slot, first_lock) = lease_profile(&base).unwrap();
        let (second_slot, second_lock) = lease_profile(&base).unwrap();

        assert_eq!(base_profile, std::fs::canonicalize(&base).unwrap());
        assert_eq!(first_slot, base_profile.join(".ozon-mcp-profiles/1"));
        assert_eq!(second_slot, base_profile.join(".ozon-mcp-profiles/2"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(base_profile.join(".ozon-mcp-profiles"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&first_slot).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        drop(base_lock);
        let (reused_base, _reused_base_lock) = lease_profile(&base).unwrap();
        assert_eq!(reused_base, base_profile);

        drop(first_lock);
        let (reused_slot, _reused_lock) = lease_profile(&base).unwrap();
        assert_eq!(reused_slot, first_slot);
        assert!(first_slot.join(".ozon-mcp.lock").is_file());
        drop(second_lock);
    }

    #[cfg(unix)]
    #[test]
    fn profile_lease_preserves_existing_base_permissions() {
        let _guard = profile_test_guard();
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("profile");
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (_profile, _lock) = lease_profile(&base).unwrap();
        assert_eq!(
            std::fs::metadata(&base).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn profile_lease_does_not_fallback_after_non_contention_error() {
        let _guard = profile_test_guard();
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("profile");
        ensure_private_directory(&base).unwrap();
        std::fs::create_dir(base.join(".ozon-mcp.lock")).unwrap();

        let error = lease_profile(&base).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Cannot lock Ozon profile directory")
        );
        assert!(!base.join(".ozon-mcp-profiles").exists());
    }

    #[cfg(unix)]
    #[test]
    fn profile_lease_rejects_symlinked_pool() {
        let _guard = profile_test_guard();
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("profile");
        let outside = root.path().join("outside");
        ensure_private_directory(&outside).unwrap();
        let (_base_profile, _base_lock) = lease_profile(&base).unwrap();
        symlink(&outside, base.join(".ozon-mcp-profiles")).unwrap();

        let error = lease_profile(&base).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Cannot create Ozon profile pool")
        );
        assert!(!outside.join(".ozon-mcp.lock").exists());
    }

    #[tokio::test]
    #[ignore = "requires pinned agent-browser, Chrome, and an explicit disposable OZON_USER_DATA_DIR"]
    async fn cancelled_evaluation_closes_private_browser_and_can_restart() {
        assert!(
            std::env::var_os("OZON_USER_DATA_DIR").is_some(),
            "Use a disposable test profile"
        );
        let mut browser = BrowserSession::from_env().await.unwrap();
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

    #[tokio::test]
    #[ignore = "requires pinned agent-browser, Chrome, and an explicit disposable OZON_USER_DATA_DIR"]
    async fn concurrent_profiles_keep_browsers_independent() {
        assert!(
            std::env::var_os("OZON_USER_DATA_DIR").is_some(),
            "Use a disposable test profile"
        );
        let mut first = BrowserSession::from_env().await.unwrap();
        let mut second = BrowserSession::from_env().await.unwrap();
        let first_cancel = CancellationToken::new();
        let second_cancel = CancellationToken::new();
        let result = async {
            ensure!(first.profile != second.profile, "Profiles must be distinct");
            let (first_launch, second_launch) =
                tokio::join!(first.launch(&first_cancel), second.launch(&second_cancel));
            first_launch?;
            second_launch?;

            ensure!(
                first
                    .evaluate("globalThis.__ozonMcpProfileMarker = 'first'", &first_cancel)
                    .await?
                    == json!("first"),
                "First browser marker was not retained"
            );
            ensure!(
                second
                    .evaluate(
                        "globalThis.__ozonMcpProfileMarker = 'second'",
                        &second_cancel,
                    )
                    .await?
                    == json!("second"),
                "Second browser marker was not retained"
            );

            first.shutdown().await?;
            ensure!(
                second
                    .evaluate("globalThis.__ozonMcpProfileMarker", &second_cancel)
                    .await?
                    == json!("second"),
                "Second browser stopped with the first browser"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let first_shutdown = first.shutdown().await;
        let second_shutdown = second.shutdown().await;
        result.unwrap();
        first_shutdown.unwrap();
        second_shutdown.unwrap();
    }
}
