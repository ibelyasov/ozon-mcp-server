use crate::error::{Code, fail};
use anyhow::{Context, Result, ensure};
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

const DEFAULT_DIRECTORY: &str = "ozon-mcp-server";
const SOCKET_NAME: &str = "broker.sock";
const MAX_UNIX_SOCKET_PATH_BYTES: usize = 103;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub root: PathBuf,
    pub profile: PathBuf,
    pub socket: PathBuf,
    pub browser: BrowserConfig,
    pub image_doh_fallback: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowserConfig {
    pub executable: Option<PathBuf>,
    pub headless: bool,
}

impl BrowserConfig {
    fn from_env() -> Result<Self> {
        let headless = parse_headless(std::env::var_os("OZON_HEADLESS").as_deref())?;
        let executable = std::env::var_os("OZON_BROWSER_EXECUTABLE")
            .map(PathBuf::from)
            .map(|path| {
                ensure!(
                    path.is_absolute(),
                    crate::error::RuntimeError::configuration(
                        "OZON_BROWSER_EXECUTABLE must be absolute"
                    )
                );
                let path = std::fs::canonicalize(path).map_err(|_| {
                    crate::error::RuntimeError::configuration(
                        "OZON_BROWSER_EXECUTABLE does not exist",
                    )
                })?;
                let metadata = std::fs::metadata(&path)?;
                ensure!(
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
                    crate::error::RuntimeError::configuration(
                        "OZON_BROWSER_EXECUTABLE must be executable"
                    )
                );
                Ok(path)
            })
            .transpose()?;
        Ok(Self {
            executable,
            headless,
        })
    }
}

fn parse_headless(value: Option<&std::ffi::OsStr>) -> Result<bool> {
    match value.and_then(std::ffi::OsStr::to_str) {
        None if value.is_none() => Ok(true),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        _ => Err(crate::error::RuntimeError::configuration(
            "OZON_HEADLESS must be exactly true or false",
        )
        .into()),
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let root = std::env::var_os("OZON_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(default_root);
        let profile = std::env::var_os("OZON_USER_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("browser-profile"));
        let socket = root.join(SOCKET_NAME);
        let mut config = Self::new(root, profile, socket)?;
        config.browser = BrowserConfig::from_env()?;
        let doh =
            std::env::var("OZON_IMAGE_DOH_FALLBACK")
                .map(Some)
                .or_else(|error| match error {
                    std::env::VarError::NotPresent => Ok(None),
                    std::env::VarError::NotUnicode(_) => Err(fail(
                        Code::InvalidConfiguration,
                        "OZON_IMAGE_DOH_FALLBACK must be on or off",
                    )),
                })?;
        config.image_doh_fallback = crate::images::parse_doh_fallback(doh.as_deref())?;
        Ok(config)
    }

    #[cfg(test)]
    pub fn at(root: PathBuf) -> Result<Self> {
        let profile = root.join("browser-profile");
        let socket = root.join(SOCKET_NAME);
        Self::new(root, profile, socket)
    }

    fn new(root: PathBuf, profile: PathBuf, socket: PathBuf) -> Result<Self> {
        ensure!(
            root.is_absolute(),
            fail(Code::InvalidArgument, "OZON_DATA_DIR must be absolute")
        );
        ensure!(
            profile.is_absolute(),
            fail(Code::InvalidArgument, "OZON_USER_DATA_DIR must be absolute")
        );
        ensure!(
            socket.is_absolute(),
            fail(Code::InvalidArgument, "broker socket must be absolute")
        );
        ensure_no_parent_components(&root, "OZON_DATA_DIR")?;
        ensure_no_parent_components(&profile, "OZON_USER_DATA_DIR")?;
        ensure_no_parent_components(&socket, "broker socket")?;
        reject_symlink_components(&root, "data directory")?;
        reject_symlink_components(&profile, "browser profile")?;
        reject_symlink_components(&socket, "broker socket")?;
        ensure_socket_parent(&root, &socket)?;
        ensure!(
            socket.as_os_str().as_encoded_bytes().len() <= MAX_UNIX_SOCKET_PATH_BYTES,
            fail(
                Code::InvalidArgument,
                "broker socket path is too long; set OZON_DATA_DIR to a shorter private path"
            )
        );
        reject_existing_symlink(&profile, "browser profile")?;
        reject_existing_symlink(&socket, "broker socket")?;
        ensure_private_root(&root)?;
        Ok(Self {
            root,
            profile,
            socket,
            image_doh_fallback: false,
            browser: BrowserConfig {
                executable: None,
                headless: true,
            },
        })
    }

    pub(crate) fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        for value in [self.root.as_os_str(), self.profile.as_os_str()] {
            let bytes = value.as_encoded_bytes();
            digest.update((bytes.len() as u64).to_be_bytes());
            digest.update(bytes);
        }
        if let Some(executable) = &self.browser.executable {
            digest.update(executable.as_os_str().as_encoded_bytes());
        }
        digest.update([
            u8::from(self.browser.headless),
            u8::from(self.image_doh_fallback),
        ]);
        format!("{:x}", digest.finalize())
    }

    pub(crate) fn owner_uid(&self) -> Result<u32> {
        owner_uid(&self.root)
    }
}

fn reject_symlink_components(path: &Path, label: &str) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                #[cfg(unix)]
                let trusted_system_link =
                    metadata.file_type().is_symlink() && metadata.uid() == 0 && current != path;
                #[cfg(not(unix))]
                let trusted_system_link = false;
                ensure!(
                    !metadata.file_type().is_symlink() || trusted_system_link,
                    fail(
                        Code::InvalidArgument,
                        format!("{label} path contains a symbolic link")
                    )
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error).with_context(|| format!("Cannot inspect {label}")),
        }
    }
    Ok(())
}

fn default_root() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    #[cfg(target_os = "macos")]
    return home
        .join("Library/Application Support")
        .join(DEFAULT_DIRECTORY);
    #[cfg(not(target_os = "macos"))]
    return std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join(DEFAULT_DIRECTORY);
}

fn ensure_no_parent_components(path: &Path, name: &str) -> Result<()> {
    ensure!(
        !path
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        fail(
            Code::InvalidArgument,
            format!("{name} must not contain '..'")
        )
    );
    Ok(())
}

pub(crate) fn ensure_private_profile(path: &Path) -> Result<()> {
    reject_symlink_components(path, "browser profile")?;
    ensure_private_root(path)
}

fn ensure_private_root(path: &Path) -> Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        ensure!(
            !metadata.file_type().is_symlink(),
            fail(
                Code::InvalidArgument,
                "data directory must not be a symbolic link"
            )
        );
        ensure!(
            metadata.is_dir(),
            fail(Code::InvalidArgument, "data directory is not a directory")
        );
        validate_owner(&metadata, "data directory")?;
        #[cfg(unix)]
        ensure!(
            metadata.permissions().mode() & 0o777 == 0o700,
            fail(
                Code::InvalidArgument,
                "data directory must have owner-only permissions (0700)"
            )
        );
        return Ok(());
    }

    let mut missing = Vec::new();
    let mut ancestor = path;
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    fail(
                        Code::InvalidArgument,
                        "data directory ancestor must not be a symbolic link"
                    )
                );
                ensure!(
                    metadata.is_dir(),
                    fail(
                        Code::InvalidArgument,
                        "data directory ancestor is not a directory"
                    )
                );
                validate_owner(&metadata, "data directory ancestor")?;
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.to_path_buf());
                ancestor = ancestor.parent().context(fail(
                    Code::InvalidArgument,
                    "data directory has no existing ancestor",
                ))?;
            }
            Err(error) => return Err(error).context("Cannot inspect data directory"),
        }
    }
    for directory in missing.iter().rev() {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        match builder.create(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("Cannot create private data directory"),
        }
        let metadata = std::fs::symlink_metadata(directory)?;
        ensure!(
            !metadata.file_type().is_symlink() && metadata.is_dir(),
            fail(
                Code::InvalidArgument,
                "data directory path changed during creation"
            )
        );
        validate_owner(&metadata, "data directory")?;
        #[cfg(unix)]
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn ensure_socket_parent(root: &Path, socket: &Path) -> Result<()> {
    let parent = socket
        .parent()
        .context(fail(Code::InvalidArgument, "broker socket has no parent"))?;
    ensure!(
        parent == root,
        fail(
            Code::InvalidArgument,
            "broker socket must be directly inside OZON_DATA_DIR"
        )
    );
    Ok(())
}

fn reject_existing_symlink(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                !metadata.file_type().is_symlink(),
                fail(
                    Code::InvalidArgument,
                    format!("{label} path must not be a symbolic link")
                )
            );
            validate_owner(&metadata, label)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("Cannot inspect {label} path")),
    }
    Ok(())
}

fn owner_uid(path: &Path) -> Result<u32> {
    #[cfg(unix)]
    {
        Ok(std::fs::symlink_metadata(path)?.uid())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        anyhow::bail!(fail(Code::InvalidArgument, "local broker requires Unix"))
    }
}

fn validate_owner(metadata: &std::fs::Metadata, label: &str) -> Result<()> {
    #[cfg(unix)]
    ensure!(
        metadata.uid() == effective_uid(),
        fail(
            Code::InvalidArgument,
            format!("{label} is owned by another OS user")
        )
    );
    Ok(())
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid takes no arguments and has no preconditions.
    unsafe { geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_is_default_and_visible_mode_requires_exact_false() {
        assert!(parse_headless(None).unwrap());
        assert!(parse_headless(Some(std::ffi::OsStr::new("true"))).unwrap());
        assert!(!parse_headless(Some(std::ffi::OsStr::new("false"))).unwrap());
        for invalid in ["False", " true", "", "0"] {
            assert!(parse_headless(Some(std::ffi::OsStr::new(invalid))).is_err());
        }
    }

    #[test]
    fn fingerprint_binds_launch_configuration() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = Config::at(root.path().to_path_buf()).unwrap();
        let mut visible = config.clone();
        visible.browser.headless = false;
        assert_ne!(config.fingerprint(), visible.fingerprint());
        let mut executable = config.clone();
        executable.browser.executable = Some(PathBuf::from("/test/chromium"));
        assert_ne!(config.fingerprint(), executable.fingerprint());
        let mut doh = config.clone();
        doh.image_doh_fallback = true;
        assert_ne!(config.fingerprint(), doh.fingerprint());
    }

    #[cfg(unix)]
    #[test]
    fn creates_private_root_and_fixed_paths() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("nested/data");
        let config = Config::at(root.clone()).unwrap();
        assert_eq!(config.profile, root.join("browser-profile"));
        assert_eq!(config.socket, root.join("broker.sock"));
        assert_eq!(
            std::fs::metadata(root).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_public_or_symlinked_root() {
        use std::os::unix::fs::symlink;
        let parent = tempfile::tempdir().unwrap();
        let public = parent.path().join("public");
        std::fs::create_dir(&public).unwrap();
        std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Config::at(public).unwrap_err().to_string().contains("0700"));

        let target = parent.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = parent.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(
            Config::at(link)
                .unwrap_err()
                .to_string()
                .contains("symbolic link")
        );
    }

    #[test]
    fn rejects_long_socket_path_with_actionable_error() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("x".repeat(110));
        let error = Config::at(root).unwrap_err();
        assert!(error.to_string().contains("OZON_DATA_DIR"));
    }
}
