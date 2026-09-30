//! Exact OS process identities for owned Chromium recovery. No process-list heuristics.
use crate::runtime::browser::error::BrowserError;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProcessIdentity {
    pub pid: u32,
    pub start_seconds: u64,
    pub start_fraction: u64,
    pub executable: PathBuf,
}

impl ProcessIdentity {
    pub(super) fn capture(pid: u32, executable: &Path, profile: &Path) -> Result<Self> {
        let (identity, args) = inspect(pid)?.ok_or(BrowserError::CleanupFailed)?;
        ensure!(
            identity.executable == executable && owns_profile(&args, profile),
            BrowserError::CleanupFailed
        );
        Ok(identity)
    }

    pub(super) fn still_matches(&self, profile: &Path) -> Result<bool> {
        Ok(inspect(self.pid)?
            .is_some_and(|(current, args)| current == *self && owns_profile(&args, profile)))
    }

    pub(super) fn confirmed_absent(&self) -> Result<bool> {
        // A reused PID is not our process. It must never be signalled or closed.
        Ok(inspect(self.pid)?.is_none_or(|(current, _)| current != *self))
    }

    pub(super) async fn wait_absent(&self, timeout: Duration) -> Result<bool> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.confirmed_absent()? {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn owns_profile(args: &[Vec<u8>], profile: &Path) -> bool {
    let bytes = profile.as_os_str().as_encoded_bytes();
    args.iter().enumerate().any(|(index, arg)| {
        arg.strip_prefix(b"--user-data-dir=")
            .is_some_and(|path| path == bytes)
            || (arg == b"--user-data-dir" && args.get(index + 1).is_some_and(|path| path == bytes))
    })
}

#[cfg(target_os = "macos")]
fn inspect(pid: u32) -> Result<Option<(ProcessIdentity, Vec<Vec<u8>>)>> {
    use std::mem::{MaybeUninit, size_of};
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    // SAFETY: proc_pidinfo writes exactly the supplied initialized allocation.
    let size = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<libc::proc_bsdinfo>() as i32,
        )
    };
    if size <= 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        // The kernel may return zero with no errno after the process exits.
        if !process_exists(pid)? {
            return Ok(None);
        }
        return Err(BrowserError::CleanupFailed.into());
    }
    ensure!(
        size as usize == size_of::<libc::proc_bsdinfo>(),
        BrowserError::CleanupFailed
    );
    // SAFETY: the kernel filled the complete proc_bsdinfo.
    let info = unsafe { info.assume_init() };
    ensure!(
        info.pbi_uid == unsafe { libc::geteuid() } && info.pbi_pid == pid,
        BrowserError::CleanupFailed
    );
    let mut path = vec![0_u8; 4096];
    // SAFETY: the writable buffer has the supplied length.
    let length =
        unsafe { libc::proc_pidpath(pid as i32, path.as_mut_ptr().cast(), path.len() as u32) };
    ensure!(length > 0, BrowserError::CleanupFailed);
    path.truncate(path.iter().position(|b| *b == 0).unwrap_or(length as usize));
    use std::os::unix::ffi::OsStringExt;
    let executable = std::fs::canonicalize(PathBuf::from(std::ffi::OsString::from_vec(path)))
        .map_err(|_| BrowserError::CleanupFailed)?;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
    let mut args = vec![0_u8; 256 * 1024];
    let mut length = args.len();
    // SAFETY: sysctl receives fixed MIB and a writable bounded buffer; no writes to kernel settings.
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            args.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    ensure!(
        status == 0 && length >= size_of::<i32>() && length <= args.len(),
        BrowserError::CleanupFailed
    );
    args.truncate(length);
    let argc = i32::from_ne_bytes(args[..4].try_into().unwrap());
    ensure!((1..=4096).contains(&argc), BrowserError::CleanupFailed);
    let mut offset = 4;
    // Skip the kernel executable path and its padding, then read exactly argc args.
    offset += args[offset..]
        .iter()
        .position(|b| *b == 0)
        .ok_or(BrowserError::CleanupFailed)?
        + 1;
    while args.get(offset) == Some(&0) {
        offset += 1;
    }
    let mut argv = Vec::new();
    for _ in 0..argc {
        let length = args
            .get(offset..)
            .and_then(|tail| tail.iter().position(|b| *b == 0))
            .ok_or(BrowserError::CleanupFailed)?;
        argv.push(args[offset..offset + length].to_vec());
        offset += length + 1;
    }
    Ok(Some((
        ProcessIdentity {
            pid,
            start_seconds: info.pbi_start_tvsec,
            start_fraction: info.pbi_start_tvusec,
            executable,
        },
        argv,
    )))
}

#[cfg(target_os = "linux")]
fn inspect(pid: u32) -> Result<Option<(ProcessIdentity, Vec<Vec<u8>>)>> {
    use std::os::unix::fs::MetadataExt;
    let root = PathBuf::from(format!("/proc/{pid}"));
    let metadata = match std::fs::metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(BrowserError::CleanupFailed.into()),
    };
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        BrowserError::CleanupFailed
    );
    let stat =
        std::fs::read_to_string(root.join("stat")).map_err(|_| BrowserError::CleanupFailed)?;
    let tail = stat.rsplit_once(") ").ok_or(BrowserError::CleanupFailed)?.1;
    let start_seconds = tail
        .split_whitespace()
        .nth(19)
        .ok_or(BrowserError::CleanupFailed)?
        .parse::<u64>()
        .map_err(|_| BrowserError::CleanupFailed)?;
    let executable =
        std::fs::canonicalize(root.join("exe")).map_err(|_| BrowserError::CleanupFailed)?;
    let argv = std::fs::read(root.join("cmdline"))
        .map_err(|_| BrowserError::CleanupFailed)?
        .split(|b| *b == 0)
        .filter(|arg| !arg.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    Ok(Some((
        ProcessIdentity {
            pid,
            start_seconds,
            start_fraction: 0,
            executable,
        },
        argv,
    )))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn inspect(_pid: u32) -> Result<Option<(ProcessIdentity, Vec<Vec<u8>>)>> {
    Err(BrowserError::CleanupFailed.into())
}

#[cfg(target_os = "macos")]
fn process_exists(pid: u32) -> Result<bool> {
    // SAFETY: signal zero checks existence without signalling the process.
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return Ok(true);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        _ => Err(BrowserError::CleanupFailed.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_match_is_an_exact_argument() {
        let profile = Path::new("/private/test/profile");
        assert!(owns_profile(
            &[
                b"chrome".to_vec(),
                b"--user-data-dir=/private/test/profile".to_vec()
            ],
            profile
        ));
        assert!(!owns_profile(
            &[b"--user-data-dir=/private/test/profile-other".to_vec()],
            profile
        ));
        assert!(!owns_profile(
            &[b"--title=/private/test/profile".to_vec()],
            profile
        ));
    }
    #[test]
    fn unrelated_process_is_never_owned_or_signalled() {
        let executable = std::env::current_exe().unwrap();
        assert!(
            ProcessIdentity::capture(
                std::process::id(),
                &executable,
                Path::new("/profile/not-in-args")
            )
            .is_err()
        );
    }
}
