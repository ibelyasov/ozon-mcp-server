use crate::browser_error::BrowserError;
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use futures_util::SinkExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::ErrorKind,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

#[path = "browser_recovery.rs"]
mod browser_recovery;

const DRIVER_VERSION: &str = "agent-browser 0.36.0";
const MAX_CLI_BYTES: usize = 12 * 1024 * 1024;
const OWNERSHIP_MARKER: &str = "browser-owned";

#[derive(Debug, Serialize, Deserialize)]
struct OwnershipMarker {
    version: u32,
    pid: u32,
    cdp: Option<String>,
    #[serde(default)]
    profile: Option<ProfileIdentity>,
    #[serde(default, rename = "closingDaemonPid")]
    closing_daemon_pid: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ProfileIdentity {
    path: PathBuf,
    dev: u64,
    ino: u64,
}
pub struct BrowserSession {
    binary: PathBuf,
    profile: PathBuf,
    executable: Option<String>,
    headed: bool,
    runtime: PathBuf,
    _profile_lock: File,
    user_agent: Option<String>,
    state: SessionState,
}

#[derive(Debug)]
pub(crate) enum RunningCommandOutcome {
    Completed(Value),
    AlreadyClosed,
}

#[derive(Debug)]
enum SessionState {
    Idle,
    Acquiring,
    Running { cdp: String },
    Poisoned,
}

impl BrowserSession {
    #[cfg(test)]
    pub(crate) fn test_running(binary: PathBuf, root: &Path) -> Self {
        let profile = root.join("profile");
        std::fs::create_dir(&profile).unwrap();
        let profile = std::fs::canonicalize(profile).unwrap();
        let profile_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(profile.join(".ozon-mcp.lock"))
            .unwrap();
        let runtime = root.join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(runtime.join("config.json"), "{}").unwrap();
        Self {
            binary,
            profile,
            executable: None,
            headed: false,
            runtime,
            _profile_lock: profile_lock,
            user_agent: Some("test".to_owned()),
            state: SessionState::Running {
                cdp: "http://127.0.0.1:9222".to_owned(),
            },
        }
    }

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
                    .join(".local/share/ozon-mcp-server/browser-profile")
            });
        let (profile, lock) = lease_profile(&profile)?;
        // Keep the daemon rendezvous stable across broker crashes. A restarted
        // broker reconnects to the surviving session instead of launching a
        // second Chromium against the same profile. The one-letter directory
        // also leaves room for agent-browser's socket on macOS.
        let runtime = std::env::var_os("OZON_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| profile.parent().unwrap_or(&profile).to_path_buf())
            .join("r");
        ensure_private_directory(&runtime).context("Cannot create browser runtime directory")?;
        write_runtime_config(&runtime)?;
        let mut browser = Self {
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
            .version_command()
            .output()
            .await
            .context("Cannot execute agent-browser")?;
        ensure!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim() == DRIVER_VERSION,
            "Expected agent-browser 0.36.0; set OZON_AGENT_BROWSER_BIN to the pinned binary"
        );
        if browser.read_marker()?.is_some() {
            // A marker can survive only when the previous owner did not confirm
            // Chromium shutdown. Reuse the same driver rendezvous and recover it
            // before this process is allowed to launch against the profile.
            browser.state = SessionState::Poisoned;
            browser
                .recover_poisoned()
                .await
                .map_err(|_| BrowserError::SessionPoisoned)?;
        }
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
        cmd.current_dir(&self.runtime)
            .env("AGENT_BROWSER_SOCKET_DIR", &self.runtime)
            .env("AGENT_BROWSER_PLUGINS", "[]")
            .env("AGENT_BROWSER_DEFAULT_TIMEOUT", "40000")
            .env("LANG", "ru_RU.UTF-8")
            .args(["--config", self.runtime.join("config.json").to_str().unwrap()])
            .args(["--session", "ozon", "--profile", self.profile.to_str().unwrap()])
            .args(["--headed", if self.headed { "true" } else { "false" }, "--no-webmcp", "--idle-timeout", "0"])
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

    fn version_command(&self) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.env_clear();
        for key in ["PATH", "HOME", "USER", "TMPDIR", "TEMP", "SystemRoot"] {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        cmd.current_dir(&self.runtime)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        cmd
    }

    async fn raw(
        &mut self,
        args: &[&str],
        script: Option<&str>,
        cancel: &CancellationToken,
        deadline: Duration,
    ) -> Result<Value> {
        if cancel.is_cancelled() {
            return Err(BrowserError::Cancelled.into());
        }
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
        let (result, interrupted) = tokio::select! {
            biased;
            _ = cancel.cancelled() => (Err(BrowserError::Cancelled.into()), true),
            _ = tokio::time::sleep(deadline) => (Err(BrowserError::CommandTimeout.into()), true),
            result = work => (result, false),
        };
        if result.is_err() {
            let _ = child.kill().await;
        }
        if interrupted {
            let cdp = match &self.state {
                SessionState::Running { cdp } => Some(cdp.clone()),
                _ => None,
            };
            // A cancelled CLI does not cancel the daemon-side evaluation. Do
            // not return ownership until the captured browser is confirmed idle.
            self.confirm_close(cdp.as_deref()).await?;
        }
        result
    }

    pub(crate) async fn run(&mut self, args: &[&str], cancel: &CancellationToken) -> Result<Value> {
        self.raw(args, None, cancel, Duration::from_secs(45)).await
    }

    pub(crate) async fn evaluate(
        &mut self,
        script: &str,
        cancel: &CancellationToken,
    ) -> Result<Value> {
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

    async fn raw_if_running(
        &mut self,
        args: &[&str],
        script: Option<&str>,
        deadline: Duration,
    ) -> Result<RunningCommandOutcome> {
        let captured_cdp = match &self.state {
            SessionState::Idle => return Ok(RunningCommandOutcome::AlreadyClosed),
            SessionState::Running { cdp } => cdp.clone(),
            SessionState::Acquiring | SessionState::Poisoned => {
                return Err(BrowserError::SessionPoisoned.into());
            }
        };
        let cleanup = CancellationToken::new();
        match self
            .raw(args, script, &cleanup, deadline.min(Duration::from_secs(5)))
            .await
        {
            Ok(value) => Ok(RunningCommandOutcome::Completed(value)),
            Err(error) => {
                if matches!(self.state, SessionState::Running { .. }) {
                    self.confirm_close(Some(&captured_cdp)).await?;
                }
                Err(error)
            }
        }
    }

    pub(crate) async fn run_if_running(
        &mut self,
        args: &[&str],
        deadline: Duration,
    ) -> Result<RunningCommandOutcome> {
        self.raw_if_running(args, None, deadline).await
    }

    pub(crate) async fn evaluate_if_running(
        &mut self,
        script: &str,
        deadline: Duration,
    ) -> Result<RunningCommandOutcome> {
        self.raw_if_running(&["eval", "--stdin"], Some(script), deadline)
            .await
            .map(|outcome| match outcome {
                RunningCommandOutcome::Completed(value) => {
                    RunningCommandOutcome::Completed(value["result"].clone())
                }
                RunningCommandOutcome::AlreadyClosed => RunningCommandOutcome::AlreadyClosed,
            })
    }

    async fn read_cdp(&mut self, cancel: &CancellationToken) -> Result<String> {
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
        validate_cdp_endpoint(endpoint)?;
        Ok(endpoint.to_owned())
    }

    async fn launch(&mut self, cancel: &CancellationToken) -> Result<()> {
        if cancel.is_cancelled() {
            return Err(BrowserError::Cancelled.into());
        }
        if matches!(self.state, SessionState::Poisoned) {
            return Err(BrowserError::SessionPoisoned.into());
        }
        self.mark_owned()?;
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
            .await
            .context(
                "BROWSER_DRIVER_FAILED: browser acquisition failed while opening the initial page",
            )?;
            self.read_cdp(&acquiring)
                .await
                .context(
                    "BROWSER_DRIVER_FAILED: browser acquisition failed while discovering the browser control endpoint",
                )
        }
        .await;
        let cdp = match acquisition {
            Ok(cdp) => cdp,
            Err(error) => {
                if matches!(
                    self.state,
                    SessionState::Acquiring | SessionState::Running { .. }
                ) {
                    self.confirm_close(None).await?;
                }
                return Err(error);
            }
        };
        self.update_marker(Some(&cdp))?;
        self.state = SessionState::Running { cdp };
        if cancel.is_cancelled() {
            let cdp = match &self.state {
                SessionState::Running { cdp } => cdp.clone(),
                _ => unreachable!(),
            };
            self.confirm_close(Some(&cdp)).await?;
            return Err(BrowserError::Cancelled.into());
        }
        Ok(())
    }

    pub(crate) async fn ensure_running(&mut self, cancel: &CancellationToken) -> Result<()> {
        if matches!(self.state, SessionState::Poisoned) {
            self.recover_poisoned().await?;
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
        self.confirm_close(cdp.as_deref()).await
    }

    async fn recover_poisoned(&mut self) -> Result<()> {
        let result =
            tokio::time::timeout(Duration::from_secs(11), self.recover_poisoned_inner()).await;
        match result {
            Ok(result) => result,
            Err(_) => {
                self.state = SessionState::Poisoned;
                Err(BrowserError::CleanupFailed.into())
            }
        }
    }

    async fn recover_poisoned_inner(&mut self) -> Result<()> {
        let marker = self.read_marker()?.ok_or(BrowserError::SessionPoisoned)?;
        if let Some(pid) = marker.closing_daemon_pid {
            let recovered =
                browser_recovery::confirm_driver_absent(&self.runtime, &self.profile, pid).await
                    || browser_recovery::close_recorded_driver(&self.runtime, &self.profile, pid)
                        .await;
            if recovered {
                self.remove_marker()?;
                self.state = SessionState::Idle;
                return Ok(());
            }
        }
        self.confirm_close(marker.cdp.as_deref()).await
    }

    async fn confirm_close(&mut self, cdp: Option<&str>) -> Result<()> {
        let result = tokio::time::timeout(Duration::from_secs(11), self.confirm_close_inner(cdp))
            .await
            .unwrap_or(false);
        if !result {
            self.state = SessionState::Poisoned;
            return Err(BrowserError::CleanupFailed.into());
        }
        self.remove_marker()?;
        self.state = SessionState::Idle;
        Ok(())
    }

    async fn confirm_close_inner(&self, cdp: Option<&str>) -> bool {
        // Never use an agent-browser command for recovery: its CLI starts the
        // daemon with launch configuration before dispatching `close`. The
        // captured browser CDP identity is the only safe process handle.
        let ipc_bound = self
            .read_marker()
            .ok()
            .flatten()
            .is_some_and(|marker| marker.version == 2 && marker.profile.is_some());
        let mut prepared_daemon = None;
        let cdp_closed = if let Some(endpoint) = cdp {
            if validate_cdp_endpoint(endpoint).is_err() {
                return false;
            }
            if ipc_bound {
                let Some(daemon_pid) =
                    browser_recovery::prepare_cdp_close(&self.runtime, &self.profile).await
                else {
                    return false;
                };
                if self.mark_closing(daemon_pid).is_err() {
                    return false;
                }
                prepared_daemon = Some(daemon_pid);
            }
            close_and_confirm_cdp_exit(endpoint).await
        } else {
            false
        };
        if cdp_closed {
            let Some(daemon_pid) = prepared_daemon else {
                return true;
            };
            if browser_recovery::confirm_driver_absent(&self.runtime, &self.profile, daemon_pid)
                .await
            {
                return true;
            }
        }
        if !cdp_closed
            && (!ipc_bound
                || !browser_recovery::close_matching_driver(
                    &self.runtime,
                    &self.profile,
                    |daemon_pid| self.mark_closing(daemon_pid).is_ok(),
                )
                .await)
        {
            return false;
        }
        if cdp_closed {
            let Some(daemon_pid) = prepared_daemon else {
                return false;
            };
            if !browser_recovery::close_recorded_driver(&self.runtime, &self.profile, daemon_pid)
                .await
            {
                return false;
            }
        }
        true
    }

    fn marker_path(&self) -> PathBuf {
        self.runtime.join(OWNERSHIP_MARKER)
    }

    fn read_marker(&self) -> Result<Option<OwnershipMarker>> {
        let path = self.marker_path();
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file()
                        && !metadata.file_type().is_symlink()
                        && metadata.uid() == unsafe { libc::geteuid() }
                        && metadata.permissions().mode() & 0o777 == 0o600
                        && metadata.len() <= 2048,
                    "BROWSER_CLEANUP_FAILED: invalid persisted browser ownership marker"
                );
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&path)?;
                let opened = file.metadata()?;
                ensure!(
                    opened.dev() == metadata.dev()
                        && opened.ino() == metadata.ino()
                        && opened.uid() == metadata.uid(),
                    "BROWSER_CLEANUP_FAILED: browser ownership marker changed"
                );
                use std::io::Read;
                let mut bytes = Vec::new();
                file.take(2049).read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() <= 2048,
                    "BROWSER_CLEANUP_FAILED: browser ownership marker is too large"
                );
                let marker: OwnershipMarker = serde_json::from_slice(&bytes)
                    .context("BROWSER_CLEANUP_FAILED: invalid browser ownership marker")?;
                ensure!(
                    marker.version == 1 || marker.version == 2,
                    "BROWSER_CLEANUP_FAILED: unsupported browser ownership marker"
                );
                if marker.version == 2 {
                    ensure!(
                        marker.profile.as_ref() == Some(&profile_identity(&self.profile)?),
                        "BROWSER_CLEANUP_FAILED: browser profile identity changed"
                    );
                }
                if let Some(endpoint) = &marker.cdp {
                    validate_cdp_endpoint(endpoint)?;
                }
                Ok(Some(marker))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn mark_owned(&self) -> Result<()> {
        if self.read_marker()?.is_some() {
            return Ok(());
        }
        write_marker_atomic(&self.runtime, &self.profile, None, None)?;
        ensure!(
            self.read_marker()?.is_some(),
            "BROWSER_CLEANUP_FAILED: browser ownership marker vanished"
        );
        Ok(())
    }

    fn update_marker(&self, cdp: Option<&str>) -> Result<()> {
        ensure!(self.read_marker()?.is_some(), BrowserError::CleanupFailed);
        write_marker_atomic(&self.runtime, &self.profile, cdp, None)
    }

    fn mark_closing(&self, daemon_pid: u32) -> Result<()> {
        ensure!(
            daemon_pid > 1,
            "BROWSER_CLEANUP_FAILED: invalid browser daemon identity"
        );
        let marker = self.read_marker()?.ok_or(BrowserError::CleanupFailed)?;
        ensure!(marker.version == 2, BrowserError::CleanupFailed);
        write_marker_atomic(
            &self.runtime,
            &self.profile,
            marker.cdp.as_deref(),
            Some(daemon_pid),
        )
    }

    fn remove_marker(&self) -> Result<()> {
        if self.read_marker()?.is_some() {
            std::fs::remove_file(self.marker_path())
                .context("BROWSER_CLEANUP_FAILED: cannot release browser ownership")?;
            let directory = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(&self.runtime)?;
            directory.sync_all()?;
        }
        Ok(())
    }
}

fn write_marker_atomic(
    runtime: &Path,
    profile: &Path,
    cdp: Option<&str>,
    closing_daemon_pid: Option<u32>,
) -> Result<()> {
    use std::io::Write;
    if let Some(endpoint) = cdp {
        validate_cdp_endpoint(endpoint)?;
    }
    let marker = OwnershipMarker {
        version: 2,
        pid: std::process::id(),
        cdp: cdp.map(str::to_owned),
        profile: Some(profile_identity(profile)?),
        closing_daemon_pid,
    };
    let bytes = serde_json::to_vec(&marker)?;
    ensure!(
        bytes.len() <= 2048,
        "BROWSER_CLEANUP_FAILED: browser ownership marker is too large"
    );
    let target = runtime.join(OWNERSHIP_MARKER);
    let mut temporary = tempfile::NamedTempFile::new_in(runtime)
        .context("BROWSER_CLEANUP_FAILED: cannot stage browser ownership")?;
    let metadata = temporary.as_file().metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o777 == 0o600,
        "BROWSER_CLEANUP_FAILED: invalid staged browser ownership"
    );
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&target)
        .context("BROWSER_CLEANUP_FAILED: cannot commit browser ownership")?;
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(runtime)?;
    directory.sync_all()?;
    Ok(())
}

fn profile_identity(profile: &Path) -> Result<ProfileIdentity> {
    let path = std::fs::canonicalize(profile)
        .context("BROWSER_CLEANUP_FAILED: cannot resolve browser profile")?;
    ensure!(
        path.to_str().is_some()
            && !path
                .as_os_str()
                .as_encoded_bytes()
                .iter()
                .any(|byte| matches!(byte, b'\r' | b'\n')),
        "BROWSER_CLEANUP_FAILED: invalid browser profile path"
    );
    let metadata = std::fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() },
        "BROWSER_CLEANUP_FAILED: invalid browser profile identity"
    );
    Ok(ProfileIdentity {
        path,
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

fn validate_cdp_endpoint(endpoint: &str) -> Result<()> {
    let url = url::Url::parse(endpoint).map_err(|_| BrowserError::DriverFailure {
        operation: "CDP endpoint validation",
    })?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        _ => false,
    };
    ensure!(
        url.scheme() == "ws"
            && loopback
            && url.port().is_some()
            && url.path().starts_with("/devtools/browser/")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        BrowserError::DriverFailure {
            operation: "CDP endpoint validation"
        }
    );
    Ok(())
}

async fn close_and_confirm_cdp_exit(endpoint: &str) -> bool {
    let connection = tokio::time::timeout(
        Duration::from_secs(1),
        tokio_tungstenite::connect_async(endpoint),
    )
    .await;
    let Ok(Ok((mut websocket, _))) = connection else {
        return false;
    };
    if websocket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"id":1,"method":"Browser.close"}).to_string().into(),
        ))
        .await
        .is_err()
    {
        return false;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let reachable = matches!(
            tokio::time::timeout(
                Duration::from_millis(500),
                tokio_tungstenite::connect_async(endpoint)
            )
            .await,
            Ok(Ok(_))
        );
        if !reachable {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn lease_profile(base: &Path) -> Result<(PathBuf, File)> {
    ensure_private_directory(base).context("Cannot create Ozon profile directory")?;
    let before =
        std::fs::symlink_metadata(base).context("Cannot inspect Ozon profile directory")?;
    let base = std::fs::canonicalize(base).context("Cannot resolve Ozon profile directory")?;
    let after =
        std::fs::symlink_metadata(&base).context("Cannot recheck Ozon profile directory")?;
    ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && after.uid() == unsafe { libc::geteuid() }
            && after.permissions().mode() & 0o777 == 0o700,
        "SOURCE_CHANGED: Ozon profile directory changed during acquisition"
    );
    if let Some(lock) = try_lock_profile(&base)
        .with_context(|| format!("Cannot lock Ozon profile directory {}", base.display()))?
    {
        return Ok((base, lock));
    }
    bail!("SERVER_BUSY: configured Ozon profile is already owned by another process")
}

fn write_runtime_config(runtime: &Path) -> Result<()> {
    use std::io::Write;
    let path = runtime.join("config.json");
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("INVALID_ARGUMENT: browser runtime config must not be a symbolic link");
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let metadata = file.metadata()?;
    let path_metadata = std::fs::symlink_metadata(&path)?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() }
            && metadata.dev() == path_metadata.dev()
            && metadata.ino() == path_metadata.ino()
            && metadata.permissions().mode() & 0o777 == 0o600,
        "INVALID_ARGUMENT: browser runtime config must be owner-only (0600)"
    );
    file.set_len(0)?;
    file.write_all(b"{}")?;
    file.sync_data()?;
    Ok(())
}

fn ensure_private_directory(path: &Path) -> std::io::Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "profile path must be a real directory",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if metadata.uid() != unsafe { libc::geteuid() }
                || metadata.permissions().mode() & 0o777 != 0o700
            {
                return Err(std::io::Error::new(
                    ErrorKind::PermissionDenied,
                    "profile directory must be owner-only (0700)",
                ));
            }
        }
        return Ok(());
    }
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(ErrorKind::InvalidInput, "profile path has no parent")
    })?;
    let parent_metadata = std::fs::symlink_metadata(parent)?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "profile parent must be a real directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if parent_metadata.uid() != unsafe { libc::geteuid() }
            || parent_metadata.permissions().mode() & 0o022 != 0
        {
            return Err(std::io::Error::new(
                ErrorKind::PermissionDenied,
                "profile parent must be owned by this OS user and not group/world writable",
            ));
        }
    }
    let mut directories = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories.create(path)?;
    Ok(())
}

fn try_lock_profile(profile: &Path) -> std::io::Result<Option<File>> {
    let path = profile.join(".ozon-mcp.lock");
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "profile lock must not be a symbolic link",
        ));
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let metadata = lock.metadata()?;
    let path_metadata = std::fs::symlink_metadata(&path)?;
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.dev() != path_metadata.dev()
        || metadata.ino() != path_metadata.ino()
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            "profile lock must be owner-only (0600)",
        ));
    }
    match lock.try_lock_exclusive() {
        Ok(()) => Ok(Some(lock)),
        Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::{fs::PermissionsExt, net::UnixListener},
        process::{Command as StdCommand, Stdio as StdStdio},
        time::Instant,
    };

    // Process creation can temporarily inherit flock descriptors on macOS.
    // Serialize this fault-injection test with lease/release assertions only.
    static PROFILE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn profile_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
        PROFILE_TEST_LOCK.blocking_lock()
    }

    fn test_session(root: &tempfile::TempDir, state: SessionState) -> BrowserSession {
        let profile = root.path().join("profile");
        std::fs::create_dir(&profile).unwrap();
        let profile = std::fs::canonicalize(profile).unwrap();
        let profile_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(profile.join(".ozon-mcp.lock"))
            .unwrap();
        let runtime = root.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(runtime.join("config.json"), "{}").unwrap();
        BrowserSession {
            binary: root.path().join("missing-agent-browser"),
            profile,
            executable: None,
            headed: false,
            runtime,
            _profile_lock: profile_lock,
            user_agent: Some("test".to_owned()),
            state,
        }
    }

    fn persist_legacy_cdp_marker(session: &BrowserSession, cdp: &str) {
        let bytes = serde_json::to_vec(&json!({
            "version": 1,
            "pid": std::process::id(),
            "cdp": cdp,
        }))
        .unwrap();
        std::fs::write(session.marker_path(), bytes).unwrap();
        std::fs::set_permissions(
            session.marker_path(),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }

    #[test]
    fn recovery_daemon_helper() {
        let Some(runtime) = std::env::var_os("OZON_TEST_RECOVERY_RUNTIME") else {
            return;
        };
        let runtime = PathBuf::from(runtime);
        let mode = std::env::var("OZON_TEST_RECOVERY_MODE").unwrap_or_default();
        let socket = runtime.join("ozon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(runtime.join("ozon.pid"), std::process::id().to_string()).unwrap();
        std::fs::set_permissions(
            runtime.join("ozon.pid"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        std::fs::write(
            runtime.join("ozon.version"),
            if mode == "wrong-version" {
                "0.35.0"
            } else {
                "0.36.0"
            },
        )
        .unwrap();
        std::fs::set_permissions(
            runtime.join("ozon.version"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        std::fs::write(runtime.join("ready"), b"").unwrap();

        for action in ["session_info", "close"] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["action"], action);
            if action == "session_info" && mode == "malformed" {
                writeln!(stream, "{{invalid").unwrap();
                return;
            }
            if action == "session_info" && mode == "oversized" {
                stream.write_all(&vec![b'x'; 16 * 1024 + 1]).unwrap();
                writeln!(stream).unwrap();
                return;
            }
            let response = if action == "session_info" {
                json!({
                    "id": request["id"],
                    "success": true,
                    "data": {
                        "backgroundPid": if mode == "wrong-pid" {
                            std::process::id() + 1
                        } else {
                            std::process::id()
                        },
                        "browserLaunched": false,
                        "pageCount": 0
                    }
                })
            } else {
                json!({"id": request["id"], "success": true, "data": {"closed": true}})
            };
            writeln!(stream, "{response}").unwrap();
        }
        drop(listener);
        std::fs::remove_file(socket).unwrap();
        std::fs::remove_file(runtime.join("ozon.pid")).unwrap();
        std::fs::remove_file(runtime.join("ozon.version")).unwrap();
    }

    async fn spawn_recovery_daemon(root: &tempfile::TempDir, mode: &str) -> std::process::Child {
        let runtime = root.path().join("runtime");
        let child = StdCommand::new(std::env::current_exe().unwrap())
            .args(["--exact", "browser::tests::recovery_daemon_helper"])
            .env("OZON_TEST_RECOVERY_RUNTIME", &runtime)
            .env("OZON_TEST_RECOVERY_MODE", mode)
            .stdin(StdStdio::null())
            .stdout(StdStdio::null())
            .stderr(StdStdio::null())
            .spawn()
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !runtime.join("ready").is_file() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "mock daemon did not start"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        child
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn null_cdp_marker_recovers_through_launch_free_ipc() {
        let root = tempfile::tempdir().unwrap();
        let mut session = test_session(&root, SessionState::Acquiring);
        session.mark_owned().unwrap();
        let mut daemon = spawn_recovery_daemon(&root, "").await;
        let daemon_pid = daemon.id();
        let waiter = tokio::task::spawn_blocking(move || daemon.wait());

        let result = session.shutdown().await;
        if result.is_err() && unsafe { libc::kill(daemon_pid as libc::pid_t, 0) } == 0 {
            unsafe { libc::kill(daemon_pid as libc::pid_t, libc::SIGKILL) };
        }
        let _ = waiter.await;

        assert!(result.is_ok());
        assert!(matches!(session.state, SessionState::Idle));
        assert!(!session.marker_path().exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_acquisition_preserves_cancellation_after_recovery() {
        let root = tempfile::tempdir().unwrap();
        let mut session = test_session(&root, SessionState::Acquiring);
        let fake_cli = root.path().join("fake-agent-browser");
        std::fs::write(&fake_cli, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
        std::fs::set_permissions(&fake_cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        session.binary = fake_cli;
        session.mark_owned().unwrap();
        let mut daemon = spawn_recovery_daemon(&root, "").await;
        let waiter = tokio::task::spawn_blocking(move || daemon.wait());
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });

        let error = session
            .raw(
                &["open", "about:blank"],
                None,
                &cancel,
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        waiter.await.unwrap().unwrap();

        assert!(matches!(
            error.downcast_ref::<BrowserError>(),
            Some(BrowserError::Cancelled)
        ));
        assert!(matches!(session.state, SessionState::Idle));
        assert!(!session.marker_path().exists());
    }

    async fn assert_mock_recovery_fails_closed(mode: &str) {
        let root = tempfile::tempdir().unwrap();
        let mut session = test_session(&root, SessionState::Acquiring);
        session.mark_owned().unwrap();
        let mut daemon = spawn_recovery_daemon(&root, mode).await;

        assert!(session.shutdown().await.is_err());
        let _ = daemon.kill();
        let _ = daemon.wait();
        assert!(matches!(session.state, SessionState::Poisoned));
        assert!(session.marker_path().is_file());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn untrusted_ipc_responses_never_clear_null_cdp_marker() {
        for mode in ["wrong-pid", "malformed", "oversized", "wrong-version"] {
            assert_mock_recovery_fails_closed(mode).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn durable_closing_phase_recovers_after_close_response_crash_window() {
        let root = tempfile::tempdir().unwrap();
        let mut session = test_session(&root, SessionState::Poisoned);
        session.mark_owned().unwrap();
        let mut daemon = spawn_recovery_daemon(&root, "").await;
        let waiter = tokio::task::spawn_blocking(move || daemon.wait());

        assert!(
            browser_recovery::close_matching_driver(&session.runtime, &session.profile, |pid| {
                session.mark_closing(pid).is_ok()
            },)
            .await
        );
        waiter.await.unwrap().unwrap();
        let marker = session.read_marker().unwrap().unwrap();
        let daemon_pid = marker.closing_daemon_pid.unwrap();
        assert!(
            browser_recovery::confirm_driver_absent(
                &session.runtime,
                &session.profile,
                daemon_pid,
            )
            .await
        );
        assert!(session.marker_path().is_file());
        session.recover_poisoned().await.unwrap();
        assert!(matches!(session.state, SessionState::Idle));
        assert!(!session.marker_path().exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recorded_closing_phase_closes_stale_daemon_without_browser() {
        let root = tempfile::tempdir().unwrap();
        let mut session = test_session(&root, SessionState::Poisoned);
        session.mark_owned().unwrap();
        let mut daemon = spawn_recovery_daemon(&root, "").await;
        let daemon_pid = daemon.id();
        session.mark_closing(daemon_pid).unwrap();
        let waiter = tokio::task::spawn_blocking(move || daemon.wait());

        session.recover_poisoned().await.unwrap();
        waiter.await.unwrap().unwrap();
        assert!(matches!(session.state, SessionState::Idle));
        assert!(!session.marker_path().exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unavailable_or_symlinked_recovery_files_never_clear_marker() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let mut unavailable = test_session(&root, SessionState::Acquiring);
        unavailable.mark_owned().unwrap();
        assert!(unavailable.shutdown().await.is_err());
        assert!(unavailable.marker_path().is_file());

        let root = tempfile::tempdir().unwrap();
        let mut symlinked_pid = test_session(&root, SessionState::Acquiring);
        symlinked_pid.mark_owned().unwrap();
        std::fs::write(
            root.path().join("pid-target"),
            std::process::id().to_string(),
        )
        .unwrap();
        symlink(
            root.path().join("pid-target"),
            symlinked_pid.runtime.join("ozon.pid"),
        )
        .unwrap();
        let listener = UnixListener::bind(symlinked_pid.runtime.join("ozon.sock")).unwrap();
        assert!(symlinked_pid.shutdown().await.is_err());
        assert!(symlinked_pid.marker_path().is_file());
        drop(listener);

        let root = tempfile::tempdir().unwrap();
        let mut symlinked_socket = test_session(&root, SessionState::Acquiring);
        symlinked_socket.mark_owned().unwrap();
        std::fs::write(
            symlinked_socket.runtime.join("ozon.pid"),
            std::process::id().to_string(),
        )
        .unwrap();
        let target = root.path().join("socket-target");
        let listener = UnixListener::bind(&target).unwrap();
        symlink(&target, symlinked_socket.runtime.join("ozon.sock")).unwrap();
        assert!(symlinked_socket.shutdown().await.is_err());
        assert!(symlinked_socket.marker_path().is_file());
        drop(listener);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn poisoned_session_can_retry_proven_recovery() {
        let root = tempfile::tempdir().unwrap();
        let mut session = test_session(&root, SessionState::Poisoned);
        session.mark_owned().unwrap();
        let mut daemon = spawn_recovery_daemon(&root, "").await;
        let waiter = tokio::task::spawn_blocking(move || daemon.wait());

        session.recover_poisoned().await.unwrap();
        waiter.await.unwrap().unwrap();
        assert!(matches!(session.state, SessionState::Idle));
        assert!(!session.marker_path().exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires fixed OZON_TEST_AGENT_BROWSER_BIN and disposable local Chrome"]
    async fn real_driver_null_marker_recovers_and_relaunches_on_disposable_profile() {
        let binary = std::env::var_os("OZON_TEST_AGENT_BROWSER_BIN")
            .expect("set OZON_TEST_AGENT_BROWSER_BIN to pinned agent-browser 0.36.0");
        let binary = std::fs::canonicalize(binary).unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut session = test_session(&root, SessionState::Idle);
        session.binary = binary;
        let cancel = CancellationToken::new();

        session.mark_owned().unwrap();
        session.state = SessionState::Acquiring;
        let initial_launch = session
            .raw(
                &["open", "about:blank"],
                None,
                &cancel,
                Duration::from_secs(15),
            )
            .await;
        if initial_launch.is_err() {
            let _ = session.confirm_close(None).await;
            let retained = root.keep();
            panic!(
                "disposable real-driver launch failed; retained {} for inspection",
                retained.display()
            );
        }
        if session.shutdown().await.is_err() {
            let retained = root.keep();
            panic!(
                "direct recovery failed; retained {} for inspection",
                retained.display()
            );
        }
        let relaunch = session.launch(&cancel).await;
        let cleanup = session.shutdown().await;
        if relaunch.is_err() || cleanup.is_err() {
            let retained = root.keep();
            panic!(
                "relaunch lifecycle failed; retained {} for inspection",
                retained.display()
            );
        }
    }

    #[tokio::test]
    async fn restore_command_never_invokes_the_driver_from_idle_or_poisoned() {
        let root = tempfile::tempdir().unwrap();
        let mut idle = test_session(&root, SessionState::Idle);
        assert!(matches!(
            idle.run_if_running(&["open", "https://www.ozon.ru/"], Duration::from_secs(1))
                .await
                .unwrap(),
            RunningCommandOutcome::AlreadyClosed
        ));
        assert!(!idle.marker_path().exists());

        idle.state = SessionState::Poisoned;
        let error = idle
            .run_if_running(&["open", "https://www.ozon.ru/"], Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<BrowserError>(),
            Some(BrowserError::SessionPoisoned)
        ));
        assert!(!idle.marker_path().exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_restore_command_closes_only_the_captured_browser() {
        let _guard = PROFILE_TEST_LOCK.lock().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "ws://{}/devtools/browser/test-restore",
            listener.local_addr().unwrap()
        );
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let message = futures_util::StreamExt::next(&mut websocket)
                .await
                .unwrap()
                .unwrap();
            assert!(message.to_text().unwrap().contains("Browser.close"));
        });
        let root = tempfile::tempdir().unwrap();
        let mut session = test_session(
            &root,
            SessionState::Running {
                cdp: endpoint.clone(),
            },
        );
        session.mark_owned().unwrap();
        persist_legacy_cdp_marker(&session, &endpoint);

        session
            .run_if_running(&["open", "https://www.ozon.ru/"], Duration::from_secs(1))
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(matches!(session.state, SessionState::Idle));
        assert!(!session.marker_path().exists());
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
        let runtime = root.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::write(runtime.join("config.json"), "{}").unwrap();
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

        session.mark_owned().unwrap();
        let cleanup = session.shutdown().await.unwrap_err();
        assert!(matches!(
            cleanup.downcast_ref::<BrowserError>(),
            Some(BrowserError::CleanupFailed)
        ));
        assert!(matches!(session.state, SessionState::Poisoned));
        assert!(session.marker_path().is_file());

        let ensure = session
            .ensure_running(&CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(
            ensure.downcast_ref::<BrowserError>(),
            Some(BrowserError::CleanupFailed)
        ));
        let shutdown = session.shutdown().await.unwrap_err();
        assert!(matches!(
            shutdown.downcast_ref::<BrowserError>(),
            Some(BrowserError::SessionPoisoned)
        ));
        assert!(session.marker_path().is_file());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confirmed_shutdown_removes_persisted_ownership_marker() {
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
        let runtime = root.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::write(runtime.join("config.json"), "{}").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "ws://{}/devtools/browser/test-identity",
            listener.local_addr().unwrap()
        );
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(listener);
            let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let message = futures_util::StreamExt::next(&mut websocket)
                .await
                .unwrap()
                .unwrap();
            assert!(message.to_text().unwrap().contains("Browser.close"));
        });
        let mut session = BrowserSession {
            binary: root.path().join("unused-agent-browser"),
            profile,
            executable: None,
            headed: false,
            runtime,
            _profile_lock: profile_lock,
            user_agent: Some("test".to_owned()),
            state: SessionState::Running {
                cdp: endpoint.clone(),
            },
        };

        session.mark_owned().unwrap();
        persist_legacy_cdp_marker(&session, &endpoint);
        assert!(session.marker_path().is_file());
        session.shutdown().await.unwrap();
        server.await.unwrap();
        assert!(matches!(session.state, SessionState::Idle));
        assert!(!session.marker_path().exists());
    }

    #[test]
    fn cdp_identity_requires_literal_loopback_browser_endpoint() {
        assert!(validate_cdp_endpoint("ws://127.0.0.1:9222/devtools/browser/id").is_ok());
        assert!(validate_cdp_endpoint("ws://[::1]:9222/devtools/browser/id").is_ok());
        assert!(validate_cdp_endpoint("ws://localhost:9222/devtools/browser/id").is_err());
        assert!(validate_cdp_endpoint("ws://192.0.2.1:9222/devtools/browser/id").is_err());
        assert!(validate_cdp_endpoint("ws://127.0.0.1:9222/devtools/page/id").is_err());
    }

    #[test]
    fn configured_profile_is_exclusive_and_reusable_after_release() {
        let _guard = profile_test_guard();
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("profile");
        let (base_profile, base_lock) = lease_profile(&base).unwrap();
        assert_eq!(base_profile, std::fs::canonicalize(&base).unwrap());
        let error = lease_profile(&base).unwrap_err();
        assert!(error.to_string().starts_with("SERVER_BUSY:"));
        assert!(!base_profile.join(".ozon-mcp-profiles").exists());

        drop(base_lock);
        let (reused_base, _reused_base_lock) = lease_profile(&base).unwrap();
        assert_eq!(reused_base, base_profile);
    }

    #[cfg(unix)]
    #[test]
    fn profile_lease_rejects_non_private_existing_directory() {
        let _guard = profile_test_guard();
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("profile");
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();

        let error = lease_profile(&base).unwrap_err();
        assert_eq!(
            std::fs::metadata(&base).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(
            error
                .to_string()
                .contains("Cannot create Ozon profile directory")
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
    async fn second_browser_rejects_configured_profile_contention() {
        assert!(
            std::env::var_os("OZON_USER_DATA_DIR").is_some(),
            "Use a disposable test profile"
        );
        let _first = BrowserSession::from_env().await.unwrap();
        let error = match BrowserSession::from_env().await {
            Ok(_) => panic!("second browser unexpectedly acquired the configured profile"),
            Err(error) => error,
        };
        assert!(error.to_string().starts_with("SERVER_BUSY:"));
    }
}
