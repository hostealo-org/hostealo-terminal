//! FTP and FTPS (explicit, AUTH TLS) file browser sessions.
//!
//! Same shape as SFTP: `ftp_open` returns an id and connects in the background, reporting
//! through `SshEvent`s; file operations are separate commands. FTP has a single control
//! connection, so browsing commands are serialised, and each transfer opens its own connection
//! so browsing keeps working while files move.
//!
//! FTPS certificates are verified against the system roots. Shared hosting often presents a
//! certificate for another name, or a self-signed one; then the user sees its fingerprint and
//! can trust it, and that exact certificate is remembered (like FileZilla does).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use suppaftp::list::{File as ListFile, ListParser, PosixPexQuery};
use suppaftp::tokio::{AsyncRustlsConnector, AsyncRustlsFtpStream};
use suppaftp::types::FileType;
use suppaftp::{FtpError, Status};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::hosts::Protocol;
use crate::secrets::{self, SecretKind};
use crate::ssh::sftp::{Progress, SftpEntry, TransferProgress};
use crate::ssh::{
    Cancelled, Connector, PromptField, PromptKind, PromptRegistry, PromptReply, SshEvent, Stage,
};
use crate::tls::{self, TrustStore, UntrustedCertificate};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const CHUNK: usize = 256 * 1024;
const MAX_ATTEMPTS: usize = 3;
/// Each transfer uses its own connection, and shared hosting often allows only a few per user,
/// so transfers beyond this wait for a free slot (FileZilla's default too).
const MAX_PARALLEL_TRANSFERS: usize = 2;
/// Idle FTP servers drop the control connection; check it before reusing it after this long.
const IDLE_CHECK: Duration = Duration::from_secs(30);

// ── Connections ─────────────────────────────────────────────

#[derive(Clone)]
struct Params {
    host: String,
    port: u16,
    user: String,
    password: String,
    tls: bool,
    /// Fingerprint of a certificate the user trusted for this server.
    trusted: Option<String>,
}

impl Params {
    /// Key in the TLS trust store.
    fn key(&self) -> String {
        format!("ftp:{}:{}", self.host, self.port)
    }
}

async fn connect(params: &Params) -> Result<AsyncRustlsFtpStream> {
    let address = (params.host.as_str(), params.port);
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address))
        .await
        .with_context(|| format!("timed out connecting to {}:{}", params.host, params.port))?
        .with_context(|| format!("could not connect to {}:{}", params.host, params.port))?;
    let mut ftp = AsyncRustlsFtpStream::connect_with_stream(tcp).await?;

    if params.tls {
        let (config, verifier) = tls::client_config(params.trusted.clone())?;
        let connector = AsyncRustlsConnector::from(tokio_rustls::TlsConnector::from(config));
        ftp = match ftp.into_secure(connector, &params.host).await {
            Ok(secure) => secure,
            Err(error) => {
                if let Some(rejected) = verifier.take_rejected() {
                    return Err(UntrustedCertificate(rejected).into());
                }
                return Err(error).context("the server did not accept TLS (AUTH TLS)");
            }
        };
    }

    // Servers behind NAT often announce a private address for passive mode.
    ftp.set_passive_nat_workaround(true);
    ftp.login(params.user.as_str(), params.password.as_str())
        .await?;
    ftp.transfer_type(FileType::Binary).await?;
    Ok(ftp)
}

fn is_login_failure(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<FtpError>(), Some(FtpError::UnexpectedResponse(r)) if r.status == Status::NotLoggedIn)
}

struct FtpSession {
    control: tokio::sync::Mutex<AsyncRustlsFtpStream>,
    last_used: Mutex<Instant>,
    params: Params,
    mlsd: bool,
    transfer_slots: tokio::sync::Semaphore,
}

impl FtpSession {
    /// The control connection, reconnecting first if the server dropped it while idle.
    async fn control(&self) -> Result<tokio::sync::MutexGuard<'_, AsyncRustlsFtpStream>> {
        let mut ftp = self.control.lock().await;
        let idle = self
            .last_used
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default();
        if idle > IDLE_CHECK && ftp.noop().await.is_err() {
            *ftp = connect(&self.params).await.context("reconnecting")?;
        }
        if let Ok(mut last_used) = self.last_used.lock() {
            *last_used = Instant::now();
        }
        Ok(ftp)
    }
}

enum Slot {
    Connecting(tauri::async_runtime::JoinHandle<()>),
    Ready(Arc<FtpSession>),
}

#[derive(Default)]
pub struct FtpState {
    sessions: Mutex<HashMap<u32, Slot>>,
    transfers: Mutex<HashMap<u32, Arc<AtomicBool>>>,
    next_id: AtomicU32,
}

impl FtpState {
    fn get(&self, id: u32) -> Result<Arc<FtpSession>, String> {
        match self.sessions.lock().map_err(|e| e.to_string())?.get(&id) {
            Some(Slot::Ready(session)) => Ok(session.clone()),
            Some(Slot::Connecting(_)) => Err("not connected yet".into()),
            None => Err("session closed".into()),
        }
    }
}

/// Connects and logs in, asking for the password and about untrusted certificates as needed.
async fn open(trust: &TrustStore, connector: &Connector) -> Result<FtpSession> {
    let host = &connector.host;
    let prompter = &connector.prompter;
    prompter.notify(SshEvent::Status {
        stage: Stage::Connecting,
    });

    let anonymous = matches!(host.username.as_str(), "anonymous" | "ftp");
    let mut params = Params {
        host: host.hostname.clone(),
        port: host.port,
        user: host.username.clone(),
        password: String::new(),
        tls: host.protocol == Protocol::Ftps,
        trusted: None,
    };
    params.trusted = trust.get(&params.key());

    let mut password = if host.has_password {
        secrets::get(&host.id, SecretKind::Password).map_err(anyhow::Error::msg)?
    } else {
        None
    };
    let mut remember = false;
    let mut failures = 0;
    loop {
        if password.is_none() && !anonymous {
            let reply = prompter
                .ask(|request_id| SshEvent::Prompt {
                    request_id,
                    kind: PromptKind::Password,
                    name: String::new(),
                    instructions: String::new(),
                    prompts: vec![PromptField {
                        prompt: "Password:".into(),
                        echo: false,
                    }],
                    can_remember: true,
                    retry: failures > 0,
                })
                .await;
            match reply {
                PromptReply::Answers {
                    mut answers,
                    remember: keep,
                } if !answers.is_empty() => {
                    password = Some(answers.swap_remove(0));
                    remember = keep;
                }
                _ => return Err(Cancelled.into()),
            }
        }
        params.password = password.clone().unwrap_or_else(|| "anonymous@".into());

        match connect(&params).await {
            Ok(mut ftp) => {
                if remember
                    && let Err(e) = (connector.save_secret)(SecretKind::Password, &params.password)
                {
                    prompter.notify(SshEvent::Banner {
                        message: format!("Could not save the password: {e}"),
                    });
                }
                let mlsd = ftp
                    .feat()
                    .await
                    .is_ok_and(|features| features.contains_key("MLSD"));
                return Ok(FtpSession {
                    control: tokio::sync::Mutex::new(ftp),
                    last_used: Mutex::new(Instant::now()),
                    params,
                    mlsd,
                    transfer_slots: tokio::sync::Semaphore::new(MAX_PARALLEL_TRANSFERS),
                });
            }
            Err(error) if error.is::<UntrustedCertificate>() => {
                let rejected = error
                    .downcast_ref::<UntrustedCertificate>()
                    .expect("checked above")
                    .0
                    .clone();
                let reply = prompter
                    .ask(|request_id| SshEvent::Certificate {
                        request_id,
                        fingerprint: rejected.fingerprint.clone(),
                        reason: rejected.reason.clone(),
                        changed: params.trusted.is_some(),
                    })
                    .await;
                if !matches!(reply, PromptReply::HostKey { accept: true }) {
                    return Err(Cancelled.into());
                }
                trust.set(&params.key(), &rejected.fingerprint)?;
                params.trusted = Some(rejected.fingerprint);
            }
            Err(error) if is_login_failure(&error) && !anonymous => {
                failures += 1;
                if failures >= MAX_ATTEMPTS {
                    bail!("login incorrect");
                }
                password = None;
            }
            Err(error) => return Err(error),
        }
    }
}

#[tauri::command]
pub fn ftp_open(
    app: AppHandle,
    state: State<'_, FtpState>,
    registry: State<'_, Arc<PromptRegistry>>,
    host_id: String,
    on_event: Channel<SshEvent>,
) -> Result<u32, String> {
    let connector = Connector::for_host(&app, &host_id, &on_event, registry.inner())?;
    let trust = TrustStore::open(&app)?;
    let id = state.next_id.fetch_add(1, Ordering::Relaxed) + 1;
    let mut sessions = state.sessions.lock().map_err(|e| e.to_string())?;

    let task_app = app.clone();
    let task = tauri::async_runtime::spawn(async move {
        let result = open(&trust, &connector).await;
        let state = task_app.state::<FtpState>();
        let event = match result {
            Ok(session) => {
                match state.sessions.lock() {
                    // Still wanted: the tab may have been closed while connecting.
                    Ok(mut sessions) if sessions.contains_key(&id) => {
                        sessions.insert(id, Slot::Ready(Arc::new(session)));
                    }
                    _ => return,
                }
                SshEvent::Status {
                    stage: Stage::Connected,
                }
            }
            Err(e) => {
                if let Ok(mut sessions) = state.sessions.lock() {
                    sessions.remove(&id);
                }
                let error = (!e.is::<Cancelled>()).then(|| format!("{e:#}"));
                SshEvent::Closed {
                    exit_code: None,
                    error,
                }
            }
        };
        let _ = on_event.send(event);
    });
    sessions.insert(id, Slot::Connecting(task));
    Ok(id)
}

#[tauri::command]
pub fn ftp_close(state: State<'_, FtpState>, id: u32) -> Result<(), String> {
    if let Some(slot) = state
        .sessions
        .lock()
        .map_err(|e| e.to_string())?
        .remove(&id)
    {
        match slot {
            Slot::Connecting(task) => task.abort(),
            // Say goodbye politely, in the background.
            Slot::Ready(session) => {
                tauri::async_runtime::spawn(async move {
                    let _ = session.control.lock().await.quit().await;
                });
            }
        }
    }
    Ok(())
}

// ── Browsing ────────────────────────────────────────────────

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn mode_bits(file: &ListFile) -> Option<u32> {
    let mut mode = 0;
    for (who, shift) in [
        (PosixPexQuery::Owner, 6),
        (PosixPexQuery::Group, 3),
        (PosixPexQuery::Others, 0),
    ] {
        mode |= (u32::from(file.can_read(who)) * 4
            + u32::from(file.can_write(who)) * 2
            + u32::from(file.can_execute(who)))
            << shift;
    }
    // MLSD and DOS listings often carry no permissions at all.
    (mode != 0).then_some(mode)
}

async fn list_dir(
    ftp: &mut AsyncRustlsFtpStream,
    path: &str,
    mlsd: bool,
) -> Result<Vec<SftpEntry>> {
    let lines = if mlsd {
        ftp.mlsd(Some(path)).await?
    } else {
        ftp.list(Some(path)).await?
    };
    let mut entries = Vec::new();
    for line in lines {
        let parsed = if mlsd {
            ListParser::parse_mlsd(&line)
        } else {
            ListParser::parse_posix(&line).or_else(|_| ListParser::parse_dos(&line))
        };
        let Ok(file) = parsed else { continue };
        let name = file.name().to_string();
        if name == "." || name == ".." || name.is_empty() {
            continue;
        }
        let full = join(path, &name);
        // A listing does not say where a symlink points; try entering it.
        let is_dir = if file.is_symlink() {
            let enters = ftp.cwd(&full).await.is_ok();
            if enters {
                let _ = ftp.cwd(path).await;
            }
            enters
        } else {
            file.is_directory()
        };
        entries.push(SftpEntry {
            name,
            path: full,
            is_dir,
            is_symlink: file.is_symlink(),
            size: file.size() as u64,
            modified: file
                .modified()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as u32),
            permissions: mode_bits(&file),
            owner: file.uid().map(|uid| uid.to_string()),
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[tauri::command]
pub async fn ftp_realpath(
    state: State<'_, FtpState>,
    id: u32,
    path: String,
) -> Result<String, String> {
    let session = state.get(id)?;
    let mut ftp = session.control().await.map_err(err)?;
    if path == "." {
        return ftp.pwd().await.map_err(|e| e.to_string());
    }
    Ok(path)
}

#[tauri::command]
pub async fn ftp_list(
    state: State<'_, FtpState>,
    id: u32,
    path: String,
) -> Result<Vec<SftpEntry>, String> {
    let session = state.get(id)?;
    let mut ftp = session.control().await.map_err(err)?;
    list_dir(&mut ftp, &path, session.mlsd).await.map_err(err)
}

#[tauri::command]
pub async fn ftp_mkdir(state: State<'_, FtpState>, id: u32, path: String) -> Result<(), String> {
    let session = state.get(id)?;
    let mut ftp = session.control().await.map_err(err)?;
    ftp.mkdir(path).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ftp_rename(
    state: State<'_, FtpState>,
    id: u32,
    from: String,
    to: String,
) -> Result<(), String> {
    let session = state.get(id)?;
    let mut ftp = session.control().await.map_err(err)?;
    ftp.rename(from, to).await.map_err(|e| e.to_string())
}

/// Deletes a file, or a directory with everything inside.
#[tauri::command]
pub async fn ftp_delete(
    state: State<'_, FtpState>,
    id: u32,
    path: String,
    is_dir: bool,
) -> Result<(), String> {
    let session = state.get(id)?;
    let mut ftp = session.control().await.map_err(err)?;
    remove(&mut ftp, &path, is_dir, session.mlsd)
        .await
        .map_err(err)
}

async fn remove(
    ftp: &mut AsyncRustlsFtpStream,
    path: &str,
    is_dir: bool,
    mlsd: bool,
) -> Result<()> {
    if !is_dir {
        return ftp
            .rm(path)
            .await
            .with_context(|| format!("could not delete {path}"));
    }
    // Depth-first: a directory is removed after its contents.
    let mut stack = vec![(path.to_string(), false)];
    while let Some((dir, emptied)) = stack.pop() {
        if emptied {
            ftp.rmdir(&dir)
                .await
                .with_context(|| format!("could not delete {dir}"))?;
            continue;
        }
        stack.push((dir.clone(), true));
        for entry in list_dir(ftp, &dir, mlsd).await? {
            if entry.is_dir && !entry.is_symlink {
                stack.push((entry.path, false));
            } else {
                ftp.rm(&entry.path)
                    .await
                    .with_context(|| format!("could not delete {}", entry.path))?;
            }
        }
    }
    Ok(())
}

// ── Transfers ───────────────────────────────────────────────

struct Job {
    remote: String,
    local: PathBuf,
    size: u64,
}

fn register_transfer(state: &FtpState, transfer_id: u32) -> Result<Arc<AtomicBool>, String> {
    let flag = Arc::new(AtomicBool::new(false));
    state
        .transfers
        .lock()
        .map_err(|e| e.to_string())?
        .insert(transfer_id, flag.clone());
    Ok(flag)
}

fn finish_transfer(state: &FtpState, transfer_id: u32) {
    if let Ok(mut transfers) = state.transfers.lock() {
        transfers.remove(&transfer_id);
    }
}

/// Downloads a remote file or directory into the local directory `local_dir`.
#[tauri::command]
pub async fn ftp_download(
    state: State<'_, FtpState>,
    id: u32,
    transfer_id: u32,
    remote: String,
    is_dir: bool,
    local_dir: String,
    on_progress: Channel<TransferProgress>,
) -> Result<(), String> {
    let session = state.get(id)?;
    let cancelled = register_transfer(&state, transfer_id)?;
    let result = download(
        &session,
        &remote,
        is_dir,
        Path::new(&local_dir),
        &on_progress,
        &cancelled,
    )
    .await;
    finish_transfer(&state, transfer_id);
    result.map_err(err)
}

async fn download(
    session: &FtpSession,
    remote: &str,
    is_dir: bool,
    local_dir: &Path,
    channel: &Channel<TransferProgress>,
    cancelled: &AtomicBool,
) -> Result<()> {
    let _slot = session.transfer_slots.acquire().await?;
    let mut ftp = connect(&session.params).await?;
    let name = remote
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or("download");
    let root = local_dir.join(name);

    let mut jobs = Vec::new();
    let mut dirs = Vec::new();
    if is_dir {
        let mut stack = vec![(remote.to_string(), root)];
        while let Some((remote_dir, local)) = stack.pop() {
            dirs.push(local.clone());
            for entry in list_dir(&mut ftp, &remote_dir, session.mlsd).await? {
                let local_child = local.join(&entry.name);
                if entry.is_dir && !entry.is_symlink {
                    stack.push((entry.path, local_child));
                } else if !entry.is_dir {
                    jobs.push(Job {
                        remote: entry.path,
                        local: local_child,
                        size: entry.size,
                    });
                }
            }
        }
    } else {
        let size = ftp.size(remote).await.unwrap_or(0) as u64;
        jobs.push(Job {
            remote: remote.to_string(),
            local: root,
            size,
        });
    }

    let mut progress = Progress::new(
        channel,
        cancelled,
        jobs.iter().map(|j| j.size).sum(),
        jobs.len(),
    );
    for dir in &dirs {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    let mut buffer = vec![0u8; CHUNK];
    for job in jobs {
        let result = async {
            let mut target = tokio::fs::File::create(&job.local)
                .await
                .with_context(|| format!("could not create {}", job.local.display()))?;
            let mut source = ftp
                .retr_as_stream(&job.remote)
                .await
                .with_context(|| format!("could not open {}", job.remote))?;
            loop {
                let n = source.read(&mut buffer).await?;
                if n == 0 {
                    break;
                }
                target.write_all(&buffer[..n]).await?;
                progress.advance(n as u64, &job.remote)?;
            }
            source.finish().await?;
            target.flush().await?;
            anyhow::Ok(())
        }
        .await;
        if result.is_err() {
            // Don't leave a truncated file behind.
            let _ = tokio::fs::remove_file(&job.local).await;
        }
        result?;
        progress.state.files_done += 1;
    }
    progress.flush();
    let _ = ftp.quit().await;
    Ok(())
}

/// Uploads local files or directories into the remote directory `remote_dir`.
#[tauri::command]
pub async fn ftp_upload(
    state: State<'_, FtpState>,
    id: u32,
    transfer_id: u32,
    locals: Vec<String>,
    remote_dir: String,
    on_progress: Channel<TransferProgress>,
) -> Result<(), String> {
    let session = state.get(id)?;
    let cancelled = register_transfer(&state, transfer_id)?;
    let result = upload(&session, &locals, &remote_dir, &on_progress, &cancelled).await;
    finish_transfer(&state, transfer_id);
    result.map_err(err)
}

async fn upload(
    session: &FtpSession,
    locals: &[String],
    remote_dir: &str,
    channel: &Channel<TransferProgress>,
    cancelled: &AtomicBool,
) -> Result<()> {
    let mut jobs = Vec::new();
    let mut dirs = Vec::new();
    for local in locals {
        let local = PathBuf::from(local);
        let name = local
            .file_name()
            .context("invalid path")?
            .to_string_lossy()
            .into_owned();
        let metadata = tokio::fs::metadata(&local)
            .await
            .with_context(|| format!("{} not found", local.display()))?;
        if metadata.is_dir() {
            let mut stack = vec![(local.clone(), join(remote_dir, &name))];
            while let Some((local_dir, remote)) = stack.pop() {
                dirs.push(remote.clone());
                let mut entries = tokio::fs::read_dir(&local_dir).await?;
                while let Some(entry) = entries.next_entry().await? {
                    let child = join(&remote, &entry.file_name().to_string_lossy());
                    let file_type = entry.file_type().await?;
                    if file_type.is_dir() {
                        stack.push((entry.path(), child));
                    } else if file_type.is_file() {
                        jobs.push(Job {
                            remote: child,
                            local: entry.path(),
                            size: entry.metadata().await?.len(),
                        });
                    }
                }
            }
        } else {
            jobs.push(Job {
                remote: join(remote_dir, &name),
                local,
                size: metadata.len(),
            });
        }
    }

    let _slot = session.transfer_slots.acquire().await?;
    let mut ftp = connect(&session.params).await?;
    let mut progress = Progress::new(
        channel,
        cancelled,
        jobs.iter().map(|j| j.size).sum(),
        jobs.len(),
    );
    for dir in &dirs {
        // The folder may already exist; a real failure shows up when writing into it.
        let _ = ftp.mkdir(dir).await;
    }
    let mut buffer = vec![0u8; CHUNK];
    for job in jobs {
        let mut source = tokio::fs::File::open(&job.local)
            .await
            .with_context(|| format!("could not open {}", job.local.display()))?;
        let mut target = ftp
            .put_with_stream(&job.remote)
            .await
            .with_context(|| format!("could not create {}", job.remote))?;
        loop {
            let n = source.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            target.write_all(&buffer[..n]).await?;
            progress.advance(n as u64, &job.remote)?;
        }
        target.finish().await?;
        progress.state.files_done += 1;
    }
    progress.flush();
    let _ = ftp.quit().await;
    Ok(())
}

#[tauri::command]
pub fn ftp_cancel_transfer(state: State<'_, FtpState>, transfer_id: u32) -> Result<(), String> {
    if let Some(flag) = state
        .transfers
        .lock()
        .map_err(|e| e.to_string())?
        .get(&transfer_id)
    {
        flag.store(true, Ordering::Relaxed);
    }
    Ok(())
}

/// End-to-end tests against a real FTP server. Ignored by default; run them with:
///
/// ```sh
/// docker run -d --name ht-ftpd -p 127.0.0.1:2121:21 -p 127.0.0.1:30000-30009:30000-30009 \
///   -e PUBLICHOST=127.0.0.1 -e FTP_USER_NAME=hostealo -e FTP_USER_PASS=secret \
///   -e FTP_USER_HOME=/home/hostealo -e ADDED_FLAGS=--tls=1 -e TLS_CN=ftp.example.test \
///   -e TLS_ORG=Test -e TLS_C=ES stilliard/pure-ftpd
/// HT_TEST_FTP_PORT=2121 HT_TEST_FTP_PASSWORD=secret cargo test -- --ignored ftp::tests
/// ```
#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::{AuthMethod, Host};
    use crate::ssh::Prompter;
    use serde_json::Value;
    use tauri::ipc::InvokeResponseBody;

    /// The test server allows few logins at a time, so these tests take turns.
    static SERVER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn env(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("set {name}"))
    }

    /// A connector whose prompts are answered by `answer` (event JSON → reply); records events.
    fn connector(
        protocol: Protocol,
        answer: impl Fn(&Value) -> PromptReply + Send + Sync + 'static,
    ) -> (Connector, Arc<Mutex<Vec<Value>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let registry = Arc::new(PromptRegistry::default());
        let channel = {
            let (events, registry) = (events.clone(), registry.clone());
            Channel::new(move |body| {
                let InvokeResponseBody::Json(json) = body else {
                    return Ok(());
                };
                let event: Value = serde_json::from_str(&json).unwrap();
                if let Some(id) = event["requestId"].as_u64() {
                    registry.reply(id, answer(&event)).unwrap();
                }
                events.lock().unwrap().push(event);
                Ok(())
            })
        };
        let host = Host {
            id: "ftp-test".into(),
            name: "ftp".into(),
            hostname: "127.0.0.1".into(),
            port: env("HT_TEST_FTP_PORT").parse().unwrap(),
            username: "hostealo".into(),
            protocol,
            auth: AuthMethod::Password,
            color: None,
            folder: None,
            rdp_shared_folder: None,
            rdp_commands_host: None,
            has_password: false,
            has_passphrase: false,
            os: None,
            hostealo: None,
            updated_at: 0,
        };
        let connector = Connector {
            host,
            known_hosts: PathBuf::new(),
            prompter: Prompter {
                events: channel,
                registry,
            },
            save_secret: Arc::new(|_, _| Ok(())),
        };
        (connector, events)
    }

    fn password_then_trust(password: String) -> impl Fn(&Value) -> PromptReply + Send + Sync {
        move |event| match event["type"].as_str() {
            Some("certificate") => PromptReply::HostKey { accept: true },
            _ => PromptReply::Answers {
                answers: vec![password.clone()],
                remember: false,
            },
        }
    }

    fn types(events: &Mutex<Vec<Value>>) -> Vec<String> {
        events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| e["type"].as_str().map(String::from))
            .collect()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ht-ftp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    #[ignore = "needs a test FTP server, see module docs"]
    async fn ftps_self_signed_certificate_is_asked_once_then_trusted() {
        let _turn = SERVER.lock().await;
        let dir = temp("tls");
        let trust = dir.join("certs.json");
        let (conn, events) = connector(
            Protocol::Ftps,
            password_then_trust(env("HT_TEST_FTP_PASSWORD")),
        );
        let session = open(&TrustStore::at(trust.clone()), &conn)
            .await
            .expect("ftps login");
        let certificate = events
            .lock()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "certificate")
            .cloned()
            .expect("certificate prompt");
        assert_eq!(certificate["changed"], false);
        assert_eq!(
            certificate["fingerprint"].as_str().unwrap().len(),
            32 * 3 - 1
        );
        assert!(session.params.trusted.is_some());

        // Second time: the remembered certificate is accepted silently.
        let (conn, events) = connector(
            Protocol::Ftps,
            password_then_trust(env("HT_TEST_FTP_PASSWORD")),
        );
        open(&TrustStore::at(trust.clone()), &conn)
            .await
            .expect("second login");
        assert!(
            !types(&events).contains(&"certificate".to_string()),
            "{:?}",
            types(&events)
        );

        // Rejecting a certificate cancels quietly.
        let (conn, _) = connector(Protocol::Ftps, |event| match event["type"].as_str() {
            Some("certificate") => PromptReply::HostKey { accept: false },
            _ => PromptReply::Answers {
                answers: vec!["secret".into()],
                remember: false,
            },
        });
        let error = open(&TrustStore::at(dir.join("empty.json")), &conn)
            .await
            .err()
            .expect("should be cancelled");
        assert!(error.is::<Cancelled>(), "{error:#}");
    }

    #[tokio::test]
    #[ignore = "needs a test FTP server, see module docs"]
    async fn wrong_password_is_asked_again() {
        let _turn = SERVER.lock().await;
        let password = env("HT_TEST_FTP_PASSWORD");
        let attempts = Arc::new(Mutex::new(vec!["wrong".to_string(), password]));
        let (conn, events) = connector(Protocol::Ftp, move |_| PromptReply::Answers {
            answers: vec![attempts.lock().unwrap().remove(0)],
            remember: false,
        });
        open(&TrustStore::at(temp("pw").join("certs.json")), &conn)
            .await
            .expect("login after retry");
        let prompts: Vec<Value> = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "prompt")
            .cloned()
            .collect();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[1]["retry"], true);
    }

    #[tokio::test]
    #[ignore = "needs a test FTP server, see module docs"]
    async fn ftps_round_trip_of_a_directory_tree() {
        let _turn = SERVER.lock().await;
        let dir = temp("tree");
        let (conn, _) = connector(
            Protocol::Ftps,
            password_then_trust(env("HT_TEST_FTP_PASSWORD")),
        );
        let session = open(&TrustStore::at(dir.join("certs.json")), &conn)
            .await
            .expect("login");
        let home = session.control().await.unwrap().pwd().await.unwrap();

        let tree = dir.join("site");
        std::fs::create_dir_all(tree.join("css/vendor")).unwrap();
        std::fs::write(tree.join("index.html"), "<h1>hola</h1>").unwrap();
        let big: Vec<u8> = (0..700_000).map(|i| (i % 253) as u8).collect();
        std::fs::write(tree.join("css/vendor/big.bin"), &big).unwrap();

        let updates = Arc::new(Mutex::new(Vec::<Value>::new()));
        let progress = {
            let updates = updates.clone();
            Channel::new(move |body| {
                if let InvokeResponseBody::Json(json) = body {
                    updates
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str(&json).unwrap());
                }
                Ok(())
            })
        };
        let go = AtomicBool::new(false);

        let remote_dir = join(&home, "ht-upload");
        {
            let mut ftp = session.control().await.unwrap();
            let _ = remove(&mut ftp, &remote_dir, true, session.mlsd).await;
            ftp.mkdir(&remote_dir).await.unwrap();
        }
        upload(
            &session,
            &[tree.to_string_lossy().into_owned()],
            &remote_dir,
            &progress,
            &go,
        )
        .await
        .expect("upload");
        let last = updates.lock().unwrap().last().cloned().unwrap();
        assert_eq!(
            (last["filesDone"].as_u64(), last["filesTotal"].as_u64()),
            (Some(2), Some(2))
        );

        let listing = {
            let mut ftp = session.control().await.unwrap();
            list_dir(&mut ftp, &join(&remote_dir, "site"), session.mlsd)
                .await
                .unwrap()
        };
        let names: Vec<(&str, bool)> = listing
            .iter()
            .map(|e| (e.name.as_str(), e.is_dir))
            .collect();
        assert_eq!(names, [("css", true), ("index.html", false)]);

        let back = dir.join("back");
        std::fs::create_dir_all(&back).unwrap();
        download(
            &session,
            &join(&remote_dir, "site"),
            true,
            &back,
            &progress,
            &go,
        )
        .await
        .expect("download");
        assert_eq!(
            std::fs::read_to_string(back.join("site/index.html")).unwrap(),
            "<h1>hola</h1>"
        );
        assert_eq!(
            std::fs::read(back.join("site/css/vendor/big.bin")).unwrap(),
            big
        );

        let mut ftp = session.control().await.unwrap();
        ftp.rename(join(&remote_dir, "site"), join(&remote_dir, "renamed"))
            .await
            .unwrap();
        remove(&mut ftp, &remote_dir, true, session.mlsd)
            .await
            .expect("recursive delete");
        let parent = list_dir(&mut ftp, &home, session.mlsd).await.unwrap();
        assert!(!parent.iter().any(|e| e.name == "ht-upload"));
    }

    #[tokio::test]
    #[ignore = "needs a test FTP server, see module docs"]
    async fn many_parallel_downloads_stay_within_the_server_login_limit() {
        let _turn = SERVER.lock().await;
        let dir = temp("parallel");
        let (conn, _) = connector(
            Protocol::Ftp,
            password_then_trust(env("HT_TEST_FTP_PASSWORD")),
        );
        let session = open(&TrustStore::at(dir.join("certs.json")), &conn)
            .await
            .expect("login");
        let home = session.control().await.unwrap().pwd().await.unwrap();
        let source = dir.join("file.txt");
        std::fs::write(&source, "x".repeat(50_000)).unwrap();
        let progress: Channel<TransferProgress> = Channel::new(|_| Ok(()));
        let go = AtomicBool::new(false);
        upload(
            &session,
            &[source.to_string_lossy().into_owned()],
            &home,
            &progress,
            &go,
        )
        .await
        .unwrap();

        // The server allows 5 logins; 6 transfers at once would exceed it without the slot limit.
        let remote = join(&home, "file.txt");
        let target = |n: usize| {
            let path = dir.join(format!("out{n}"));
            std::fs::create_dir_all(&path).unwrap();
            path
        };
        let paths: Vec<PathBuf> = (0..6).map(target).collect();
        let d = |i: usize| download(&session, &remote, false, &paths[i], &progress, &go);
        let results = tokio::join!(d(0), d(1), d(2), d(3), d(4), d(5));
        for result in [
            results.0, results.1, results.2, results.3, results.4, results.5,
        ] {
            result.expect("parallel download");
        }
        assert!(
            paths
                .iter()
                .all(|p| std::fs::metadata(p.join("file.txt")).unwrap().len() == 50_000)
        );
        session.control().await.unwrap().rm(&remote).await.unwrap();
    }
}
