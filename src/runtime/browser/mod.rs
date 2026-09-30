//! Direct Chromium ownership: one private profile lease, one browser and one owned page.
use crate::{
    error::{Code, fail},
    runtime::browser::error::BrowserError,
    runtime::config::Config,
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{File, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

mod cdp;
pub(crate) mod error;
mod identity;
use cdp::Cdp;
use identity::ProcessIdentity;

const OWNER_FILE: &str = ".ozon-mcp-owner.json";
const OWNER_VERSION: u32 = 3;
const ACQUISITION_TIMEOUT: Duration = Duration::from_secs(15);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(40);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const EXIT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OWNER_BYTES: u64 = 16 * 1024;
const CHROMIUM_SINGLETON_FILES: [&str; 3] = ["SingletonLock", "SingletonSocket", "SingletonCookie"];

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileIdentity {
    path: PathBuf,
    dev: u64,
    ino: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Acquiring,
    Running,
    Closing,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnershipRecord {
    version: u32,
    generation: String,
    phase: Phase,
    profile: ProfileIdentity,
    executable: PathBuf,
    process: Option<ProcessIdentity>,
    browser_endpoint: Option<String>,
    target_id: Option<String>,
}

struct OwnedBrowser {
    child: Child,
    identity: Option<ProcessIdentity>,
    browser: Option<Cdp>,
    page: Option<Cdp>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionState {
    Idle,
    Acquiring,
    Running,
    Closing,
    Poisoned,
}

pub struct BrowserSession {
    config: Config,
    profile: ProfileIdentity,
    _profile_lock: Option<File>,
    cleanup_on_drop: bool,
    state: SessionState,
    record: Option<OwnershipRecord>,
    owned: Option<OwnedBrowser>,
}

#[derive(Debug)]
pub(crate) enum RunningCommandOutcome {
    Completed(Value),
    AlreadyClosed,
}

impl BrowserSession {
    #[cfg(test)]
    pub(crate) async fn test_running(root: &Path, page_endpoint: &str) -> Self {
        let config = Config::at(root.to_path_buf()).unwrap();
        let mut session = Self::new(&config).await.unwrap();
        tests::owned_child(&mut session, true).await;
        session.owned.as_mut().unwrap().page = Some(Cdp::connect(page_endpoint).await.unwrap());
        session
    }

    pub async fn new(config: &Config) -> Result<Self> {
        let (profile, lock) = lease_profile(&config.profile)?;
        let mut session = Self {
            config: config.clone(),
            profile,
            _profile_lock: Some(lock),
            cleanup_on_drop: true,
            state: SessionState::Idle,
            record: None,
            owned: None,
        };
        if let Some(record) = session.read_record()? {
            session.record = Some(record);
            session.state = SessionState::Poisoned;
            session.recover().await?;
        }
        Ok(session)
    }

    pub(crate) async fn ensure_running(&mut self, cancel: &CancellationToken) -> Result<()> {
        if cancel.is_cancelled() {
            return Err(BrowserError::Cancelled.into());
        }
        match self.state {
            SessionState::Running => return Ok(()),
            SessionState::Poisoned => return Err(BrowserError::SessionPoisoned.into()),
            SessionState::Acquiring | SessionState::Closing => {
                return Err(BrowserError::CleanupFailed.into());
            }
            SessionState::Idle => {}
        }
        self.launch(cancel).await
    }

    async fn launch(&mut self, cancel: &CancellationToken) -> Result<()> {
        let executable = self.config.browser.executable.clone().ok_or_else(|| fail(Code::InvalidConfiguration, "Set OZON_BROWSER_EXECUTABLE to an explicit Chromium executable before marketplace calls"))?;
        ensure_profile_unclaimed(&self.profile)?;
        let previous_endpoint_file = read_endpoint_file(&self.profile.path).ok();
        let record = OwnershipRecord {
            version: OWNER_VERSION,
            generation: uuid::Uuid::new_v4().to_string(),
            phase: Phase::Acquiring,
            profile: self.profile.clone(),
            executable: executable.clone(),
            process: None,
            browser_endpoint: None,
            target_id: None,
        };
        self.state = SessionState::Acquiring;
        self.record = Some(record);
        if let Err(error) = self.write_record(true) {
            self.state = SessionState::Poisoned;
            return Err(error);
        }
        let child = self.spawn_reserved(browser_command(
            &executable,
            &self.profile.path,
            self.config.browser.headless,
        ))?;
        self.owned = Some(OwnedBrowser {
            child,
            identity: None,
            browser: None,
            page: None,
        });
        let acquisition = async {
            let pid = self
                .owned
                .as_ref()
                .and_then(|owned| owned.child.id())
                .ok_or(BrowserError::CleanupFailed)?;
            let identity_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
            let identity = loop {
                match ProcessIdentity::capture(pid, &executable, &self.profile.path) {
                    Ok(identity) => break identity,
                    Err(_) if tokio::time::Instant::now() < identity_deadline => {
                        if self.owned.as_mut().unwrap().child.try_wait()?.is_some() {
                            return Err(BrowserError::DriverFailure {
                                operation: "launch",
                            }
                            .into());
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(_) => return Err(BrowserError::CleanupFailed.into()),
                }
            };
            self.owned.as_mut().unwrap().identity = Some(identity.clone());
            self.record.as_mut().unwrap().process = Some(identity);
            // Capture and persist identity before waiting for Chromium's endpoint.
            self.persist_record()?;
            let endpoint = loop {
                if self.owned.as_mut().unwrap().child.try_wait()?.is_some() {
                    return Err(BrowserError::DriverFailure {
                        operation: "launch",
                    }
                    .into());
                }
                if let Ok(bytes) = read_endpoint_file(&self.profile.path)
                    && previous_endpoint_file.as_ref() != Some(&bytes)
                {
                    break endpoint_from_file(&bytes)?;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            ensure!(
                self.owned
                    .as_ref()
                    .unwrap()
                    .identity
                    .as_ref()
                    .unwrap()
                    .still_matches(&self.profile.path)?,
                BrowserError::CleanupFailed
            );
            self.record.as_mut().unwrap().browser_endpoint = Some(endpoint.clone());
            self.persist_record()?;
            let mut browser = Cdp::connect(&endpoint).await?;
            let target = browser.create_target().await?;
            self.owned.as_mut().unwrap().browser = Some(browser);
            self.record.as_mut().unwrap().target_id = Some(target.clone());
            self.persist_record()?;
            let mut page_url = cdp::validate_endpoint(&endpoint)?;
            page_url.set_path(&format!("/devtools/page/{target}"));
            let mut page = Cdp::connect(page_url.as_str()).await?;
            page.enable_page().await?;
            self.owned.as_mut().unwrap().page = Some(page);
            self.record.as_mut().unwrap().phase = Phase::Running;
            self.persist_record()?;
            Ok::<(), anyhow::Error>(())
        };
        let result = tokio::select! {
            _ = cancel.cancelled() => Err(BrowserError::Cancelled.into()),
            result = tokio::time::timeout(ACQUISITION_TIMEOUT, acquisition) => result.unwrap_or_else(|_| Err(BrowserError::CommandTimeout.into())),
        };
        if let Err(error) = result {
            // Cleanup is shielded from request cancellation and retains the lease until exit.
            self.shutdown().await?;
            return Err(error);
        }
        self.state = SessionState::Running;
        Ok(())
    }

    fn spawn_reserved(&mut self, mut command: Command) -> Result<Child> {
        // Chromium can forward this launch to a native profile owner, including
        // a visible browser. Recheck immediately before spawn while holding our
        // lease; independently launched Chromium does not observe that lease.
        let result = ensure_profile_unclaimed(&self.profile).and_then(|()| {
            command.spawn().map_err(|_| {
                BrowserError::DriverFailure {
                    operation: "launch",
                }
                .into()
            })
        });
        match result {
            Ok(child) => Ok(child),
            Err(error) => {
                // No child was started, so only our launch reservation can be
                // released. Chromium's own entries are never read or removed.
                if self.release_record().is_err() {
                    self.state = SessionState::Poisoned;
                    return Err(BrowserError::CleanupFailed.into());
                }
                self.state = SessionState::Idle;
                Err(error)
            }
        }
    }

    pub async fn navigate(&mut self, url: &str, cancel: &CancellationToken) -> Result<()> {
        self.ensure_running(cancel).await?;
        let result = tokio::select! {
            _ = cancel.cancelled() => Err(BrowserError::Cancelled.into()),
            result = tokio::time::timeout(OPERATION_TIMEOUT, self.page()?.navigate(url)) => result.unwrap_or_else(|_| Err(BrowserError::CommandTimeout.into())),
        };
        self.after_operation(result).await
    }

    pub async fn evaluate(
        &mut self,
        expression: &str,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.ensure_running(cancel).await?;
        let result = tokio::select! {
            _ = cancel.cancelled() => Err(BrowserError::Cancelled.into()),
            result = tokio::time::timeout(OPERATION_TIMEOUT, self.page()?.evaluate(expression)) => result.unwrap_or_else(|_| Err(BrowserError::CommandTimeout.into())),
        };
        self.after_operation(result).await
    }

    pub async fn wait(&mut self, duration: Duration, cancel: &CancellationToken) -> Result<()> {
        self.ensure_running(cancel).await?;
        tokio::select! {
            _ = cancel.cancelled() => { self.shutdown().await?; Err(BrowserError::Cancelled.into()) },
            _ = tokio::time::sleep(duration) => Ok(()),
        }
    }

    fn page(&mut self) -> Result<&mut Cdp> {
        self.owned
            .as_mut()
            .and_then(|owned| owned.page.as_mut())
            .ok_or_else(|| BrowserError::SessionPoisoned.into())
    }

    async fn after_operation<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.shutdown().await?;
        }
        result
    }

    fn is_running(&self) -> Result<bool> {
        match self.state {
            SessionState::Idle => Ok(false),
            SessionState::Running => Ok(true),
            _ => Err(BrowserError::SessionPoisoned.into()),
        }
    }

    pub(crate) async fn navigate_if_running(
        &mut self,
        url: &str,
        deadline: Duration,
    ) -> Result<RunningCommandOutcome> {
        if !self.is_running()? {
            return Ok(RunningCommandOutcome::AlreadyClosed);
        }
        let result = tokio::time::timeout(deadline, self.page()?.navigate(url))
            .await
            .unwrap_or_else(|_| Err(BrowserError::CommandTimeout.into()));
        self.after_operation(result).await?;
        Ok(RunningCommandOutcome::Completed(Value::Null))
    }
    pub(crate) async fn wait_if_running(
        &mut self,
        duration: Duration,
        deadline: Duration,
    ) -> Result<RunningCommandOutcome> {
        if !self.is_running()? {
            return Ok(RunningCommandOutcome::AlreadyClosed);
        }
        if duration > deadline {
            self.shutdown().await?;
            return Err(BrowserError::CommandTimeout.into());
        }
        tokio::time::sleep(duration).await;
        Ok(RunningCommandOutcome::Completed(Value::Null))
    }
    pub(crate) async fn evaluate_if_running(
        &mut self,
        expression: &str,
        deadline: Duration,
    ) -> Result<RunningCommandOutcome> {
        if !self.is_running()? {
            return Ok(RunningCommandOutcome::AlreadyClosed);
        }
        let result = tokio::time::timeout(deadline, self.page()?.evaluate(expression))
            .await
            .unwrap_or_else(|_| Err(BrowserError::CommandTimeout.into()));
        Ok(RunningCommandOutcome::Completed(
            self.after_operation(result).await?,
        ))
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        if self.state == SessionState::Idle {
            return Ok(());
        }
        if self.owned.is_none() {
            self.state = SessionState::Poisoned;
            return Err(BrowserError::SessionPoisoned.into());
        }
        self.state = SessionState::Closing;
        if let Some(record) = &mut self.record {
            record.phase = Phase::Closing;
        }
        let marker_ok = self.persist_record().is_ok();
        let endpoint = self
            .record
            .as_ref()
            .and_then(|record| record.browser_endpoint.clone());
        // Drop any interrupted page RPC. Browser.close uses a separate fresh connection.
        self.owned.as_mut().unwrap().page = None;
        self.owned.as_mut().unwrap().browser = None;
        // Close input for any owned carrier; Chromium itself starts with null input.
        drop(self.owned.as_mut().unwrap().child.stdin.take());
        if let Some(endpoint) = endpoint {
            let owned = self.owned.as_ref().unwrap();
            if owned
                .identity
                .as_ref()
                .is_some_and(|identity| identity.still_matches(&self.profile.path).unwrap_or(false))
            {
                let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
                    let mut browser = Cdp::connect(&endpoint).await?;
                    browser.close_browser().await
                })
                .await;
            }
        }
        let child = &mut self.owned.as_mut().unwrap().child;
        let exited = tokio::time::timeout(EXIT_TIMEOUT, child.wait())
            .await
            .is_ok_and(|result| result.is_ok());
        let exited = if exited {
            true
        } else {
            // An unreaped Child pins its PID. Never signal a recovered non-child PID.
            let owned = self.owned.as_mut().unwrap();
            let identity_ok = owned
                .identity
                .as_ref()
                .is_none_or(|identity| identity.still_matches(&self.profile.path).unwrap_or(false));
            identity_ok
                && owned.child.start_kill().is_ok()
                && tokio::time::timeout(EXIT_TIMEOUT, owned.child.wait())
                    .await
                    .is_ok_and(|result| result.is_ok())
        };
        if !marker_ok || !exited {
            self.state = SessionState::Poisoned;
            return Err(BrowserError::CleanupFailed.into());
        }
        if self.release_record().is_err() {
            self.state = SessionState::Poisoned;
            return Err(BrowserError::CleanupFailed.into());
        }
        self.owned = None;
        self.state = SessionState::Idle;
        Ok(())
    }

    async fn recover(&mut self) -> Result<()> {
        let record = self
            .record
            .as_ref()
            .ok_or(BrowserError::SessionPoisoned)?
            .clone();
        let Some(identity) = record.process.as_ref() else {
            // Parent may have crashed between spawn and recording PID. Ownership is unknown.
            return Err(BrowserError::SessionPoisoned.into());
        };
        if identity.confirmed_absent()? {
            self.release_record()?;
            self.state = SessionState::Idle;
            return Ok(());
        }
        ensure!(
            identity.still_matches(&self.profile.path)?,
            BrowserError::SessionPoisoned
        );
        let endpoint = record
            .browser_endpoint
            .ok_or(BrowserError::SessionPoisoned)?;
        self.record.as_mut().unwrap().phase = Phase::Closing;
        self.persist_record()?;
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
            let mut browser = Cdp::connect(&endpoint).await?;
            browser.close_browser().await
        })
        .await;
        // A PID check followed by kill is not an atomic identity operation on macOS.
        // Recovery therefore never kills a process without our unreaped Child handle.
        ensure!(
            identity.wait_absent(EXIT_TIMEOUT).await?,
            BrowserError::SessionPoisoned
        );
        self.release_record()?;
        self.state = SessionState::Idle;
        Ok(())
    }

    fn record_path(&self) -> PathBuf {
        self.profile.path.join(OWNER_FILE)
    }
    fn read_record(&self) -> Result<Option<OwnershipRecord>> {
        let path = self.record_path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(BrowserError::CleanupFailed.into()),
        };
        ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.permissions().mode() & 0o777 == 0o600
                && metadata.len() <= MAX_OWNER_BYTES,
            BrowserError::CleanupFailed
        );
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|_| BrowserError::CleanupFailed)?;
        ensure!(
            same_file(&metadata, &file.metadata()?),
            BrowserError::CleanupFailed
        );
        let mut bytes = Vec::new();
        file.take(MAX_OWNER_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_OWNER_BYTES,
            BrowserError::CleanupFailed
        );
        let record: OwnershipRecord =
            serde_json::from_slice(&bytes).map_err(|_| BrowserError::CleanupFailed)?;
        ensure!(
            record.version == OWNER_VERSION
                && record.profile == self.profile
                && record.generation.len() == 36
                && record
                    .process
                    .as_ref()
                    .is_none_or(
                        |process| process.executable == record.executable && process.pid > 1
                    ),
            BrowserError::CleanupFailed
        );
        if let Some(endpoint) = &record.browser_endpoint {
            ensure!(
                cdp::validate_endpoint(endpoint)?
                    .path()
                    .starts_with("/devtools/browser/"),
                BrowserError::CleanupFailed
            );
        }
        Ok(Some(record))
    }
    fn persist_record(&self) -> Result<()> {
        self.write_record(false)
    }
    fn write_record(&self, reservation: bool) -> Result<()> {
        let record = self.record.as_ref().ok_or(BrowserError::CleanupFailed)?;
        match self.read_record()? {
            Some(current) => ensure!(
                current.generation == record.generation,
                BrowserError::CleanupFailed
            ),
            None => ensure!(reservation, BrowserError::CleanupFailed),
        }
        let bytes = serde_json::to_vec(record)?;
        ensure!(
            bytes.len() as u64 <= MAX_OWNER_BYTES,
            BrowserError::CleanupFailed
        );
        let mut temporary = tempfile::NamedTempFile::new_in(&self.profile.path)?;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.record_path())
            .map_err(|_| BrowserError::CleanupFailed)?;
        sync_directory(&self.profile.path)?;
        Ok(())
    }
    fn release_record(&mut self) -> Result<()> {
        let current = self.read_record()?.ok_or(BrowserError::CleanupFailed)?;
        ensure!(
            self.record
                .as_ref()
                .is_some_and(|record| record.generation == current.generation),
            BrowserError::CleanupFailed
        );
        std::fs::remove_file(self.record_path())?;
        sync_directory(&self.profile.path)?;
        self.record = None;
        Ok(())
    }
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        if self.owned.is_none() {
            return;
        }
        if self.cleanup_on_drop {
            let mut cleanup = Self {
                config: self.config.clone(),
                profile: self.profile.clone(),
                _profile_lock: self._profile_lock.take(),
                cleanup_on_drop: false,
                state: self.state,
                record: self.record.take(),
                owned: self.owned.take(),
            };
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                // The task owns both Child and profile lease. Aborting the caller
                // cannot release either while this bounded shutdown is running.
                runtime.spawn(async move {
                    let _ = cleanup.shutdown().await;
                });
                return;
            }
            // Without an executor, leave durable ownership unresolved. Dropping
            // this guard below deliberately preserves its live lease and Child.
            return;
        }
        // Cleanup failure or runtime teardown is unknown, never idle. Preserve
        // ownership until process exit; the durable record then fails closed.
        if let Some(owned) = self.owned.take() {
            std::mem::forget(owned);
        }
        if let Some(lease) = self._profile_lock.take() {
            std::mem::forget(lease);
        }
    }
}

fn browser_command(executable: &Path, profile: &Path, headless: bool) -> Command {
    let mut command = Command::new(executable);
    command.arg(profile_argument(profile)).args([
        "--remote-debugging-port=0",
        "--remote-debugging-address=127.0.0.1",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-extensions",
        "--disable-background-networking",
        "--mute-audio",
        "--lang=ru-RU",
    ]);
    if headless {
        command.args(["--headless=new", "--no-startup-window"]);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

fn profile_argument(profile: &Path) -> std::ffi::OsString {
    let mut argument = std::ffi::OsString::from("--user-data-dir=");
    argument.push(profile.as_os_str());
    argument
}

fn ensure_profile_unclaimed(profile: &ProfileIdentity) -> Result<()> {
    let unknown = || {
        fail(
            Code::ServerBusy,
            "Cannot establish that the browser profile is unclaimed",
        )
    };
    let metadata = std::fs::symlink_metadata(&profile.path).map_err(|_| unknown())?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.dev() == profile.dev
            && metadata.ino() == profile.ino
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o777 == 0o700,
        unknown()
    );
    for name in CHROMIUM_SINGLETON_FILES {
        match std::fs::symlink_metadata(profile.path.join(name)) {
            // Lock and cookie targets are normally dangling symlinks. Existence
            // says nothing about liveness; let the owner resolve stale entries.
            Ok(_) => {
                return Err(fail(
                    Code::ServerBusy,
                    "Chromium ownership markers exist for this profile; close its browser and resolve stale markers manually",
                ));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => return Err(unknown()),
        }
    }
    Ok(())
}

fn lease_profile(path: &Path) -> Result<(ProfileIdentity, File)> {
    crate::runtime::config::ensure_private_profile(path)?;
    let profile = std::fs::canonicalize(path)?;
    let metadata = std::fs::symlink_metadata(&profile)?;
    let lock_path = profile.join(".ozon-mcp.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
        .context("Cannot lease private browser profile")?;
    let opened = lock.metadata()?;
    let named = std::fs::symlink_metadata(&lock_path)?;
    ensure!(
        same_file(&opened, &named)
            && opened.is_file()
            && opened.uid() == unsafe { libc::geteuid() }
            && opened.permissions().mode() & 0o777 == 0o600,
        fail(Code::InvalidArgument, "Invalid browser profile lock")
    );
    lock.try_lock_exclusive().map_err(|error| {
        if error.kind() == ErrorKind::WouldBlock {
            fail(Code::ServerBusy, "Another broker owns this browser profile")
        } else {
            error.into()
        }
    })?;
    Ok((
        ProfileIdentity {
            path: profile,
            dev: metadata.dev(),
            ino: metadata.ino(),
        },
        lock,
    ))
}

fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino() && left.uid() == right.uid()
}
fn sync_directory(path: &Path) -> Result<()> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?
        .sync_all()?;
    Ok(())
}
fn read_endpoint_file(profile: &Path) -> Result<Vec<u8>> {
    let path = profile.join("DevToolsActivePort");
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.len() <= 1024,
        BrowserError::InvalidBridgeResponse
    );
    let mut bytes = Vec::new();
    file.take(1025).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024, BrowserError::InvalidBridgeResponse);
    Ok(bytes)
}
fn endpoint_from_file(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes).map_err(|_| BrowserError::InvalidBridgeResponse)?;
    let mut lines = text.lines();
    let port = lines
        .next()
        .ok_or(BrowserError::InvalidBridgeResponse)?
        .parse::<u16>()
        .map_err(|_| BrowserError::InvalidBridgeResponse)?;
    let path = lines.next().ok_or(BrowserError::InvalidBridgeResponse)?;
    ensure!(
        port > 0
            && path.starts_with("/devtools/browser/")
            && path.len() <= 256
            && lines.next().is_none(),
        BrowserError::InvalidBridgeResponse
    );
    let endpoint = format!("ws://127.0.0.1:{port}{path}");
    cdp::validate_endpoint(&endpoint)?;
    Ok(endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use serde_json::json;
    use tokio::net::TcpListener;

    pub(super) async fn owned_child(session: &mut BrowserSession, controlled_input: bool) -> u32 {
        let executable = std::fs::canonicalize("/bin/bash").unwrap();
        let child = Command::new(&executable)
            .args([
                "-c",
                if controlled_input {
                    "read -r _"
                } else {
                    "kill -STOP $$"
                },
                "chromium-test",
            ])
            .arg(profile_argument(&session.profile.path))
            .stdin(if controlled_input {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let identity = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(identity) =
                    ProcessIdentity::capture(pid, &executable, &session.profile.path)
                {
                    break identity;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("test child identity must be capturable");
        session.record = Some(OwnershipRecord {
            version: OWNER_VERSION,
            generation: uuid::Uuid::new_v4().to_string(),
            phase: Phase::Running,
            profile: session.profile.clone(),
            executable,
            process: Some(identity.clone()),
            browser_endpoint: None,
            target_id: Some("owned-page".into()),
        });
        session.write_record(true).unwrap();
        session.owned = Some(OwnedBrowser {
            child,
            identity: Some(identity),
            browser: None,
            page: None,
        });
        session.state = SessionState::Running;
        pid
    }

    async fn test_session() -> (tempfile::TempDir, BrowserSession) {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = Config::at(root.path().to_path_buf()).unwrap();
        let session = BrowserSession::new(&config).await.unwrap();
        (root, session)
    }

    fn spawn_probe(session: &mut BrowserSession, root: &Path) -> PathBuf {
        let executable = root.join("spawn-probe");
        std::fs::write(&executable, "#!/bin/sh\n: > \"$0.spawned\"\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executable = std::fs::canonicalize(executable).unwrap();
        let sentinel = executable.with_file_name("spawn-probe.spawned");
        session.config.browser.executable = Some(executable);
        sentinel
    }

    #[tokio::test]
    async fn native_singleton_entries_block_before_reservation_and_spawn_without_changes() {
        for name in CHROMIUM_SINGLETON_FILES {
            for shape in ["file", "directory", "dangling_symlink"] {
                let (root, mut session) = test_session().await;
                let sentinel = spawn_probe(&mut session, root.path());
                let marker = session.profile.path.join(name);
                match shape {
                    "file" => std::fs::write(&marker, b"external-owner\n").unwrap(),
                    "directory" => std::fs::create_dir(&marker).unwrap(),
                    _ => std::os::unix::fs::symlink("unknown-host-12345", &marker).unwrap(),
                }
                let before = std::fs::symlink_metadata(&marker).unwrap();
                let error = session
                    .ensure_running(&CancellationToken::new())
                    .await
                    .unwrap_err();
                assert_eq!(crate::error::code(&error), "SERVER_BUSY", "{name} {shape}");
                assert_eq!(session.state, SessionState::Idle);
                assert!(session.owned.is_none());
                assert!(session.record.is_none());
                assert!(!session.record_path().exists());
                assert!(!sentinel.exists(), "executable must not be invoked");
                session.shutdown().await.unwrap();
                let after = std::fs::symlink_metadata(&marker).unwrap();
                assert!(same_file(&before, &after));
                assert_eq!(before.permissions().mode(), after.permissions().mode());
                match shape {
                    "file" => assert_eq!(std::fs::read(&marker).unwrap(), b"external-owner\n"),
                    "directory" => assert!(after.is_dir()),
                    _ => assert_eq!(
                        std::fs::read_link(&marker).unwrap(),
                        Path::new("unknown-host-12345")
                    ),
                }
            }
        }
    }

    #[tokio::test]
    async fn native_singleton_appearing_after_reservation_blocks_spawn_and_releases_only_our_record()
     {
        let (root, mut session) = test_session().await;
        let sentinel = spawn_probe(&mut session, root.path());
        ensure_profile_unclaimed(&session.profile).unwrap();
        session.record = Some(OwnershipRecord {
            version: OWNER_VERSION,
            generation: uuid::Uuid::new_v4().to_string(),
            phase: Phase::Acquiring,
            profile: session.profile.clone(),
            executable: session.config.browser.executable.clone().unwrap(),
            process: None,
            browser_endpoint: None,
            target_id: None,
        });
        session.state = SessionState::Acquiring;
        session.write_record(true).unwrap();
        let marker = session.profile.path.join("SingletonLock");
        std::os::unix::fs::symlink("unknown-host-12345", &marker).unwrap();
        let before = std::fs::symlink_metadata(&marker).unwrap();
        let command = browser_command(
            session.config.browser.executable.as_ref().unwrap(),
            &session.profile.path,
            true,
        );
        let error = session.spawn_reserved(command).unwrap_err();
        assert_eq!(crate::error::code(&error), "SERVER_BUSY");
        assert_eq!(session.state, SessionState::Idle);
        assert!(session.record.is_none());
        assert!(session.owned.is_none());
        assert!(!session.record_path().exists());
        assert!(!sentinel.exists());
        assert!(same_file(
            &before,
            &std::fs::symlink_metadata(&marker).unwrap()
        ));
        assert_eq!(
            std::fs::read_link(marker).unwrap(),
            Path::new("unknown-host-12345")
        );
    }

    #[tokio::test]
    async fn unknown_or_replaced_profile_blocks_before_spawn() {
        for replacement in [false, true] {
            let (root, mut session) = test_session().await;
            let sentinel = spawn_probe(&mut session, root.path());
            let original = root.path().join("leased-profile");
            std::fs::rename(&session.profile.path, &original).unwrap();
            if replacement {
                std::fs::create_dir(&session.profile.path).unwrap();
                std::fs::set_permissions(
                    &session.profile.path,
                    std::fs::Permissions::from_mode(0o700),
                )
                .unwrap();
            }
            let error = session
                .ensure_running(&CancellationToken::new())
                .await
                .unwrap_err();
            assert_eq!(crate::error::code(&error), "SERVER_BUSY");
            assert_eq!(session.state, SessionState::Idle);
            assert!(session.record.is_none());
            assert!(!sentinel.exists());
            if replacement {
                std::fs::remove_dir(&session.profile.path).unwrap();
            }
            std::fs::rename(original, &session.profile.path).unwrap();
        }
    }

    #[tokio::test]
    async fn native_singleton_does_not_prevent_recovery_of_proven_exited_owned_process() {
        let (root, mut session) = test_session().await;
        let sentinel = spawn_probe(&mut session, root.path());
        owned_child(&mut session, true).await;
        let config = session.config.clone();
        let owned = session.owned.as_mut().unwrap();
        drop(owned.child.stdin.take());
        owned.child.wait().await.unwrap();
        let marker = session.profile.path.join("SingletonLock");
        std::os::unix::fs::symlink("unknown-host-12345", &marker).unwrap();
        let before = std::fs::symlink_metadata(&marker).unwrap();
        let owner_record = session.record_path();
        session.owned = None;
        drop(session);
        let mut recovered = BrowserSession::new(&config).await.unwrap();
        assert!(
            !owner_record.exists(),
            "recorded recovery precedes the native guard"
        );
        assert_eq!(recovered.state, SessionState::Idle);
        let error = recovered
            .ensure_running(&CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(crate::error::code(&error), "SERVER_BUSY");
        assert!(!sentinel.exists());
        assert!(same_file(
            &before,
            &std::fs::symlink_metadata(&marker).unwrap()
        ));
        assert_eq!(
            std::fs::read_link(marker).unwrap(),
            Path::new("unknown-host-12345")
        );
    }

    #[test]
    fn headless_launch_has_no_visible_or_camouflage_flags() {
        let profile = Path::new("/private/test/profile");
        let command = browser_command(Path::new("/test/chromium"), profile, true);
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|arg| arg == "--headless=new"));
        assert!(args.iter().any(|arg| arg == "--no-startup-window"));
        assert!(
            args.iter()
                .any(|arg| arg == "--remote-debugging-address=127.0.0.1")
        );
        assert!(!args.iter().any(|arg| arg.contains("user-agent")
            || arg.contains("AutomationControlled")
            || arg.contains("activate")
            || arg.contains("bringToFront")));
        let visible = browser_command(Path::new("/test/chromium"), profile, false);
        assert!(
            !visible
                .as_std()
                .get_args()
                .any(|arg| arg == "--headless=new")
        );
        assert_eq!(command.as_std().get_program(), "/test/chromium");
    }

    #[test]
    fn profile_argument_preserves_non_unicode_path_bytes() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let profile = PathBuf::from(std::ffi::OsString::from_vec(
            b"/private/profile-\xff".to_vec(),
        ));
        let command = browser_command(Path::new("/test/chromium"), &profile, true);
        assert_eq!(
            command.as_std().get_args().next().unwrap().as_bytes(),
            b"--user-data-dir=/private/profile-\xff"
        );
    }

    #[tokio::test]
    async fn executable_is_required_only_for_live_operations_and_profile_lease_is_exclusive() {
        let (root, mut session) = test_session().await;
        let config = Config::at(root.path().to_path_buf()).unwrap();
        assert_eq!(
            crate::error::code(&BrowserSession::new(&config).await.err().unwrap()),
            "SERVER_BUSY"
        );
        assert_eq!(
            crate::error::code(
                &session
                    .ensure_running(&CancellationToken::new())
                    .await
                    .unwrap_err()
            ),
            "INVALID_ARGUMENT"
        );
        assert_eq!(session.state, SessionState::Idle);
        assert!(!session.record_path().exists());
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_waits_for_owned_child_exit_before_releasing_record() {
        let (_root, mut session) = test_session().await;
        let pid = owned_child(&mut session, false).await;
        let identity = session.owned.as_ref().unwrap().identity.clone().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        // A cancellation raised before admission must not launch a replacement.
        assert!(matches!(
            session
                .navigate("about:blank", &cancel)
                .await
                .unwrap_err()
                .downcast_ref::<BrowserError>(),
            Some(BrowserError::Cancelled)
        ));
        assert_eq!(session.owned.as_ref().unwrap().child.id(), Some(pid));
        assert!(identity.still_matches(&session.profile.path).unwrap());
        session.shutdown().await.unwrap();
        assert!(identity.confirmed_absent().unwrap());
        assert!(!session.record_path().exists());
        assert_eq!(session.state, SessionState::Idle);
    }

    #[tokio::test]
    async fn interrupted_rpc_cleans_up_and_never_relaunches_during_restore() {
        let (_root, mut session) = test_session().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        owned_child(&mut session, false).await;
        let identity = session.owned.as_ref().unwrap().identity.clone().unwrap();
        let endpoint = format!(
            "ws://127.0.0.1:{}/devtools/page/owned",
            listener.local_addr().unwrap().port()
        );
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let request: Value =
                serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(request["method"], "Runtime.evaluate");
            entered_tx.send(()).unwrap();
            let _ = socket.next().await;
        });
        session.owned.as_mut().unwrap().page = Some(Cdp::connect(&endpoint).await.unwrap());
        let cancel = CancellationToken::new();
        let cancel_other = cancel.clone();
        let cancelled = tokio::spawn(async move {
            entered_rx.await.unwrap();
            cancel_other.cancel();
        });
        let error = session
            .evaluate("new Promise(() => {})", &cancel)
            .await
            .unwrap_err();
        assert_eq!(crate::error::code(&error), "CANCELLED");
        assert!(identity.confirmed_absent().unwrap());
        assert!(matches!(
            session
                .navigate_if_running("about:blank", Duration::from_secs(1))
                .await
                .unwrap(),
            RunningCommandOutcome::AlreadyClosed
        ));
        assert!(matches!(
            session
                .evaluate_if_running("42", Duration::from_secs(1))
                .await
                .unwrap(),
            RunningCommandOutcome::AlreadyClosed
        ));
        assert!(!session.record_path().exists());
        cancelled.await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn marker_fault_preserves_poisoned_ownership_after_confirmed_exit() {
        let (_root, mut session) = test_session().await;
        owned_child(&mut session, false).await;
        let identity = session.owned.as_ref().unwrap().identity.clone().unwrap();
        std::fs::remove_file(session.record_path()).unwrap();
        std::fs::create_dir(session.record_path()).unwrap();
        assert!(matches!(
            session
                .shutdown()
                .await
                .unwrap_err()
                .downcast_ref::<BrowserError>(),
            Some(BrowserError::CleanupFailed)
        ));
        assert!(identity.confirmed_absent().unwrap());
        assert_eq!(session.state, SessionState::Poisoned);
        assert!(session.record_path().is_dir());
        assert!(
            session
                .ensure_running(&CancellationToken::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn crash_reservation_and_old_markers_fail_closed_without_deletion() {
        let (_root, mut session) = test_session().await;
        session.record = Some(OwnershipRecord {
            version: OWNER_VERSION,
            generation: uuid::Uuid::new_v4().to_string(),
            phase: Phase::Acquiring,
            profile: session.profile.clone(),
            executable: PathBuf::from("/test/chromium"),
            process: None,
            browser_endpoint: None,
            target_id: None,
        });
        session.write_record(true).unwrap();
        let config = session.config.clone();
        let marker = session.record_path();
        drop(session);
        assert!(BrowserSession::new(&config).await.is_err());
        assert!(marker.exists());
        let old = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(old.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = Config::at(old.path().to_path_buf()).unwrap();
        std::fs::create_dir(config.root.join("r")).unwrap();
        std::fs::write(config.root.join("r/browser-owned"), r#"{"version":2}"#).unwrap();
        let mut fresh = BrowserSession::new(&config).await.unwrap();
        fresh.shutdown().await.unwrap();
        assert!(config.root.join("r/browser-owned").exists());
        std::fs::write(fresh.record_path(), r#"{"version":2}"#).unwrap();
        let marker = fresh.record_path();
        drop(fresh);
        assert!(BrowserSession::new(&config).await.is_err());
        assert_eq!(std::fs::read_to_string(marker).unwrap(), r#"{"version":2}"#);
    }

    #[tokio::test]
    async fn startup_exit_is_confirmed_before_launch_reservation_is_released() {
        let (_root, mut session) = test_session().await;
        session.config.browser.executable = Some(std::fs::canonicalize("/usr/bin/true").unwrap());
        let error = session
            .ensure_running(&CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(crate::error::code(&error), "SOURCE_CHANGED");
        assert_eq!(session.state, SessionState::Idle);
        assert!(session.owned.is_none());
        assert!(!session.record_path().exists());
    }

    #[tokio::test]
    async fn reused_or_unrelated_pid_never_receives_cdp_close() {
        let (_root, mut session) = test_session().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "ws://127.0.0.1:{}/devtools/browser/unrelated",
            listener.local_addr().unwrap().port()
        );
        let executable = std::env::current_exe().unwrap();
        session.record = Some(OwnershipRecord {
            version: OWNER_VERSION,
            generation: uuid::Uuid::new_v4().to_string(),
            phase: Phase::Running,
            profile: session.profile.clone(),
            executable: executable.clone(),
            process: Some(ProcessIdentity {
                pid: std::process::id(),
                start_seconds: 0,
                start_fraction: 0,
                executable,
            }),
            browser_endpoint: Some(endpoint),
            target_id: Some("unrelated".into()),
        });
        session.write_record(true).unwrap();
        let config = session.config.clone();
        drop(session);
        let mut recovered = BrowserSession::new(&config).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        recovered.shutdown().await.unwrap();
        assert!(!recovered.record_path().exists());
    }

    #[tokio::test]
    async fn restart_releases_a_proven_exited_owned_process() {
        let (_root, mut session) = test_session().await;
        owned_child(&mut session, false).await;
        let config = session.config.clone();
        let owned = session.owned.as_mut().unwrap();
        owned.child.start_kill().unwrap();
        owned.child.wait().await.unwrap();
        let marker = session.record_path();
        session.owned = None; // Simulate owner loss after independently confirmed exit.
        drop(session);
        let mut recovered = BrowserSession::new(&config).await.unwrap();
        assert!(!marker.exists());
        assert_eq!(recovered.state, SessionState::Idle);
        recovered.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dropping_owned_task_keeps_profile_lease_until_cleanup_finishes() {
        let (_root, mut session) = test_session().await;
        owned_child(&mut session, false).await;
        let config = session.config.clone();
        let identity = session.owned.as_ref().unwrap().identity.clone().unwrap();
        let marker = session.record_path();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            entered.send(()).unwrap();
            session
                .wait(Duration::from_secs(60), &CancellationToken::new())
                .await
                .unwrap();
        });
        ready.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // The Drop cleanup task still holds the same lease during bounded wait.
        assert_eq!(
            crate::error::code(&BrowserSession::new(&config).await.err().unwrap()),
            "SERVER_BUSY"
        );
        tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                if identity.confirmed_absent().unwrap() && !marker.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut replacement = BrowserSession::new(&config).await.unwrap();
        replacement.shutdown().await.unwrap();
    }

    #[test]
    fn endpoint_file_is_strict_and_uses_browser_target() {
        assert_eq!(
            endpoint_from_file(b"9222\n/devtools/browser/owned\n").unwrap(),
            "ws://127.0.0.1:9222/devtools/browser/owned"
        );
        for invalid in [
            b"0\n/devtools/browser/owned\n".as_slice(),
            b"9222\n/devtools/page/first\n",
            b"9222\n/devtools/browser/a\nextra\n",
        ] {
            assert!(endpoint_from_file(invalid).is_err());
        }
    }

    /// Offline disposable acceptance, explicitly opt-in; never reads user profiles.
    #[tokio::test]
    #[ignore = "requires OZON_TEST_CHROME and local process/socket permission"]
    async fn real_chromium_headless_lifecycle() {
        let executable = std::env::var_os("OZON_TEST_CHROME")
            .expect("set OZON_TEST_CHROME to an explicit Chromium executable");
        let root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = Config::at(root.path().to_path_buf()).unwrap();
        config.browser.executable = Some(std::fs::canonicalize(executable).unwrap());
        let mut browser = BrowserSession::new(&config).await.unwrap();
        browser
            .ensure_running(&CancellationToken::new())
            .await
            .unwrap();
        eprintln!(
            "OWNED_CHROMIUM_PID={}",
            browser.owned.as_ref().unwrap().child.id().unwrap()
        );
        let first = browser.owned.as_ref().unwrap().identity.clone().unwrap();
        browser
            .navigate("about:blank", &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            browser
                .evaluate("21 * 2", &CancellationToken::new())
                .await
                .unwrap(),
            json!(42)
        );
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let entered = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        assert_eq!(
            crate::error::code(
                &browser
                    .evaluate("new Promise(() => {})", &cancel)
                    .await
                    .unwrap_err()
            ),
            "CANCELLED"
        );
        entered.await.unwrap();
        assert!(first.confirmed_absent().unwrap());
        assert!(!browser.record_path().exists());
        browser
            .ensure_running(&CancellationToken::new())
            .await
            .unwrap();
        eprintln!(
            "OWNED_CHROMIUM_PID={}",
            browser.owned.as_ref().unwrap().child.id().unwrap()
        );
        let second = browser.owned.as_ref().unwrap().identity.clone().unwrap();
        assert_ne!(first, second);
        assert_eq!(
            browser
                .evaluate("6 * 7", &CancellationToken::new())
                .await
                .unwrap(),
            json!(42)
        );
        let marker = browser.record_path();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            entered.send(()).unwrap();
            browser
                .wait(Duration::from_secs(60), &CancellationToken::new())
                .await
                .unwrap();
        });
        ready.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if second.confirmed_absent().unwrap() && !marker.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut browser = BrowserSession::new(&config).await.unwrap();
        browser
            .ensure_running(&CancellationToken::new())
            .await
            .unwrap();
        eprintln!(
            "OWNED_CHROMIUM_PID={}",
            browser.owned.as_ref().unwrap().child.id().unwrap()
        );
        assert_eq!(
            browser
                .evaluate("7 * 6", &CancellationToken::new())
                .await
                .unwrap(),
            json!(42)
        );
        let third = browser.owned.as_ref().unwrap().identity.clone().unwrap();
        // Simulate owner loss without giving the replacement a Child handle.
        // A separate reaper only waits for exit; recovery must use the recorded
        // exact native identity + BrowserWS and standard Browser.close.
        let mut survivor = browser.owned.take().unwrap();
        survivor.browser = None;
        survivor.page = None;
        let reaper = tokio::spawn(async move { survivor.child.wait().await.unwrap() });
        drop(browser);
        let mut browser = BrowserSession::new(&config).await.unwrap();
        reaper.await.unwrap();
        assert!(third.confirmed_absent().unwrap());
        assert!(!browser.record_path().exists());
        browser
            .ensure_running(&CancellationToken::new())
            .await
            .unwrap();
        eprintln!(
            "OWNED_CHROMIUM_PID={}",
            browser.owned.as_ref().unwrap().child.id().unwrap()
        );
        assert_eq!(
            browser
                .evaluate("42", &CancellationToken::new())
                .await
                .unwrap(),
            json!(42)
        );
        let fourth = browser.owned.as_ref().unwrap().identity.clone().unwrap();
        browser.shutdown().await.unwrap();
        assert!(fourth.confirmed_absent().unwrap());
        assert!(!browser.record_path().exists());
    }
}
