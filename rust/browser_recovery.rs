use serde::Deserialize;
use serde_json::json;
use std::{
    io::Read,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::Path,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::Command,
};

const RECOVERY_LIMIT: Duration = Duration::from_secs(11);
const RESPONSE_LIMIT: usize = 16 * 1024;
const PID_FILE_LIMIT: u64 = 32;
const PROCESS_LIST_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

#[derive(Deserialize)]
struct Response<T> {
    id: String,
    success: bool,
    data: T,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionInfo {
    background_pid: u32,
    browser_launched: bool,
    page_count: u64,
}

#[derive(Deserialize)]
struct CloseInfo {
    closed: bool,
}

pub(super) async fn close_matching_driver<F>(
    runtime: &Path,
    profile: &Path,
    before_close: F,
) -> bool
where
    F: FnOnce(u32) -> bool,
{
    tokio::time::timeout(
        RECOVERY_LIMIT,
        close_matching_driver_inner(runtime, profile, before_close),
    )
    .await
    .is_ok_and(|result| result)
}

async fn close_matching_driver_inner<F>(runtime: &Path, profile: &Path, before_close: F) -> bool
where
    F: FnOnce(u32) -> bool,
{
    let Some(runtime_identity) = private_runtime_identity(runtime) else {
        return false;
    };
    if !driver_version_matches(runtime, runtime_identity) {
        return false;
    }
    let Some(pid) = read_pid(runtime, runtime_identity) else {
        return false;
    };
    let socket = runtime.join("ozon.sock");
    let Some(socket_identity) = socket_identity(&socket, runtime, runtime_identity) else {
        return false;
    };

    let info: Response<SessionInfo> = match request(
        &socket,
        runtime,
        runtime_identity,
        socket_identity,
        pid,
        json!({"id":"ozon-recovery-info","action":"session_info"}),
    )
    .await
    {
        Some(response) => response,
        None => return false,
    };
    if info.id != "ozon-recovery-info"
        || !info.success
        || info.data.background_pid != pid
        || info.data.page_count > u32::MAX as u64
    {
        return false;
    }
    let _browser_was_launched = info.data.browser_launched;
    if !daemon_owns_profile(
        pid,
        profile,
        info.data.browser_launched,
        info.data.page_count,
    )
    .await
        || !before_close(pid)
    {
        return false;
    }

    let close: Response<CloseInfo> = match request(
        &socket,
        runtime,
        runtime_identity,
        socket_identity,
        pid,
        json!({"id":"ozon-recovery-close","action":"close"}),
    )
    .await
    {
        Some(response) => response,
        None => return false,
    };
    if close.id != "ozon-recovery-close" || !close.success || !close.data.closed {
        return false;
    }

    let exit_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if !process_exists(pid) && !socket.exists() {
            break;
        }
        if tokio::time::Instant::now() >= exit_deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let Ok(profile) = std::fs::canonicalize(profile) else {
        return false;
    };
    process_list_has_profile(&profile)
        .await
        .is_some_and(|found| !found)
}

pub(super) async fn confirm_driver_absent(runtime: &Path, profile: &Path, pid: u32) -> bool {
    tokio::time::timeout(RECOVERY_LIMIT, async {
        if private_runtime_identity(runtime).is_none() {
            return false;
        }
        if process_exists(pid) || runtime.join("ozon.sock").exists() {
            return false;
        }
        let Ok(profile) = std::fs::canonicalize(profile) else {
            return false;
        };
        process_list_has_profile(&profile)
            .await
            .is_some_and(|found| !found)
    })
    .await
    .is_ok_and(|result| result)
}

pub(super) async fn prepare_cdp_close(runtime: &Path, profile: &Path) -> Option<u32> {
    tokio::time::timeout(Duration::from_secs(1), async {
        let runtime_identity = private_runtime_identity(runtime)?;
        if !driver_version_matches(runtime, runtime_identity) {
            return None;
        }
        let pid = read_pid(runtime, runtime_identity)?;
        let socket = runtime.join("ozon.sock");
        let expected_socket = socket_identity(&socket, runtime, runtime_identity)?;
        let stream = UnixStream::connect(&socket).await.ok()?;
        if socket_identity(&socket, runtime, runtime_identity) != Some(expected_socket)
            || !peer_matches(&stream, pid)
            || !daemon_owns_profile(pid, profile, true, 1).await
        {
            return None;
        }
        Some(pid)
    })
    .await
    .ok()
    .flatten()
}

pub(super) async fn close_recorded_driver(
    runtime: &Path,
    profile: &Path,
    expected_pid: u32,
) -> bool {
    tokio::time::timeout(RECOVERY_LIMIT, async {
        let Some(runtime_identity) = private_runtime_identity(runtime) else {
            return false;
        };
        if !driver_version_matches(runtime, runtime_identity)
            || read_pid(runtime, runtime_identity) != Some(expected_pid)
        {
            return false;
        }
        let socket = runtime.join("ozon.sock");
        let Some(expected_socket) = socket_identity(&socket, runtime, runtime_identity) else {
            return false;
        };
        let info: Response<SessionInfo> = match request(
            &socket,
            runtime,
            runtime_identity,
            expected_socket,
            expected_pid,
            json!({"id":"ozon-recorded-info","action":"session_info"}),
        )
        .await
        {
            Some(response) => response,
            None => return false,
        };
        if info.id != "ozon-recorded-info"
            || !info.success
            || info.data.background_pid != expected_pid
            || !daemon_has_no_profile_descendants(expected_pid).await
            || process_list_has_profile(profile)
                .await
                .is_none_or(|found| found)
        {
            return false;
        }
        let close: Response<CloseInfo> = match request(
            &socket,
            runtime,
            runtime_identity,
            expected_socket,
            expected_pid,
            json!({"id":"ozon-recorded-close","action":"close"}),
        )
        .await
        {
            Some(response) => response,
            None => return false,
        };
        if close.id != "ozon-recorded-close" || !close.success || !close.data.closed {
            return false;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while process_exists(expected_pid) || socket.exists() {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        process_list_has_profile(profile)
            .await
            .is_some_and(|found| !found)
    })
    .await
    .is_ok_and(|result| result)
}

fn private_runtime_identity(runtime: &Path) -> Option<FileIdentity> {
    let before = std::fs::symlink_metadata(runtime).ok()?;
    if before.file_type().is_symlink()
        || !before.is_dir()
        || before.uid() != unsafe { libc::geteuid() }
        || before.permissions().mode() & 0o777 != 0o700
    {
        return None;
    }
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(runtime)
        .ok()?;
    let opened = directory.metadata().ok()?;
    let identity = FileIdentity {
        dev: opened.dev(),
        ino: opened.ino(),
    };
    (identity
        == FileIdentity {
            dev: before.dev(),
            ino: before.ino(),
        })
    .then_some(identity)
}

fn runtime_is_unchanged(runtime: &Path, expected: FileIdentity) -> bool {
    private_runtime_identity(runtime) == Some(expected)
}

fn read_pid(runtime: &Path, runtime_identity: FileIdentity) -> Option<u32> {
    if !runtime_is_unchanged(runtime, runtime_identity) {
        return None;
    }
    let path = runtime.join("ozon.pid");
    let before = std::fs::symlink_metadata(&path).ok()?;
    if before.file_type().is_symlink()
        || !before.is_file()
        || before.uid() != unsafe { libc::geteuid() }
        || before.permissions().mode() & 0o022 != 0
        || before.len() > PID_FILE_LIMIT
    {
        return None;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .ok()?;
    let opened = file.metadata().ok()?;
    if opened.dev() != before.dev() || opened.ino() != before.ino() || opened.uid() != before.uid()
    {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(PID_FILE_LIMIT + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > PID_FILE_LIMIT || !runtime_is_unchanged(runtime, runtime_identity) {
        return None;
    }
    let pid = std::str::from_utf8(&bytes)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    (pid > 1).then_some(pid)
}

fn driver_version_matches(runtime: &Path, runtime_identity: FileIdentity) -> bool {
    let path = runtime.join("ozon.version");
    let Some(before) = std::fs::symlink_metadata(&path).ok() else {
        return false;
    };
    if !runtime_is_unchanged(runtime, runtime_identity)
        || before.file_type().is_symlink()
        || !before.is_file()
        || before.uid() != unsafe { libc::geteuid() }
        || before.permissions().mode() & 0o022 != 0
        || before.len() > 16
    {
        return false;
    }
    let Some(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()
    else {
        return false;
    };
    let Some(opened) = file.metadata().ok() else {
        return false;
    };
    let mut bytes = Vec::new();
    opened.dev() == before.dev()
        && opened.ino() == before.ino()
        && file.take(17).read_to_end(&mut bytes).is_ok()
        && bytes == b"0.36.0"
        && runtime_is_unchanged(runtime, runtime_identity)
}

fn socket_identity(
    socket: &Path,
    runtime: &Path,
    runtime_identity: FileIdentity,
) -> Option<FileIdentity> {
    if !runtime_is_unchanged(runtime, runtime_identity) {
        return None;
    }
    let metadata = std::fs::symlink_metadata(socket).ok()?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_socket()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
    {
        return None;
    }
    Some(FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

async fn request<T: for<'de> Deserialize<'de>>(
    socket: &Path,
    runtime: &Path,
    runtime_identity: FileIdentity,
    expected_socket: FileIdentity,
    expected_pid: u32,
    request: serde_json::Value,
) -> Option<T> {
    if socket_identity(socket, runtime, runtime_identity) != Some(expected_socket) {
        return None;
    }
    let mut stream = UnixStream::connect(socket).await.ok()?;
    if socket_identity(socket, runtime, runtime_identity) != Some(expected_socket)
        || !peer_matches(&stream, expected_pid)
    {
        return None;
    }
    let mut encoded = serde_json::to_vec(&request).ok()?;
    encoded.push(b'\n');
    stream.write_all(&encoded).await.ok()?;
    stream.shutdown().await.ok()?;

    let mut response = Vec::new();
    let bytes = BufReader::new(stream.take((RESPONSE_LIMIT + 1) as u64))
        .read_until(b'\n', &mut response)
        .await
        .ok()?;
    if bytes == 0 || bytes > RESPONSE_LIMIT || response.last() != Some(&b'\n') {
        return None;
    }
    serde_json::from_slice(&response).ok()
}

#[cfg(target_os = "macos")]
fn peer_matches(stream: &UnixStream, _expected_pid: u32) -> bool {
    let mut uid = 0;
    let mut gid = 0;
    unsafe {
        libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) == 0 && uid == libc::geteuid()
    }
}

#[cfg(target_os = "linux")]
fn peer_matches(stream: &UnixStream, expected_pid: u32) -> bool {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    result == 0
        && length as usize == std::mem::size_of::<libc::ucred>()
        && credentials.uid == unsafe { libc::geteuid() }
        && credentials.pid > 1
        && credentials.pid as u32 == expected_pid
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn peer_matches(_stream: &UnixStream, _expected_pid: u32) -> bool {
    false
}

fn process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

async fn process_list_has_profile(profile: &Path) -> Option<bool> {
    let profile = profile.as_os_str().as_encoded_bytes();
    if profile.iter().any(|byte| matches!(byte, b'\r' | b'\n')) {
        return None;
    }
    let equals = [b"--user-data-dir=".as_slice(), profile].concat();
    let separated = [b"--user-data-dir ".as_slice(), profile].concat();
    let bytes = read_process_list(&["-axww", "-o", "command="]).await?;
    Some(bytes.split(|byte| *byte == b'\n').any(|line| {
        contains_complete_argument(line, &equals) || contains_complete_argument(line, &separated)
    }))
}

async fn daemon_owns_profile(
    daemon_pid: u32,
    profile: &Path,
    browser_launched: bool,
    page_count: u64,
) -> bool {
    let Ok(profile) = std::fs::canonicalize(profile) else {
        return false;
    };
    let bytes = profile.as_os_str().as_encoded_bytes();
    if bytes.iter().any(|byte| matches!(byte, b'\r' | b'\n')) {
        return false;
    }
    let Some(processes) = process_list().await else {
        return false;
    };
    let descendants: Vec<_> = processes
        .iter()
        .filter(|process| is_descendant(process.pid, daemon_pid, &processes))
        .collect();
    let equals = [b"--user-data-dir=".as_slice(), bytes].concat();
    let separated = [b"--user-data-dir ".as_slice(), bytes].concat();
    let exact = |command: &[u8]| {
        contains_complete_argument(command, &equals)
            || contains_complete_argument(command, &separated)
    };
    let profile_bearing = |command: &[u8]| {
        command
            .windows(b"--user-data-dir=".len())
            .enumerate()
            .any(|(offset, part)| {
                part == b"--user-data-dir="
                    && (offset == 0 || command[offset - 1].is_ascii_whitespace())
            })
            || command
                .windows(b"--user-data-dir ".len())
                .enumerate()
                .any(|(offset, part)| {
                    part == b"--user-data-dir "
                        && (offset == 0 || command[offset - 1].is_ascii_whitespace())
                })
    };
    if browser_launched {
        descendants.iter().any(|process| exact(&process.command))
            && descendants
                .iter()
                .all(|process| !profile_bearing(&process.command) || exact(&process.command))
    } else {
        page_count == 0
            && descendants
                .iter()
                .all(|process| !profile_bearing(&process.command))
    }
}

async fn daemon_has_no_profile_descendants(daemon_pid: u32) -> bool {
    let Some(processes) = process_list().await else {
        return false;
    };
    processes
        .iter()
        .filter(|process| is_descendant(process.pid, daemon_pid, &processes))
        .all(|process| {
            !process
                .command
                .windows(b"--user-data-dir".len())
                .enumerate()
                .any(|(offset, part)| {
                    part == b"--user-data-dir"
                        && (offset == 0 || process.command[offset - 1].is_ascii_whitespace())
                })
        })
}

struct ProcessRecord {
    pid: u32,
    ppid: u32,
    command: Vec<u8>,
}

async fn process_list() -> Option<Vec<ProcessRecord>> {
    let bytes =
        read_process_list(&["-axww", "-o", "pid=", "-o", "ppid=", "-o", "command="]).await?;
    let mut records = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let mut offset = 0;
        while line
            .get(offset)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            offset += 1;
        }
        if offset == line.len() {
            continue;
        }
        let pid_start = offset;
        while line.get(offset).is_some_and(|byte| byte.is_ascii_digit()) {
            offset += 1;
        }
        let pid = std::str::from_utf8(&line[pid_start..offset])
            .ok()?
            .parse()
            .ok()?;
        while line
            .get(offset)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            offset += 1;
        }
        let ppid_start = offset;
        while line.get(offset).is_some_and(|byte| byte.is_ascii_digit()) {
            offset += 1;
        }
        let ppid = std::str::from_utf8(&line[ppid_start..offset])
            .ok()?
            .parse()
            .ok()?;
        while line
            .get(offset)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            offset += 1;
        }
        records.push(ProcessRecord {
            pid,
            ppid,
            command: line[offset..].to_vec(),
        });
    }
    Some(records)
}

fn is_descendant(pid: u32, ancestor: u32, processes: &[ProcessRecord]) -> bool {
    let mut current = pid;
    for _ in 0..64 {
        let Some(process) = processes.iter().find(|process| process.pid == current) else {
            return false;
        };
        if process.ppid == ancestor {
            return true;
        }
        if process.ppid <= 1 || process.ppid == current {
            return false;
        }
        current = process.ppid;
    }
    false
}

async fn read_process_list(args: &[&str]) -> Option<Vec<u8>> {
    let mut command = Command::new("/bin/ps");
    command
        .env_clear()
        .args(args)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let mut bytes = Vec::new();
    (&mut stdout)
        .take((PROCESS_LIST_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .ok()?;
    if bytes.len() > PROCESS_LIST_LIMIT || !child.wait().await.ok()?.success() {
        return None;
    }
    Some(bytes)
}

fn contains_complete_argument(line: &[u8], needle: &[u8]) -> bool {
    line.windows(needle.len())
        .enumerate()
        .any(|(offset, value)| {
            value == needle
                && (offset == 0 || line[offset - 1].is_ascii_whitespace())
                && line
                    .get(offset + needle.len())
                    .is_none_or(|byte| byte.is_ascii_whitespace())
        })
}
