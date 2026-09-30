use sha2::{Digest, Sha256};
use std::{
    env, fs,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
};

fn collect(directory: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(&entry.path(), files)?;
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

fn record(digest: &mut Sha256, name: &str, value: &[u8]) {
    digest.update((name.len() as u64).to_be_bytes());
    digest.update(name.as_bytes());
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut files = Vec::new();
    for directory in ["src", "contracts/schemas"] {
        println!("cargo::rerun-if-changed={directory}");
        collect(Path::new(directory), &mut files)?;
    }
    files.extend(
        [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "build.rs",
            "browser/extract.js",
        ]
        .map(PathBuf::from),
    );
    files.sort();
    let mut digest = Sha256::new();
    for file in files {
        println!("cargo::rerun-if-changed={}", file.display());
        record(
            &mut digest,
            file.to_str().ok_or("non-UTF8 source filename")?,
            &fs::read(&file)?,
        );
    }
    let mut configuration: Vec<_> = env::vars_os()
        .filter(|(name, _)| {
            let Some(name) = name.to_str() else {
                return false;
            };
            name.starts_with("CARGO_CFG_")
                || name.starts_with("CARGO_FEATURE_")
                || matches!(
                    name,
                    "TARGET"
                        | "HOST"
                        | "PROFILE"
                        | "OPT_LEVEL"
                        | "DEBUG"
                        | "RUSTC"
                        | "RUSTC_WRAPPER"
                        | "RUSTC_WORKSPACE_WRAPPER"
                        | "RUSTFLAGS"
                        | "CARGO_ENCODED_RUSTFLAGS"
                )
        })
        .collect();
    configuration.sort();
    for (name, value) in configuration {
        let name = name.to_str().ok_or("non-UTF8 build configuration name")?;
        println!("cargo::rerun-if-env-changed={name}");
        record(&mut digest, name, value.as_os_str().as_bytes());
    }
    let compiler = Command::new(env::var_os("RUSTC").ok_or("missing Cargo compiler")?)
        .arg("--version")
        .output()?;
    if !compiler.status.success() {
        return Err("cannot identify the Cargo compiler".into());
    }
    record(&mut digest, "compiler-version", &compiler.stdout);
    println!("cargo::rustc-env=OZON_BUILD_ID={:x}", digest.finalize());
    Ok(())
}
