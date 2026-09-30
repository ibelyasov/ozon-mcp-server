//! One private local Broker generation shared by stdio frontends.
use crate::{
    error::{Code, fail},
    research::application::Application,
    runtime::config::Config,
    runtime::wire::ToolReply,
};
use anyhow::{Context, Result, ensure};
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

const PROTOCOL_VERSION: u32 = 2;
const MAX_REQUEST_BYTES: usize = 128 * 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CLIENTS: usize = 32;
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const CALL_TIMEOUT: Duration = Duration::from_secs(70);
const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Compatibility {
    protocol: u32,
    version: String,
    build: String,
    launch: String,
}
impl Compatibility {
    fn current(config: &Config) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").into(),
            build: env!("OZON_BUILD_ID").to_owned(),
            launch: config.fingerprint(),
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum BrokerRequest {
    Hello {
        compatibility: Compatibility,
    },
    Call {
        generation: String,
        name: String,
        args: Value,
    },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum BrokerResponse {
    Ready {
        compatibility: Compatibility,
        generation: String,
    },
    Reply {
        generation: String,
        reply: ToolReply,
    },
    Error {
        code: Code,
        message: String,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SocketIdentity {
    dev: u64,
    ino: u64,
}

pub async fn run(config: Config) -> Result<()> {
    let owner_lock = lock_owner(&config)?;
    prepare_socket(&config).await?;
    let listener =
        UnixListener::bind(&config.socket).context("Cannot bind private broker socket")?;
    std::fs::set_permissions(&config.socket, std::fs::Permissions::from_mode(0o600))?;
    let socket_identity = validate_socket(&config)?;
    let compatibility = Compatibility::current(&config);
    let generation = uuid::Uuid::new_v4().to_string();
    let service = match Application::new(&config).await {
        Ok(service) => Arc::new(service),
        Err(error) => {
            let _ = remove_owned_socket(&config, socket_identity);
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
                if joined.is_err() { eprintln!("broker client task failed"); }
                idle.as_mut().reset(tokio::time::Instant::now() + IDLE_TIMEOUT);
            }
            accepted = listener.accept(), if clients.len() < MAX_CLIENTS => {
                let (stream, _) = match accepted { Ok(accepted) => accepted, Err(_) => break };
                if verify_peer(&stream, config.owner_uid()?).is_err() { continue; }
                let service = service.clone();
                let cancel = shutdown.child_token();
                let compatibility = compatibility.clone();
                let generation = generation.clone();
                clients.spawn(async move { handle_client(stream, service, cancel, compatibility, generation).await });
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
    let socket_cleanup = remove_owned_socket(&config, socket_identity);
    drop(owner_lock);
    service_shutdown?;
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
        fail(Code::InvalidArgument, "Invalid tool name")
    );
    let mut stream = connect_or_start(config, &cancel).await?;
    let generation = handshake(&mut stream, &Compatibility::current(config), &cancel).await?;
    write_json_frame(
        &mut stream,
        &BrokerRequest::Call {
            generation: generation.clone(),
            name: name.to_owned(),
            args,
        },
        MAX_REQUEST_BYTES,
    )
    .await?;
    let response = tokio::select! {
        _ = cancel.cancelled() => return Err(fail(Code::Cancelled, "Broker request cancelled")),
        result = tokio::time::timeout(CALL_TIMEOUT, read_json_frame::<_, BrokerResponse>(&mut stream, MAX_RESPONSE_BYTES)) => result.map_err(|_| fail(Code::ServerBusy, "Broker response timed out"))??,
    };
    match response {
        BrokerResponse::Reply {
            generation: current,
            reply,
        } if current == generation => Ok(reply),
        BrokerResponse::Error { code, message } => Err(fail(code, bounded_message(&message))),
        _ => Err(fail(
            Code::SourceChanged,
            "Broker returned an incompatible generation or response",
        )),
    }
}

async fn handshake(
    stream: &mut UnixStream,
    compatibility: &Compatibility,
    cancel: &CancellationToken,
) -> Result<String> {
    write_json_frame(
        stream,
        &BrokerRequest::Hello {
            compatibility: compatibility.clone(),
        },
        MAX_REQUEST_BYTES,
    )
    .await?;
    let response = tokio::select! {
        _ = cancel.cancelled() => return Err(fail(Code::Cancelled, "Broker connection cancelled")),
        result = tokio::time::timeout(CONNECT_TIMEOUT, read_json_frame::<_, BrokerResponse>(stream, MAX_RESPONSE_BYTES)) => result.map_err(|_| fail(Code::ServerBusy, "Broker compatibility handshake timed out"))?.map_err(|_| fail(Code::SourceChanged, "The running Broker uses an unsupported protocol; close it before reconnecting"))?,
    };
    match response {
        BrokerResponse::Ready {
            compatibility: current,
            generation,
        } if current == *compatibility && uuid::Uuid::parse_str(&generation).is_ok() => {
            Ok(generation)
        }
        BrokerResponse::Error { code, message } => Err(fail(code, bounded_message(&message))),
        _ => Err(fail(
            Code::SourceChanged,
            "The running Broker has a different build or launch configuration; close it before reconnecting",
        )),
    }
}

async fn handle_client(
    mut stream: UnixStream,
    service: Arc<Application>,
    broker_shutdown: CancellationToken,
    compatibility: Compatibility,
    generation: String,
) -> Result<()> {
    let hello = tokio::time::timeout(
        REQUEST_READ_TIMEOUT,
        read_json_frame::<_, BrokerRequest>(&mut stream, MAX_REQUEST_BYTES),
    )
    .await
    .map_err(|_| fail(Code::InvalidArgument, "Broker handshake read timed out"))??;
    if !matches!(hello, BrokerRequest::Hello { compatibility: incoming } if incoming == compatibility)
    {
        return write_json_frame_bounded(&mut stream, &protocol_error(Code::SourceChanged, "The running Broker has a different build or launch configuration; close it before reconnecting"), MAX_RESPONSE_BYTES, RESPONSE_WRITE_TIMEOUT, &broker_shutdown).await;
    }
    write_json_frame_bounded(
        &mut stream,
        &BrokerResponse::Ready {
            compatibility,
            generation: generation.clone(),
        },
        MAX_RESPONSE_BYTES,
        RESPONSE_WRITE_TIMEOUT,
        &broker_shutdown,
    )
    .await?;
    let request = tokio::time::timeout(
        REQUEST_READ_TIMEOUT,
        read_json_frame::<_, BrokerRequest>(&mut stream, MAX_REQUEST_BYTES),
    )
    .await
    .map_err(|_| fail(Code::InvalidArgument, "Broker request read timed out"))??;
    let BrokerRequest::Call {
        generation: incoming,
        name,
        args,
    } = request
    else {
        return Err(fail(Code::InvalidArgument, "Expected a broker call"));
    };
    let (mut read, mut write) = stream.into_split();
    let response = if incoming != generation {
        protocol_error(Code::SourceChanged, "Broker generation changed")
    } else if name.is_empty() || name.len() > 128 {
        protocol_error(Code::InvalidArgument, "Invalid tool name")
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
            Some(reply) => BrokerResponse::Reply { generation, reply },
            None => return Ok(()),
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
    tokio::select! { _ = &mut disconnected => { request_cancel.cancel(); None }, _ = &mut shutdown => { request_cancel.cancel(); None }, value = &mut call => Some(value) }
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
    let mut command =
        Command::new(std::env::current_exe().context("Cannot locate broker executable")?);
    command
        .arg("--broker")
        .env("OZON_DATA_DIR", &config.root)
        .env("OZON_USER_DATA_DIR", &config.profile)
        .env(
            "OZON_IMAGE_DOH_FALLBACK",
            if config.image_doh_fallback {
                "on"
            } else {
                "off"
            },
        )
        .env(
            "OZON_HEADLESS",
            if config.browser.headless {
                "true"
            } else {
                "false"
            },
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(executable) = &config.browser.executable {
        command.env("OZON_BROWSER_EXECUTABLE", executable);
    } else {
        command.env_remove("OZON_BROWSER_EXECUTABLE");
    }
    let mut child = command.spawn().context("Cannot start local Ozon broker")?;
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        if child
            .try_wait()
            .context("Cannot inspect broker startup")?
            .is_some()
        {
            return Err(fail(Code::ServerBusy, "Broker exited during startup"));
        }
        if let Some(stream) = connect_if_present(config).await? {
            drop(bootstrap);
            return Ok(stream);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(fail(
                Code::ServerBusy,
                "Broker did not create its private socket within 5 seconds",
            ));
        }
        tokio::select! { _ = cancel.cancelled() => return Err(fail(Code::Cancelled, "Broker startup cancelled")), _ = tokio::time::sleep(Duration::from_millis(50)) => {} }
    }
}

async fn connect_if_present(config: &Config) -> Result<Option<UnixStream>> {
    match std::fs::symlink_metadata(&config.socket) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(_) => {
            let identity = validate_socket(config)?;
            match UnixStream::connect(&config.socket).await {
                Ok(stream) => {
                    verify_peer(&stream, config.owner_uid()?)?;
                    ensure!(
                        validate_socket(config)? == identity,
                        fail(Code::SourceChanged, "Broker socket changed during connect")
                    );
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

fn validate_socket(config: &Config) -> Result<SocketIdentity> {
    let metadata = std::fs::symlink_metadata(&config.socket).context("Broker socket is absent")?;
    ensure!(
        metadata.file_type().is_socket()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == config.owner_uid()?
            && metadata.permissions().mode() & 0o777 == 0o600,
        fail(
            Code::InvalidArgument,
            "Broker socket must be an owner-only Unix socket"
        )
    );
    Ok(SocketIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}
fn verify_peer(stream: &UnixStream, expected_uid: u32) -> Result<()> {
    ensure!(
        stream.peer_cred()?.uid() == expected_uid,
        fail(
            Code::InvalidArgument,
            "Broker peer belongs to another OS user"
        )
    );
    Ok(())
}
fn lock_owner(config: &Config) -> Result<File> {
    let lock = open_private_lock(&config.root.join("broker.lock"))?;
    lock.try_lock_exclusive().map_err(|error| {
        if error.kind() == ErrorKind::WouldBlock {
            fail(
                Code::ServerBusy,
                "An Ozon broker already owns this data directory",
            )
        } else {
            error.into()
        }
    })?;
    Ok(lock)
}
async fn acquire_bootstrap_lock(config: &Config, cancel: &CancellationToken) -> Result<File> {
    let lock = open_private_lock(&config.root.join("bootstrap.lock"))?;
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(lock),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(fail(
                Code::ServerBusy,
                "Broker startup is already in progress",
            ));
        }
        tokio::select! { _ = cancel.cancelled() => return Err(fail(Code::Cancelled, "Broker startup cancelled")), _ = tokio::time::sleep(Duration::from_millis(50)) => {} }
    }
}
fn open_private_lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    let named = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && named.uid() == metadata.uid()
            && metadata.dev() == named.dev()
            && metadata.ino() == named.ino()
            && metadata.permissions().mode() & 0o777 == 0o600,
        fail(
            Code::InvalidArgument,
            "Broker lock must be an unchanged owner-only file"
        )
    );
    Ok(file)
}
async fn prepare_socket(config: &Config) -> Result<()> {
    match std::fs::symlink_metadata(&config.socket) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => {
            let identity = validate_socket(config)?;
            if tokio::time::timeout(
                Duration::from_millis(250),
                UnixStream::connect(&config.socket),
            )
            .await
            .is_ok_and(|result| result.is_ok())
            {
                return Err(fail(
                    Code::ServerBusy,
                    "A live Broker already owns the socket",
                ));
            }
            remove_owned_socket(config, identity)
        }
    }
}
fn remove_owned_socket(config: &Config, identity: SocketIdentity) -> Result<()> {
    match std::fs::symlink_metadata(&config.socket) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => {
            ensure!(
                validate_socket(config)? == identity,
                fail(Code::SourceChanged, "Broker socket changed before cleanup")
            );
            std::fs::remove_file(&config.socket)?;
            Ok(())
        }
    }
}
fn protocol_error(code: Code, message: &str) -> BrokerResponse {
    BrokerResponse::Error {
        code,
        message: message.to_owned(),
    }
}
fn bounded_message(message: &str) -> String {
    message
        .chars()
        .filter(|c| !c.is_control())
        .take(1500)
        .collect()
}
async fn read_json_frame<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(
    reader: &mut R,
    maximum: usize,
) -> Result<T> {
    let length = reader
        .read_u32()
        .await
        .context("Cannot read broker frame length")? as usize;
    ensure!(
        length <= maximum,
        fail(Code::InvalidArgument, "Broker frame exceeds its byte limit")
    );
    let mut bytes = vec![0_u8; length];
    reader
        .read_exact(&mut bytes)
        .await
        .context("Cannot read complete broker frame")?;
    serde_json::from_slice(&bytes).map_err(|_| fail(Code::InvalidArgument, "Malformed broker JSON"))
}
async fn write_json_frame<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
    maximum: usize,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= maximum,
        fail(Code::ResultTooLarge, "Broker frame exceeds its byte limit")
    );
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}
async fn write_json_frame_bounded<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
    maximum: usize,
    deadline: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    tokio::select! { _ = cancel.cancelled() => Err(fail(Code::Cancelled, "Broker is shutting down")), result = tokio::time::timeout(deadline, write_json_frame(writer, value, maximum)) => result.map_err(|_| fail(Code::ServerBusy, "Broker response write timed out"))? }
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
        assert_eq!(crate::error::code(&error), "INVALID_ARGUMENT");
    }

    #[test]
    fn owner_lock_rejects_profile_broker_contention() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = Config::at(root.path().to_path_buf()).unwrap();
        let _first = lock_owner(&config).unwrap();
        assert_eq!(
            crate::error::code(&lock_owner(&config).unwrap_err()),
            "SERVER_BUSY"
        );
    }

    #[tokio::test]
    async fn framed_round_trip_preserves_generation_and_request() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let request = BrokerRequest::Call {
            generation: "generation".into(),
            name: "ozon_search".into(),
            args: serde_json::json!({"query":"mouse"}),
        };
        let (sent, received) = tokio::join!(
            write_json_frame(&mut client, &request, MAX_REQUEST_BYTES),
            read_json_frame::<_, BrokerRequest>(&mut server, MAX_REQUEST_BYTES)
        );
        sent.unwrap();
        assert!(
            matches!(received.unwrap(), BrokerRequest::Call { generation, name, .. } if generation == "generation" && name == "ozon_search")
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
        assert!(
            await_call_or_disconnect(
                disconnected,
                broker_shutdown.cancelled(),
                &request_cancel,
                std::future::pending::<()>()
            )
            .await
            .is_none()
        );
        assert!(request_cancel.is_cancelled());
        assert!(!broker_shutdown.is_cancelled());
    }

    #[tokio::test]
    async fn stalled_response_writer_is_bounded() {
        let (mut writer, _reader) = tokio::io::duplex(16);
        let response = protocol_error(Code::ServerBusy, &"x".repeat(4096));
        let error = write_json_frame_bounded(
            &mut writer,
            &response,
            MAX_RESPONSE_BYTES,
            Duration::from_millis(20),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(crate::error::code(&error), "SERVER_BUSY");
    }

    #[tokio::test]
    async fn two_frontends_use_one_compatible_private_listener() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = Config::at(root.path().to_path_buf()).unwrap();
        let listener = UnixListener::bind(&config.socket).unwrap();
        std::fs::set_permissions(&config.socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let compatibility = Compatibility::current(&config);
        let expected = compatibility.clone();
        let generation = uuid::Uuid::new_v4().to_string();
        let server_generation = generation.clone();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                assert!(
                    matches!(read_json_frame::<_, BrokerRequest>(&mut stream, MAX_REQUEST_BYTES).await.unwrap(), BrokerRequest::Hello { compatibility } if compatibility == expected)
                );
                write_json_frame(
                    &mut stream,
                    &BrokerResponse::Ready {
                        compatibility: expected.clone(),
                        generation: server_generation.clone(),
                    },
                    MAX_RESPONSE_BYTES,
                )
                .await
                .unwrap();
            }
        });
        let first = async {
            let mut stream = connect_if_present(&config).await.unwrap().unwrap();
            handshake(&mut stream, &compatibility, &CancellationToken::new())
                .await
                .unwrap()
        };
        let second = async {
            let mut stream = connect_if_present(&config).await.unwrap().unwrap();
            handshake(&mut stream, &compatibility, &CancellationToken::new())
                .await
                .unwrap()
        };
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first, generation);
        assert_eq!(second, generation);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn changed_build_or_launch_config_is_rejected_without_starting_another_broker() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = Config::at(root.path().to_path_buf()).unwrap();
        let compatibility = Compatibility::current(&config);
        for change in ["build", "launch", "version", "protocol"] {
            let listener = UnixListener::bind(&config.socket).unwrap();
            std::fs::set_permissions(&config.socket, std::fs::Permissions::from_mode(0o600))
                .unwrap();
            let mut incompatible = compatibility.clone();
            match change {
                "build" => incompatible.build.push('x'),
                "launch" => incompatible.launch.push('x'),
                "version" => incompatible.version.push('x'),
                _ => incompatible.protocol += 1,
            }
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _ = read_json_frame::<_, BrokerRequest>(&mut stream, MAX_REQUEST_BYTES)
                    .await
                    .unwrap();
                write_json_frame(
                    &mut stream,
                    &BrokerResponse::Ready {
                        compatibility: incompatible,
                        generation: uuid::Uuid::new_v4().to_string(),
                    },
                    MAX_RESPONSE_BYTES,
                )
                .await
                .unwrap();
            });
            let mut stream = connect_if_present(&config).await.unwrap().unwrap();
            assert_eq!(
                crate::error::code(
                    &handshake(&mut stream, &compatibility, &CancellationToken::new())
                        .await
                        .unwrap_err()
                ),
                "SOURCE_CHANGED"
            );
            server.await.unwrap();
            std::fs::remove_file(&config.socket).unwrap();
        }
    }

    #[tokio::test]
    async fn socket_cleanup_is_bound_to_the_owned_inode() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = Config::at(root.path().to_path_buf()).unwrap();
        let _owner = lock_owner(&config).unwrap();
        let first = UnixListener::bind(&config.socket).unwrap();
        std::fs::set_permissions(&config.socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let identity = validate_socket(&config).unwrap();
        std::fs::remove_file(&config.socket).unwrap();
        let _replacement = UnixListener::bind(&config.socket).unwrap();
        std::fs::set_permissions(&config.socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_ne!(validate_socket(&config).unwrap(), identity);
        assert!(remove_owned_socket(&config, identity).is_err());
        assert!(config.socket.exists());
        drop(first);
    }

    #[tokio::test]
    async fn stale_socket_cleanup_requires_owner_lock_and_checked_socket() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = Config::at(root.path().to_path_buf()).unwrap();
        let _owner = lock_owner(&config).unwrap();
        let stale = UnixListener::bind(&config.socket).unwrap();
        std::fs::set_permissions(&config.socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        drop(stale);
        prepare_socket(&config).await.unwrap();
        assert!(!config.socket.exists());
        std::fs::write(&config.socket, "unrelated file").unwrap();
        assert!(prepare_socket(&config).await.is_err());
        assert_eq!(
            std::fs::read_to_string(&config.socket).unwrap(),
            "unrelated file"
        );
    }
}
