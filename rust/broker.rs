use crate::{config::Config, service::Service, wire::ToolReply};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{File, OpenOptions},
    future::Future,
    io::ErrorKind,
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    process::Command,
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

const PROTOCOL_VERSION: u32 = 1;
const MAX_REQUEST_BYTES: usize = 128 * 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CLIENTS: usize = 32;
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const CALL_TIMEOUT: Duration = Duration::from_secs(70);
const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum BrokerRequest {
    Ping {
        version: u32,
    },
    Call {
        version: u32,
        name: String,
        args: Value,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BrokerResponse {
    Pong { version: u32 },
    Reply { version: u32, reply: ToolReply },
    Error { version: u32, message: String },
}

pub async fn run(config: Config) -> Result<()> {
    let owner_lock = lock_owner(&config)?;
    prepare_socket(&config).await?;
    let listener =
        UnixListener::bind(&config.socket).context("Cannot bind private broker socket")?;
    std::fs::set_permissions(&config.socket, std::fs::Permissions::from_mode(0o600))?;
    validate_socket(&config)?;

    let service = match Service::new(&config).await {
        Ok(service) => Arc::new(service),
        Err(error) => {
            let _ = remove_owned_socket(&config);
            return Err(error);
        }
    };
    let shutdown = CancellationToken::new();
    let mut clients = JoinSet::new();
    let mut idle = Box::pin(tokio::time::sleep(IDLE_TIMEOUT));

    loop {
        tokio::select! {
            biased;
            _ = shutdown_signal() => break,
            _ = &mut idle, if clients.is_empty() => break,
            Some(joined) = clients.join_next(), if !clients.is_empty() => {
                if let Err(error) = joined {
                    eprintln!("broker client task failed: {error}");
                }
                idle.as_mut().reset(tokio::time::Instant::now() + IDLE_TIMEOUT);
            }
            accepted = listener.accept(), if clients.len() < MAX_CLIENTS => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        eprintln!("broker listener failed: {error}");
                        break;
                    }
                };
                if let Err(error) = verify_peer(&stream, config.owner_uid()?) {
                    eprintln!("rejected broker peer: {error}");
                    continue;
                }
                let service = service.clone();
                let cancel = shutdown.child_token();
                clients.spawn(async move { handle_client(stream, service, cancel).await });
                idle.as_mut().reset(tokio::time::Instant::now() + IDLE_TIMEOUT);
            }
        }
    }

    shutdown.cancel();
    let drain = async { while clients.join_next().await.is_some() {} };
    if tokio::time::timeout(Duration::from_secs(10), drain)
        .await
        .is_err()
    {
        clients.abort_all();
        while clients.join_next().await.is_some() {}
    }
    let service_shutdown = service.shutdown().await;
    drop(listener);
    let socket_cleanup = remove_owned_socket(&config);
    drop(owner_lock);
    service_shutdown.context("BROWSER_CLEANUP_FAILED: broker service shutdown failed")?;
    socket_cleanup?;
    Ok(())
}

pub async fn forward(
    config: &Config,
    name: &str,
    args: Value,
    cancel: CancellationToken,
) -> Result<ToolReply> {
    ensure!(
        !name.is_empty() && name.len() <= 128,
        "INVALID_ARGUMENT: invalid tool name"
    );
    let mut stream = connect_or_start(config, &cancel).await?;
    let request = BrokerRequest::Call {
        version: PROTOCOL_VERSION,
        name: name.to_owned(),
        args,
    };
    write_json_frame(&mut stream, &request, MAX_REQUEST_BYTES).await?;
    let response = tokio::select! {
        _ = cancel.cancelled() => bail!("CANCELLED: broker request cancelled"),
        result = tokio::time::timeout(CALL_TIMEOUT, read_json_frame::<_, BrokerResponse>(&mut stream, MAX_RESPONSE_BYTES)) => {
            result.context("SERVER_BUSY: broker response timed out")??
        }
    };
    match response {
        BrokerResponse::Reply {
            version: PROTOCOL_VERSION,
            reply,
        } => Ok(reply),
        BrokerResponse::Error {
            version: PROTOCOL_VERSION,
            message,
        } => bail!("{message}"),
        BrokerResponse::Pong { .. } => {
            bail!("SOURCE_CHANGED: broker returned an unexpected response")
        }
        _ => bail!("SOURCE_CHANGED: incompatible broker protocol version"),
    }
}

#[cfg(test)]
async fn status(config: &Config) -> Result<bool> {
    let mut stream = match connect_if_present(config).await? {
        Some(stream) => stream,
        None => return Ok(false),
    };
    write_json_frame(
        &mut stream,
        &BrokerRequest::Ping {
            version: PROTOCOL_VERSION,
        },
        MAX_REQUEST_BYTES,
    )
    .await?;
    Ok(matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            read_json_frame(&mut stream, MAX_RESPONSE_BYTES)
        )
        .await,
        Ok(Ok(BrokerResponse::Pong {
            version: PROTOCOL_VERSION
        }))
    ))
}

async fn handle_client(
    stream: UnixStream,
    service: Arc<Service>,
    broker_shutdown: CancellationToken,
) -> Result<()> {
    let (mut read, mut write) = stream.into_split();
    let request = tokio::time::timeout(
        REQUEST_READ_TIMEOUT,
        read_json_frame::<_, BrokerRequest>(&mut read, MAX_REQUEST_BYTES),
    )
    .await
    .context("INVALID_ARGUMENT: timed out reading broker request")??;

    let response = match request {
        BrokerRequest::Ping { version } => {
            if version != PROTOCOL_VERSION {
                protocol_error("SOURCE_CHANGED: incompatible broker protocol version")
            } else {
                BrokerResponse::Pong {
                    version: PROTOCOL_VERSION,
                }
            }
        }
        BrokerRequest::Call {
            version,
            name,
            args,
        } => {
            if version != PROTOCOL_VERSION {
                protocol_error("SOURCE_CHANGED: incompatible broker protocol version")
            } else if name.is_empty() || name.len() > 128 {
                protocol_error("INVALID_ARGUMENT: invalid tool name")
            } else {
                let request_cancel = broker_shutdown.child_token();
                let call_cancel = request_cancel.clone();
                let disconnected = async {
                    let mut byte = [0_u8; 1];
                    loop {
                        match read.read(&mut byte).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                };
                match await_call_or_disconnect(
                    disconnected,
                    broker_shutdown.cancelled(),
                    &request_cancel,
                    service.call(&name, args, call_cancel),
                )
                .await
                {
                    Some(reply) => BrokerResponse::Reply {
                        version: PROTOCOL_VERSION,
                        reply,
                    },
                    None => return Ok(()),
                }
            }
        }
    };
    write_json_frame_bounded(
        &mut write,
        &response,
        MAX_RESPONSE_BYTES,
        RESPONSE_WRITE_TIMEOUT,
        &broker_shutdown,
    )
    .await
}

async fn await_call_or_disconnect<D, S, F, T>(
    disconnected: D,
    shutdown: S,
    request_cancel: &CancellationToken,
    call: F,
) -> Option<T>
where
    D: Future<Output = ()>,
    S: Future<Output = ()>,
    F: Future<Output = T>,
{
    tokio::pin!(disconnected);
    tokio::pin!(shutdown);
    tokio::pin!(call);
    tokio::select! {
        _ = &mut disconnected => {
            request_cancel.cancel();
            None
        }
        _ = &mut shutdown => {
            request_cancel.cancel();
            None
        }
        value = &mut call => Some(value),
    }
}

async fn connect_or_start(config: &Config, cancel: &CancellationToken) -> Result<UnixStream> {
    if let Some(stream) = connect_if_present(config).await? {
        return Ok(stream);
    }
    let bootstrap = acquire_bootstrap_lock(config, cancel).await?;
    if let Some(stream) = connect_if_present(config).await? {
        drop(bootstrap);
        return Ok(stream);
    }

    let executable = std::env::current_exe().context("Cannot locate broker executable")?;
    let mut child = Command::new(executable)
        .arg("--broker")
        .env("OZON_DATA_DIR", &config.root)
        .env("OZON_USER_DATA_DIR", &config.profile)
        .env("OZON_BROKER_SOCKET", &config.socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Cannot start local Ozon broker")?;

    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().context("Cannot inspect broker startup")? {
            bail!("SERVER_BUSY: broker exited during startup with {status}");
        }
        if let Some(stream) = connect_if_present(config).await? {
            drop(bootstrap);
            return Ok(stream);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("SERVER_BUSY: broker did not create its private socket within 5 seconds");
        }
        tokio::select! {
            _ = cancel.cancelled() => bail!("CANCELLED: broker startup cancelled"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}

async fn connect_if_present(config: &Config) -> Result<Option<UnixStream>> {
    match std::fs::symlink_metadata(&config.socket) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(_) => {
            validate_socket(config)?;
            match UnixStream::connect(&config.socket).await {
                Ok(stream) => {
                    verify_peer(&stream, config.owner_uid()?)?;
                    Ok(Some(stream))
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::ConnectionRefused | ErrorKind::NotFound
                    ) =>
                {
                    Ok(None)
                }
                Err(error) => Err(error).context("Cannot connect to broker socket"),
            }
        }
    }
}

fn validate_socket(config: &Config) -> Result<()> {
    let metadata = std::fs::symlink_metadata(&config.socket).context("Broker socket is absent")?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "INVALID_ARGUMENT: broker socket must not be a symbolic link"
    );
    ensure!(
        metadata.file_type().is_socket(),
        "INVALID_ARGUMENT: broker path is not a Unix socket"
    );
    ensure!(
        metadata.uid() == config.owner_uid()?,
        "INVALID_ARGUMENT: broker socket is owned by another OS user"
    );
    ensure!(
        metadata.permissions().mode() & 0o777 == 0o600,
        "INVALID_ARGUMENT: broker socket permissions must be 0600"
    );
    Ok(())
}

fn verify_peer(stream: &UnixStream, expected_uid: u32) -> Result<()> {
    ensure!(
        stream.peer_cred()?.uid() == expected_uid,
        "INVALID_ARGUMENT: broker peer belongs to another OS user"
    );
    Ok(())
}

fn lock_owner(config: &Config) -> Result<File> {
    let lock = open_private_lock(&config.root.join("broker.lock"))?;
    match lock.try_lock_exclusive() {
        Ok(()) => Ok(lock),
        Err(error) if error.kind() == ErrorKind::WouldBlock => {
            bail!("SERVER_BUSY: an Ozon broker already owns this data directory")
        }
        Err(error) => Err(error).context("Cannot lock broker owner file"),
    }
}

async fn acquire_bootstrap_lock(config: &Config, cancel: &CancellationToken) -> Result<File> {
    let lock = open_private_lock(&config.root.join("bootstrap.lock"))?;
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(lock),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => return Err(error).context("Cannot lock broker bootstrap file"),
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("SERVER_BUSY: broker startup is already in progress");
        }
        tokio::select! {
            _ = cancel.cancelled() => bail!("CANCELLED: broker startup cancelled"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}

fn open_private_lock(path: &Path) -> Result<File> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        ensure!(
            !metadata.file_type().is_symlink(),
            "INVALID_ARGUMENT: broker lock must not be a symbolic link"
        );
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    let path_metadata = std::fs::symlink_metadata(path)?;
    let parent = path
        .parent()
        .context("INVALID_ARGUMENT: broker lock has no parent")?;
    ensure!(
        metadata.uid() == path_metadata.uid()
            && metadata.uid() == std::fs::symlink_metadata(parent)?.uid()
            && metadata.dev() == path_metadata.dev()
            && metadata.ino() == path_metadata.ino(),
        "INVALID_ARGUMENT: broker lock path changed during open"
    );
    ensure!(
        metadata.permissions().mode() & 0o777 == 0o600,
        "INVALID_ARGUMENT: broker lock permissions must be 0600"
    );
    Ok(file)
}

async fn prepare_socket(config: &Config) -> Result<()> {
    match std::fs::symlink_metadata(&config.socket) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(metadata) => {
            ensure!(
                !metadata.file_type().is_symlink(),
                "INVALID_ARGUMENT: broker socket must not be a symbolic link"
            );
            ensure!(
                metadata.file_type().is_socket(),
                "INVALID_ARGUMENT: broker path is not a Unix socket"
            );
            ensure!(
                metadata.uid() == config.owner_uid()?,
                "INVALID_ARGUMENT: broker socket is owned by another OS user"
            );
            if tokio::time::timeout(
                Duration::from_millis(250),
                UnixStream::connect(&config.socket),
            )
            .await
            .is_ok_and(|r| r.is_ok())
            {
                bail!("SERVER_BUSY: a live broker already owns the socket")
            }
            std::fs::remove_file(&config.socket).context("Cannot remove stale broker socket")?;
            Ok(())
        }
    }
}

fn remove_owned_socket(config: &Config) -> Result<()> {
    match std::fs::symlink_metadata(&config.socket) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_socket() && metadata.uid() == config.owner_uid()?,
                "SOURCE_CHANGED: broker socket path changed during shutdown"
            );
            std::fs::remove_file(&config.socket)?;
            Ok(())
        }
    }
}

fn protocol_error(message: &str) -> BrokerResponse {
    BrokerResponse::Error {
        version: PROTOCOL_VERSION,
        message: message.to_owned(),
    }
}

async fn read_json_frame<R, T>(reader: &mut R, maximum: usize) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let length = reader
        .read_u32()
        .await
        .context("Cannot read broker frame length")? as usize;
    ensure!(
        length <= maximum,
        "INVALID_ARGUMENT: broker frame exceeds {maximum} bytes"
    );
    let mut bytes = vec![0_u8; length];
    reader
        .read_exact(&mut bytes)
        .await
        .context("Cannot read complete broker frame")?;
    serde_json::from_slice(&bytes).context("INVALID_ARGUMENT: malformed broker JSON")
}

async fn write_json_frame<W, T>(writer: &mut W, value: &T, maximum: usize) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= maximum,
        "INVALID_ARGUMENT: broker frame exceeds {maximum} bytes"
    );
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn write_json_frame_bounded<W, T>(
    writer: &mut W,
    value: &T,
    maximum: usize,
    deadline: Duration,
    cancel: &CancellationToken,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    tokio::select! {
        _ = cancel.cancelled() => bail!("CANCELLED: broker is shutting down"),
        result = tokio::time::timeout(deadline, write_json_frame(writer, value, maximum)) => {
            result.context("SERVER_BUSY: broker response write timed out")?
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_body_read() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer
            .write_u32((MAX_REQUEST_BYTES + 1) as u32)
            .await
            .unwrap();
        let error = read_json_frame::<_, BrokerRequest>(&mut reader, MAX_REQUEST_BYTES)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[cfg(unix)]
    #[test]
    fn owner_lock_rejects_profile_broker_contention() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = Config::at(root.path().to_path_buf()).unwrap();
        let _first = lock_owner(&config).unwrap();
        let error = lock_owner(&config).unwrap_err();
        assert!(error.to_string().starts_with("SERVER_BUSY:"));
    }

    #[tokio::test]
    async fn framed_round_trip_preserves_request() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let request = BrokerRequest::Call {
            version: PROTOCOL_VERSION,
            name: "ozon_search".to_owned(),
            args: serde_json::json!({"query":"mouse"}),
        };
        let send = write_json_frame(&mut client, &request, MAX_REQUEST_BYTES);
        let receive = read_json_frame::<_, BrokerRequest>(&mut server, MAX_REQUEST_BYTES);
        let (sent, received) = tokio::join!(send, receive);
        sent.unwrap();
        assert!(
            matches!(received.unwrap(), BrokerRequest::Call { name, .. } if name == "ozon_search")
        );
    }

    #[tokio::test]
    async fn disconnect_cancels_only_its_request() {
        let (client, mut server) = tokio::io::duplex(16);
        let request_cancel = CancellationToken::new();
        let broker_shutdown = CancellationToken::new();
        drop(client);
        let disconnected = async {
            let mut byte = [0_u8; 1];
            let _ = server.read(&mut byte).await;
        };
        let result = await_call_or_disconnect(
            disconnected,
            broker_shutdown.cancelled(),
            &request_cancel,
            std::future::pending::<()>(),
        )
        .await;
        assert!(result.is_none());
        assert!(request_cancel.is_cancelled());
        assert!(!broker_shutdown.is_cancelled());
    }

    #[tokio::test]
    async fn stalled_response_writer_is_bounded() {
        let (mut writer, _reader) = tokio::io::duplex(16);
        let response = BrokerResponse::Error {
            version: PROTOCOL_VERSION,
            message: "x".repeat(4096),
        };
        let error = write_json_frame_bounded(
            &mut writer,
            &response,
            MAX_RESPONSE_BYTES,
            Duration::from_millis(20),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("write timed out"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn two_frontends_use_one_private_listener() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = Config::at(root.path().to_path_buf()).unwrap();
        let listener = UnixListener::bind(&config.socket).unwrap();
        std::fs::set_permissions(&config.socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        validate_socket(&config).unwrap();

        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                assert!(matches!(
                    read_json_frame::<_, BrokerRequest>(&mut stream, MAX_REQUEST_BYTES)
                        .await
                        .unwrap(),
                    BrokerRequest::Ping {
                        version: PROTOCOL_VERSION
                    }
                ));
                write_json_frame(
                    &mut stream,
                    &BrokerResponse::Pong {
                        version: PROTOCOL_VERSION,
                    },
                    MAX_RESPONSE_BYTES,
                )
                .await
                .unwrap();
            }
        });
        let (first, second) = tokio::join!(status(&config), status(&config));
        assert!(first.unwrap());
        assert!(second.unwrap());
        server.await.unwrap();
    }
}
