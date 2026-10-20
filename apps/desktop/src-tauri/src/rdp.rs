//! Remote desktop (RDP) sessions, with IronRDP.
//!
//! `rdp_connect` returns a session id and connects in the background: TCP, protocol
//! negotiation, TLS (verified like FTPS, see `tls`), then NLA/CredSSP and the RDP connection
//! sequence. The session then decodes graphics updates into a frame buffer and sends only the
//! changed rectangle to the UI as raw RGBA, which draws it on a canvas. Mouse and keyboard
//! input come back through `rdp_input`.
//!
//! Frame messages (`on_frame`, raw bytes, little endian):
//! - `0, x: u16, y: u16, width: u16, height: u16, rgba...`: a rectangle of the desktop.
//! - `1, width: u16, height: u16, hotspot_x: u16, hotspot_y: u16, rgba...`: the mouse pointer.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use ironrdp::cliprdr::pdu::ClipboardPdu;
use ironrdp::cliprdr::{CliprdrClient, CliprdrSvcMessages};
use ironrdp::connector::connection_activation::ConnectionActivationState;
use ironrdp::connector::{
    self as rdp_connector, ClientConnector, ConnectionResult, Credentials, DesktopSize,
};
use ironrdp::displaycontrol::client::DisplayControlClient;
use ironrdp::displaycontrol::pdu::MonitorLayoutEntry;
use ironrdp::dvc::DrdynvcClient;
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::graphics::pointer::DecodedPointer;
use ironrdp::input::{Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp::pdu::geometry::InclusiveRectangle;
use ironrdp::pdu::rdp::capability_sets::{MajorPlatformType, client_codecs_capabilities};
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::rdpdr::Rdpdr;
use ironrdp::rdpsnd::client::{Rdpsnd, RdpsndClientHandler};
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageOutput, fast_path};
use ironrdp::svc::{ChannelFlags, SvcMessage};
use ironrdp_tokio::{FramedWrite, TokioFramed, single_sequence_step_read, split_tokio_framed};
use serde::{Deserialize, Serialize};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Manager, State};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use x509_cert::der::Decode;

use crate::rdp_clipboard::{
    Backend as ClipboardBackend, ClipboardEvent, ClipboardLink, LocalClipboard, Notice,
};
use crate::rdp_drive::{DRIVE_ID, DRIVE_NAME, DriveBackend};
use crate::secrets::{self, SecretKind};
use crate::ssh::{
    Cancelled, Connector, PromptField, PromptKind, PromptRegistry, PromptReply, SshEvent, Stage,
};
use crate::tls::{self, TrustStore, UntrustedCertificate};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// RDP limits the client name to 15 characters.
const CLIENT_NAME: &str = "HOSTEALO";

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum InputOp {
    MouseMove {
        x: u16,
        y: u16,
    },
    /// `button` uses the DOM numbering: 0 left, 1 middle, 2 right, 3 back, 4 forward.
    MouseButton {
        button: u8,
        pressed: bool,
    },
    Wheel {
        vertical: bool,
        units: i16,
    },
    /// PC/AT set 1 scancode; extended keys carry the 0xE000 prefix.
    Key {
        scancode: u16,
        pressed: bool,
    },
    Unicode {
        text: String,
        pressed: bool,
    },
    /// Releases every key and button (the window lost focus).
    ReleaseAll,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DisplayEvent {
    Size {
        width: u16,
        height: u16,
    },
    PointerDefault,
    PointerHidden,
    /// Files copied on the remote, and the progress of saving them.
    Clipboard {
        notice: Notice,
    },
    /// Remote sound started playing here, or could not (`error`).
    Audio {
        playing: bool,
        error: Option<String>,
    },
}

enum Command {
    Input(Vec<InputOp>),
    /// Answers whether the server resizes live (display control channel); without it the UI
    /// reconnects at the new size.
    Resize {
        width: u32,
        height: u32,
        reply: oneshot::Sender<bool>,
    },
    /// Re-announce the local clipboard (the session tab got the focus).
    ClipboardSync,
    /// Save the files copied on the remote into this folder.
    ClipboardSave(PathBuf),
    ClipboardCancel,
    Close,
}

struct SessionHandle {
    commands: mpsc::UnboundedSender<Command>,
    cancel: watch::Sender<bool>,
}

#[derive(Default)]
pub struct RdpState {
    sessions: Mutex<HashMap<u32, SessionHandle>>,
    next_id: AtomicU32,
}

impl RdpState {
    fn send(&self, id: u32, command: Command) -> Result<(), String> {
        let sessions = self.sessions.lock().map_err(|e| e.to_string())?;
        let session = sessions.get(&id).ok_or("session not found")?;
        session
            .commands
            .send(command)
            .map_err(|_| "session closed".to_string())
    }
}

/// `DOMAIN\user` or `user@domain` → (user, domain). A UPN is passed through as is.
fn split_user(username: &str) -> (String, Option<String>) {
    match username.split_once('\\') {
        Some((domain, user)) if !domain.is_empty() => (user.to_string(), Some(domain.to_string())),
        _ => (username.to_string(), None),
    }
}

/// The host's shared folder, with `~` expanded, if it is set and exists.
fn shared_folder(setting: Option<&str>) -> Option<PathBuf> {
    let setting = setting.map(str::trim).filter(|s| !s.is_empty())?;
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    let path = match (
        setting
            .strip_prefix("~/")
            .or_else(|| setting.strip_prefix("~\\")),
        home,
    ) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ if setting == "~" => PathBuf::from(std::env::var_os("HOME")?),
        _ => PathBuf::from(setting),
    };
    path.is_dir().then_some(path)
}

fn connector_config(
    username: &str,
    password: &str,
    width: u16,
    height: u16,
) -> Result<rdp_connector::Config> {
    let (username, domain) = split_user(username);
    let codecs = client_codecs_capabilities(&[]).map_err(anyhow::Error::msg)?;
    Ok(rdp_connector::Config {
        desktop_size: DesktopSize { width, height },
        desktop_scale_factor: 0,
        enable_tls: true,
        enable_credssp: true,
        credentials: Credentials::UsernamePassword {
            username,
            password: password.to_string(),
        },
        domain,
        client_build: 0,
        client_name: CLIENT_NAME.to_string(),
        keyboard_type: ironrdp::pdu::gcc::KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: 0,
        ime_file_name: String::new(),
        bitmap: Some(rdp_connector::BitmapConfig {
            lossy_compression: true,
            color_depth: 32,
            codecs,
        }),
        dig_product_id: String::new(),
        client_dir: String::new(),
        platform: MajorPlatformType::UNSPECIFIED,
        hardware_id: None,
        request_data: None,
        // Log in with the given credentials instead of showing the server's login screen.
        autologon: true,
        // Remote audio plays on this computer's speakers (desktop only for now).
        enable_audio_playback: cfg!(desktop),
        performance_flags: PerformanceFlags::default(),
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        enable_server_pointer: true,
        pointer_software_rendering: false,
    })
}

trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite> AsyncReadWrite for T {}
type UpgradedFramed = TokioFramed<Box<dyn AsyncReadWrite + Unpin + Send + Sync>>;

struct Target {
    host: String,
    port: u16,
    username: String,
    password: String,
    width: u16,
    height: u16,
    /// Local folder shared as a drive, if any.
    shared_folder: Option<PathBuf>,
}

/// Connection sequence up to an active session.
async fn connect(
    target: &Target,
    trusted: Option<String>,
    clipboard: &ClipboardBackend,
    audio: Option<Box<dyn RdpsndClientHandler>>,
) -> Result<(ConnectionResult, UpgradedFramed)> {
    let address = (target.host.as_str(), target.port);
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address))
        .await
        .with_context(|| format!("timed out connecting to {}:{}", target.host, target.port))?
        .with_context(|| format!("could not connect to {}:{}", target.host, target.port))?;
    stream.set_nodelay(true)?;
    let client_addr: SocketAddr = stream.local_addr()?;
    let mut framed = TokioFramed::new(stream);

    let config = connector_config(
        &target.username,
        &target.password,
        target.width,
        target.height,
    )?;
    let drdynvc =
        DrdynvcClient::new().with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())));
    let connector = ClientConnector::new(config, client_addr)
        .with_static_channel(drdynvc)
        .with_static_channel(CliprdrClient::new(Box::new(clipboard.clone())));
    let connector = match audio {
        Some(handler) => connector.with_static_channel(Rdpsnd::new(handler)),
        None => connector,
    };
    let mut connector = match &target.shared_folder {
        Some(folder) => {
            let computer = crate::device::host_name().unwrap_or_else(|| CLIENT_NAME.to_string());
            connector.with_static_channel(
                Rdpdr::new(Box::new(DriveBackend::new(folder.clone())), computer)
                    .with_drives(Some(vec![(DRIVE_ID, DRIVE_NAME.to_string())])),
            )
        }
        None => connector,
    };

    let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector).await?;
    let (stream, leftover) = framed.into_inner();

    let (tls_config, verifier) = tls::client_config(trusted)?;
    let server_name = rustls::pki_types::ServerName::try_from(target.host.clone())?;
    let tls_stream = match tokio_rustls::TlsConnector::from(tls_config)
        .connect(server_name, stream)
        .await
    {
        Ok(tls_stream) => tls_stream,
        Err(error) => {
            if let Some(rejected) = verifier.take_rejected() {
                return Err(UntrustedCertificate(rejected).into());
            }
            return Err(error).context("TLS handshake failed");
        }
    };
    // CredSSP binds the login to the server's TLS public key.
    let certificate = tls_stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|chain| chain.first())
        .context("the server sent no certificate")?;
    let certificate = x509_cert::Certificate::from_der(certificate)?;
    let server_public_key = certificate
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .context("unsupported server certificate")?
        .to_vec();

    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
    let erased: Box<dyn AsyncReadWrite + Unpin + Send + Sync> = Box::new(tls_stream);
    let mut framed = TokioFramed::new_with_leftover(erased, leftover);
    let result = ironrdp_tokio::connect_finalize(
        upgraded,
        &mut framed,
        connector,
        rdp_connector::ServerName::new(target.host.clone()),
        server_public_key,
        None,
        None,
    )
    .await?;
    Ok((result, framed))
}

/// Asks for the password and about untrusted certificates as needed, then connects.
async fn open(
    trust: &TrustStore,
    connector: &Connector,
    clipboard: &ClipboardBackend,
    audio: &dyn Fn() -> Option<Box<dyn RdpsndClientHandler>>,
    width: u16,
    height: u16,
) -> Result<(ConnectionResult, UpgradedFramed)> {
    let host = &connector.host;
    let prompter = &connector.prompter;
    prompter.notify(SshEvent::Status {
        stage: Stage::Connecting,
    });

    let mut remember = false;
    let password = match host
        .has_password
        .then(|| secrets::get(&host.id, SecretKind::Password))
        .transpose()
    {
        Ok(Some(Some(password))) => password,
        Ok(_) => {
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
                    retry: false,
                })
                .await;
            match reply {
                PromptReply::Answers {
                    mut answers,
                    remember: keep,
                } if !answers.is_empty() => {
                    remember = keep;
                    answers.swap_remove(0)
                }
                _ => return Err(Cancelled.into()),
            }
        }
        Err(e) => return Err(anyhow::Error::msg(e)),
    };

    let target = Target {
        host: host.hostname.clone(),
        port: host.port,
        username: host.username.clone(),
        password,
        width,
        height,
        shared_folder: shared_folder(host.rdp_shared_folder.as_deref()),
    };
    let key = format!("rdp:{}:{}", target.host, target.port);
    let mut trusted = trust.get(&key);
    loop {
        match connect(&target, trusted.clone(), clipboard, audio()).await {
            Ok(connected) => {
                if remember
                    && let Err(e) = (connector.save_secret)(SecretKind::Password, &target.password)
                {
                    prompter.notify(SshEvent::Banner {
                        message: format!("Could not save the password: {e}"),
                    });
                }
                return Ok(connected);
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
                        changed: trusted.is_some(),
                    })
                    .await;
                if !matches!(reply, PromptReply::HostKey { accept: true }) {
                    return Err(Cancelled.into());
                }
                trust.set(&key, &rejected.fingerprint)?;
                trusted = Some(rejected.fingerprint);
            }
            Err(error) => return Err(error),
        }
    }
}

fn to_operation(op: InputOp) -> Option<Operation> {
    Some(match op {
        InputOp::MouseMove { x, y } => Operation::MouseMove(MousePosition { x, y }),
        InputOp::MouseButton { button, pressed } => {
            let button = MouseButton::from_web_button(button)?;
            if pressed {
                Operation::MouseButtonPressed(button)
            } else {
                Operation::MouseButtonReleased(button)
            }
        }
        InputOp::Wheel { vertical, units } => Operation::WheelRotations(WheelRotations {
            is_vertical: vertical,
            rotation_units: units,
        }),
        InputOp::Key { scancode, pressed } => {
            let scancode = Scancode::from_u16(scancode);
            if pressed {
                Operation::KeyPressed(scancode)
            } else {
                Operation::KeyReleased(scancode)
            }
        }
        InputOp::Unicode { text, pressed } => {
            let character = text.chars().next()?;
            if pressed {
                Operation::UnicodeKeyPressed(character)
            } else {
                Operation::UnicodeKeyReleased(character)
            }
        }
        InputOp::ReleaseAll => return None,
    })
}

fn union(a: Option<InclusiveRectangle>, b: InclusiveRectangle) -> InclusiveRectangle {
    match a {
        None => b,
        Some(a) => InclusiveRectangle {
            left: a.left.min(b.left),
            top: a.top.min(b.top),
            right: a.right.max(b.right),
            bottom: a.bottom.max(b.bottom),
        },
    }
}

/// Copies a rectangle of the frame buffer into a frame message (opaque RGBA rows).
fn encode_rect(image: &DecodedImage, rect: &InclusiveRectangle) -> Vec<u8> {
    let right = rect.right.min(image.width().saturating_sub(1));
    let bottom = rect.bottom.min(image.height().saturating_sub(1));
    let (left, top) = (rect.left.min(right), rect.top.min(bottom));
    let (width, height) = (usize::from(right - left) + 1, usize::from(bottom - top) + 1);
    let mut message = Vec::with_capacity(9 + width * height * 4);
    message.push(0);
    for value in [left, top, width as u16, height as u16] {
        message.extend_from_slice(&value.to_le_bytes());
    }
    let (data, stride) = (image.data(), image.stride());
    for row in 0..height {
        let start = (usize::from(top) + row) * stride + usize::from(left) * 4;
        message.extend_from_slice(&data[start..start + width * 4]);
    }
    // The decoder leaves alpha unset; the canvas needs opaque pixels.
    for pixel in message[9..].as_chunks_mut::<4>().0 {
        pixel[3] = 255;
    }
    message
}

fn encode_pointer(pointer: &DecodedPointer) -> Vec<u8> {
    let mut message = Vec::with_capacity(9 + pointer.bitmap_data.len());
    message.push(1);
    for value in [
        pointer.width,
        pointer.height,
        pointer.hotspot_x,
        pointer.hotspot_y,
    ] {
        message.extend_from_slice(&value.to_le_bytes());
    }
    message.extend_from_slice(&pointer.bitmap_data);
    message
}

/// The active session: decode server updates, forward input. Returns the disconnect reason.
async fn active_session(
    framed: UpgradedFramed,
    connection: ConnectionResult,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    clipboard: (&ClipboardLink, &mut mpsc::UnboundedReceiver<ClipboardEvent>),
    on_display: &Channel<DisplayEvent>,
    on_frame: &Channel<InvokeResponseBody>,
) -> Result<String> {
    let (clipboard, clipboard_events) = clipboard;
    let (mut reader, mut writer) = split_tokio_framed(framed);
    let size = connection.desktop_size;
    let mut image = DecodedImage::new(PixelFormat::RgbA32, size.width, size.height);
    let _ = on_display.send(DisplayEvent::Size {
        width: size.width,
        height: size.height,
    });
    let mut active_stage = ActiveStage::new(connection);
    let mut input = Database::new();

    loop {
        let outputs = tokio::select! {
            frame = reader.read_pdu() => {
                let (action, payload) = frame.context("connection lost")?;
                active_stage.process(&mut image, action, &payload)?
            }
            command = commands.recv() => match command {
                Some(Command::Input(ops)) => {
                    let events = if ops.iter().any(|op| matches!(op, InputOp::ReleaseAll)) {
                        input.release_all()
                    } else {
                        input.apply(ops.into_iter().filter_map(to_operation))
                    };
                    active_stage.process_fastpath_input(&mut image, &events)?
                }
                Some(Command::Resize {
                    width,
                    height,
                    reply,
                }) => {
                    let (width, height) = MonitorLayoutEntry::adjust_display_size(width, height);
                    match active_stage.encode_resize(width, height, None, None) {
                        Some(frame) => {
                            let _ = reply.send(true);
                            vec![ActiveStageOutput::ResponseFrame(frame?)]
                        }
                        None => {
                            let _ = reply.send(false);
                            Vec::new()
                        }
                    }
                }
                Some(Command::ClipboardSync) => {
                    clipboard.resync();
                    Vec::new()
                }
                Some(Command::ClipboardSave(dir)) => {
                    clipboard.save_remote_files(dir);
                    Vec::new()
                }
                Some(Command::ClipboardCancel) => {
                    clipboard.cancel_download();
                    Vec::new()
                }
                Some(Command::Close) | None => active_stage.graceful_shutdown()?,
            },
            Some(event) = clipboard_events.recv() => {
                let messages = match (active_stage.get_svc_processor::<CliprdrClient>(), event) {
                    (_, ClipboardEvent::Notice(notice)) => {
                        let _ = on_display.send(DisplayEvent::Clipboard { notice });
                        continue;
                    }
                    (None, _) => continue,
                    (Some(cliprdr), ClipboardEvent::Announce(formats)) => cliprdr.initiate_copy(&formats)?,
                    (Some(cliprdr), ClipboardEvent::Data(data)) => cliprdr.submit_format_data(data)?,
                    (Some(cliprdr), ClipboardEvent::Fetch(format)) => cliprdr.initiate_paste(format)?,
                    (Some(cliprdr), ClipboardEvent::FileContents(response)) => {
                        cliprdr.submit_file_contents(response)?
                    }
                    // IronRDP has no helper to ask for file contents; the PDU is sent as its own
                    // helpers send theirs (clipboard PDUs must carry SHOW_PROTOCOL).
                    (Some(_), ClipboardEvent::RequestFileContents(request)) => {
                        CliprdrSvcMessages::new(vec![
                            SvcMessage::from(ClipboardPdu::FileContentsRequest(request))
                                .with_flags(ChannelFlags::SHOW_PROTOCOL),
                        ])
                    }
                };
                vec![ActiveStageOutput::ResponseFrame(active_stage.process_svc_processor_messages(messages)?)]
            }
        };

        let mut dirty = None;
        for output in outputs {
            match output {
                ActiveStageOutput::ResponseFrame(frame) => writer.write_all(&frame).await?,
                ActiveStageOutput::GraphicsUpdate(rect) => dirty = Some(union(dirty, rect)),
                ActiveStageOutput::PointerDefault => {
                    let _ = on_display.send(DisplayEvent::PointerDefault);
                }
                ActiveStageOutput::PointerHidden => {
                    let _ = on_display.send(DisplayEvent::PointerHidden);
                }
                ActiveStageOutput::PointerPosition { .. } => {}
                ActiveStageOutput::PointerBitmap(pointer) => {
                    let _ = on_frame.send(InvokeResponseBody::Raw(encode_pointer(&pointer)));
                }
                ActiveStageOutput::DeactivateAll(mut sequence) => {
                    // Deactivation-Reactivation Sequence (e.g. after a resolution change).
                    let mut buf = ironrdp::core::WriteBuf::new();
                    loop {
                        let written =
                            single_sequence_step_read(&mut reader, &mut *sequence, &mut buf)
                                .await?;
                        if written.size().is_some() {
                            writer.write_all(buf.filled()).await?;
                        }
                        if let ConnectionActivationState::Finalized {
                            io_channel_id,
                            user_channel_id,
                            desktop_size,
                            enable_server_pointer,
                            pointer_software_rendering,
                        } = sequence.state
                        {
                            image = DecodedImage::new(
                                PixelFormat::RgbA32,
                                desktop_size.width,
                                desktop_size.height,
                            );
                            active_stage.set_fastpath_processor(
                                fast_path::ProcessorBuilder {
                                    io_channel_id,
                                    user_channel_id,
                                    enable_server_pointer,
                                    pointer_software_rendering,
                                }
                                .build(),
                            );
                            active_stage.set_enable_server_pointer(enable_server_pointer);
                            let _ = on_display.send(DisplayEvent::Size {
                                width: desktop_size.width,
                                height: desktop_size.height,
                            });
                            dirty = None;
                            break;
                        }
                    }
                }
                ActiveStageOutput::Terminate(reason) => return Ok(reason.description()),
            }
        }
        if let Some(rect) = dirty
            && on_frame
                .send(InvokeResponseBody::Raw(encode_rect(&image, &rect)))
                .is_err()
        {
            // The tab is gone.
            return Ok("closed".into());
        }
    }
}

/// Plays remote audio on this device's speakers, telling the UI when it starts (or fails).
fn native_audio(
    on_display: &Channel<DisplayEvent>,
) -> impl Fn() -> Option<Box<dyn RdpsndClientHandler>> + use<> {
    let on_display = on_display.clone();
    move || {
        let on_display = on_display.clone();
        Some(
            Box::new(crate::rdp_audio::Speaker::new(Box::new(move |status| {
                let event = match status {
                    crate::rdp_audio::AudioStatus::Playing => DisplayEvent::Audio {
                        playing: true,
                        error: None,
                    },
                    crate::rdp_audio::AudioStatus::Failed(error) => DisplayEvent::Audio {
                        playing: false,
                        error: Some(error),
                    },
                };
                let _ = on_display.send(event);
            }))) as Box<dyn RdpsndClientHandler>,
        )
    }
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub fn rdp_connect(
    app: AppHandle,
    state: State<'_, RdpState>,
    registry: State<'_, Arc<PromptRegistry>>,
    host_id: String,
    width: u16,
    height: u16,
    on_event: Channel<SshEvent>,
    on_display: Channel<DisplayEvent>,
    on_frame: Channel<InvokeResponseBody>,
) -> Result<u32, String> {
    let connector = Connector::for_host(&app, &host_id, &on_event, registry.inner())?;
    let trust = TrustStore::open(&app)?;
    let (commands, mut command_rx) = mpsc::unbounded_channel();
    let (cancel, mut cancel_rx) = watch::channel(false);
    let id = state.next_id.fetch_add(1, Ordering::Relaxed) + 1;
    state
        .sessions
        .lock()
        .map_err(|e| e.to_string())?
        .insert(id, SessionHandle { commands, cancel });

    // IronRDP's connection future is not `Send`, so each session gets a thread with its own
    // single-threaded runtime (which also keeps heavy graphics decoding off the shared one).
    let task = move || async move {
        let session = async {
            #[cfg(desktop)]
            let local: Arc<dyn LocalClipboard> =
                Arc::new(crate::rdp_clipboard::SystemClipboard::new());
            #[cfg(target_os = "ios")]
            let local: Arc<dyn LocalClipboard> = Arc::new(crate::rdp_clipboard::IosClipboard);
            #[cfg(all(mobile, not(target_os = "ios")))]
            let local: Arc<dyn LocalClipboard> =
                Arc::new(crate::rdp_clipboard::MemoryClipboard::default());
            let (link, mut clipboard_events) = ClipboardLink::new(local);
            let audio = native_audio(&on_display);
            let (connection, framed) = open(
                &trust,
                &connector,
                &link.backend(),
                &audio,
                width.max(200),
                height.max(200),
            )
            .await?;
            connector.prompter.notify(SshEvent::Status {
                stage: Stage::Connected,
            });
            link.watch();
            active_session(
                framed,
                connection,
                &mut command_rx,
                (&link, &mut clipboard_events),
                &on_display,
                &on_frame,
            )
            .await
        };
        let outcome = tokio::select! {
            result = session => result,
            _ = cancel_rx.wait_for(|cancelled| *cancelled) => Ok("closed".into()),
        };
        if let Ok(mut sessions) = app.state::<RdpState>().sessions.lock() {
            sessions.remove(&id);
        }
        let error = match outcome {
            Ok(_) => None,
            Err(e) if e.is::<Cancelled>() => None,
            Err(e) => Some(format!("{e:#}")),
        };
        let _ = on_event.send(SshEvent::Closed {
            exit_code: None,
            error,
        });
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    std::thread::Builder::new()
        .name(format!("rdp-{id}"))
        .spawn(move || runtime.block_on(task()))
        .map_err(|e| e.to_string())?;
    Ok(id)
}

#[tauri::command]
pub fn rdp_input(state: State<'_, RdpState>, id: u32, ops: Vec<InputOp>) -> Result<(), String> {
    state.send(id, Command::Input(ops))
}

/// Re-announces the local clipboard to the server; the UI calls it when the session gets focus.
#[tauri::command]
pub fn rdp_clipboard_sync(state: State<'_, RdpState>, id: u32) -> Result<(), String> {
    state.send(id, Command::ClipboardSync)
}

/// Saves the files copied on the remote into `dir` (the Downloads folder by default).
/// Progress arrives as clipboard notices on the display channel.
#[tauri::command]
pub fn rdp_clipboard_save(
    app: AppHandle,
    state: State<'_, RdpState>,
    id: u32,
    dir: Option<String>,
) -> Result<(), String> {
    let dir = match dir {
        Some(dir) => PathBuf::from(dir),
        None => app.path().download_dir().map_err(|e| e.to_string())?,
    };
    state.send(id, Command::ClipboardSave(dir))
}

#[tauri::command]
pub fn rdp_clipboard_cancel(state: State<'_, RdpState>, id: u32) -> Result<(), String> {
    state.send(id, Command::ClipboardCancel)
}

/// Asks the server for a new desktop size. `false`: it cannot resize live (no display control
/// channel), so the UI reconnects at the new size.
#[tauri::command]
pub async fn rdp_resize(
    state: State<'_, RdpState>,
    id: u32,
    width: u32,
    height: u32,
) -> Result<bool, String> {
    let (reply, answer) = oneshot::channel();
    state.send(
        id,
        Command::Resize {
            width,
            height,
            reply,
        },
    )?;
    tokio::time::timeout(Duration::from_secs(5), answer)
        .await
        .map_err(|_| "the session did not answer".to_string())?
        .map_err(|_| "the session ended".to_string())
}

#[tauri::command]
pub fn rdp_close(state: State<'_, RdpState>, id: u32) -> Result<(), String> {
    let sessions = state.sessions.lock().map_err(|e| e.to_string())?;
    if let Some(session) = sessions.get(&id) {
        // A graceful disconnect when connected; aborts a connection still in progress.
        let _ = session.commands.send(Command::Close);
        let cancel = session.cancel.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = cancel.send(true);
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_is_split_from_the_user_name() {
        assert_eq!(
            split_user("ACME\\admin"),
            ("admin".into(), Some("ACME".into()))
        );
        assert_eq!(split_user("admin"), ("admin".into(), None));
        assert_eq!(
            split_user("admin@acme.local"),
            ("admin@acme.local".into(), None)
        );
    }

    #[test]
    fn dirty_rectangles_are_merged_and_copied_opaque() {
        let merged = union(
            Some(InclusiveRectangle {
                left: 10,
                top: 10,
                right: 19,
                bottom: 19,
            }),
            InclusiveRectangle {
                left: 5,
                top: 15,
                right: 12,
                bottom: 30,
            },
        );
        assert_eq!(
            (merged.left, merged.top, merged.right, merged.bottom),
            (5, 10, 19, 30)
        );

        let image = DecodedImage::new(PixelFormat::RgbA32, 64, 32);
        let message = encode_rect(
            &image,
            &InclusiveRectangle {
                left: 2,
                top: 3,
                right: 5,
                bottom: 4,
            },
        );
        assert_eq!(message[0], 0);
        assert_eq!(u16::from_le_bytes([message[5], message[6]]), 4); // width
        assert_eq!(u16::from_le_bytes([message[7], message[8]]), 2); // height
        assert_eq!(message.len(), 9 + 4 * 2 * 4);
        assert!(message[9..].chunks(4).all(|pixel| pixel[3] == 255));

        // Rectangles reaching past the image are clipped instead of panicking.
        let clipped = encode_rect(
            &image,
            &InclusiveRectangle {
                left: 60,
                top: 30,
                right: 80,
                bottom: 40,
            },
        );
        assert_eq!(u16::from_le_bytes([clipped[5], clipped[6]]), 4);
        assert_eq!(u16::from_le_bytes([clipped[7], clipped[8]]), 2);
    }

    /// Against the same test server: what it does with an odd size and a resize. Live resizing
    /// answers `true` and a new size follows; otherwise `false` (the UI then reconnects).
    #[tokio::test]
    #[ignore = "needs a test RDP server, see the test docs"]
    async fn resizes_live_or_says_it_cannot() {
        use crate::hosts::{AuthMethod, Host, Protocol};
        use crate::ssh::Prompter;
        use serde_json::Value;

        let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let password = env("HT_TEST_RDP_PASSWORD");
        let registry = Arc::new(PromptRegistry::default());
        let on_event = {
            let registry = registry.clone();
            Channel::new(move |body| {
                if let InvokeResponseBody::Json(json) = body {
                    let event: Value = serde_json::from_str(&json).unwrap();
                    if let Some(id) = event["requestId"].as_u64() {
                        let reply = if event["type"] == "certificate" {
                            PromptReply::HostKey { accept: true }
                        } else {
                            PromptReply::Answers {
                                answers: vec![password.clone()],
                                remember: false,
                            }
                        };
                        registry.reply(id, reply).unwrap();
                    }
                }
                Ok(())
            })
        };
        let connector = Connector {
            host: Host {
                id: "rdp-resize".into(),
                name: "rdp".into(),
                hostname: "127.0.0.1".into(),
                port: env("HT_TEST_RDP_PORT").parse().unwrap(),
                username: env("HT_TEST_RDP_USER"),
                protocol: Protocol::Rdp,
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
            },
            known_hosts: std::path::PathBuf::new(),
            prompter: Prompter {
                events: on_event,
                registry,
            },
            save_secret: Arc::new(|_, _| Ok(())),
        };
        let trust = TrustStore::at(
            std::env::temp_dir().join(format!("ht-rdp-trust-r{}.json", std::process::id())),
        );
        let (link, mut clipboard_events) =
            ClipboardLink::new(Arc::new(crate::rdp_clipboard::MemoryClipboard::default()));
        let no_audio = || -> Option<Box<dyn RdpsndClientHandler>> { None };
        // An odd size, as a window can have: the session must still start.
        let (connection, framed) = open(&trust, &connector, &link.backend(), &no_audio, 1001, 701)
            .await
            .expect("connect");
        let sizes = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
        let on_display = {
            let sizes = sizes.clone();
            Channel::new(move |body| {
                if let InvokeResponseBody::Json(json) = body {
                    let event: Value = serde_json::from_str(&json).unwrap();
                    if event["type"] == "size" {
                        sizes.lock().unwrap().push((
                            event["width"].as_u64().unwrap(),
                            event["height"].as_u64().unwrap(),
                        ));
                    }
                }
                Ok(())
            })
        };
        let drawn = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let on_frame = {
            let drawn = drawn.clone();
            Channel::new(move |_| {
                drawn.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
        };
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let driver = async {
            while drawn.load(Ordering::Relaxed) < 20 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let (reply, answer) = oneshot::channel();
            commands
                .send(Command::Resize {
                    width: 1280,
                    height: 800,
                    reply,
                })
                .unwrap();
            let live = answer.await.unwrap();
            if live {
                for _ in 0..100 {
                    if sizes.lock().unwrap().contains(&(1280, 800)) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            commands.send(Command::Close).unwrap();
            live
        };
        let session = tokio::time::timeout(
            Duration::from_secs(60),
            active_session(
                framed,
                connection,
                &mut command_rx,
                (&link, &mut clipboard_events),
                &on_display,
                &on_frame,
            ),
        );
        let (outcome, live) = tokio::join!(session, driver);
        outcome.expect("session timed out").expect("session failed");
        let sizes = sizes.lock().unwrap().clone();
        println!("sizes: {sizes:?}, live resize: {live}");
        assert!(!sizes.is_empty());
        if live {
            assert_eq!(sizes.last(), Some(&(1280, 800)), "{sizes:?}");
        }
    }

    /// End-to-end test against a real RDP server. Ignored by default; run it with:
    ///
    /// ```sh
    /// docker run -d --name ht-rdp -p 127.0.0.1:3390:3389 --shm-size=1gb lscr.io/linuxserver/rdesktop:ubuntu-xfce
    /// HT_TEST_RDP_PORT=3390 HT_TEST_RDP_USER=abc HT_TEST_RDP_PASSWORD=abc cargo test -- --ignored rdp::tests
    /// ```
    #[tokio::test]
    #[ignore = "needs a test RDP server, see the test docs"]
    async fn connects_trusts_the_certificate_and_streams_the_desktop() {
        use crate::hosts::{AuthMethod, Host, Protocol};
        use crate::rdp_clipboard::Image;
        use crate::ssh::Prompter;
        use serde_json::Value;
        use sha2::Digest;

        // RUST_LOG=ironrdp_rdpsnd=trace shows the channel traffic.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let password = env("HT_TEST_RDP_PASSWORD");
        // A local folder shared with the server as a drive.
        let shared = std::env::temp_dir().join(format!("ht-rdp-share-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&shared);
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("desde-el-mac.txt"), "hola desde el Mac").unwrap();
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let registry = Arc::new(PromptRegistry::default());
        let on_event = {
            let (events, registry) = (events.clone(), registry.clone());
            Channel::new(move |body| {
                let InvokeResponseBody::Json(json) = body else {
                    return Ok(());
                };
                let event: Value = serde_json::from_str(&json).unwrap();
                if let Some(id) = event["requestId"].as_u64() {
                    let reply = match event["type"].as_str() {
                        Some("certificate") => PromptReply::HostKey { accept: true },
                        _ => PromptReply::Answers {
                            answers: vec![password.clone()],
                            remember: false,
                        },
                    };
                    registry.reply(id, reply).unwrap();
                }
                events.lock().unwrap().push(event);
                Ok(())
            })
        };
        let connector = Connector {
            host: Host {
                id: "rdp-test".into(),
                name: "rdp".into(),
                hostname: "127.0.0.1".into(),
                port: env("HT_TEST_RDP_PORT").parse().unwrap(),
                username: env("HT_TEST_RDP_USER"),
                protocol: Protocol::Rdp,
                auth: AuthMethod::Password,
                color: None,
                folder: None,
                rdp_shared_folder: Some(shared.to_string_lossy().into_owned()),
                rdp_commands_host: None,
                has_password: false,
                has_passphrase: false,
                os: None,
                hostealo: None,
                updated_at: 0,
            },
            known_hosts: std::path::PathBuf::new(),
            prompter: Prompter {
                events: on_event,
                registry,
            },
            save_secret: Arc::new(|_, _| Ok(())),
        };
        let trust_file =
            std::env::temp_dir().join(format!("ht-rdp-trust-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&trust_file);
        let trust = TrustStore::at(&trust_file);

        let memory = Arc::new(crate::rdp_clipboard::MemoryClipboard::default());
        let saved_dir = std::env::temp_dir().join(format!("ht-rdp-saved-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&saved_dir);
        let (link, mut clipboard_events) = ClipboardLink::new(memory.clone());
        let audio_bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let recorder = {
            let audio_bytes = audio_bytes.clone();
            move || -> Option<Box<dyn RdpsndClientHandler>> {
                Some(Box::new(AudioRecorder(audio_bytes.clone())))
            }
        };
        let (connection, framed) = open(&trust, &connector, &link.backend(), &recorder, 1024, 768)
            .await
            .expect("connect");
        link.watch();
        let types: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| e["type"].as_str().map(String::from))
            .collect();
        assert!(
            types.contains(&"certificate".to_string()),
            "xrdp uses a self-signed certificate: {types:?}"
        );
        assert!(
            trust
                .get(&format!("rdp:127.0.0.1:{}", env("HT_TEST_RDP_PORT")))
                .as_deref()
                .is_some_and(|f| f.len() == 95)
        );

        let sizes = Arc::new(Mutex::new(Vec::<Value>::new()));
        let on_display = {
            let sizes = sizes.clone();
            Channel::new(move |body| {
                if let InvokeResponseBody::Json(json) = body {
                    sizes
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str(&json).unwrap());
                }
                Ok(())
            })
        };
        // (pixels received, non-black pixels seen)
        let frames = Arc::new(Mutex::new((0usize, 0usize)));
        // The desktop as the UI would compose it, to look at (HT_TEST_RDP_DUMP=path.ppm).
        let canvas = Arc::new(Mutex::new(vec![0u8; 1024 * 768 * 3]));
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let on_frame = {
            let (frames, canvas) = (frames.clone(), canvas.clone());
            Channel::new(move |body| {
                if let InvokeResponseBody::Raw(bytes) = body
                    && bytes[0] == 0
                {
                    let field = |i: usize| {
                        usize::from(u16::from_le_bytes([bytes[1 + i * 2], bytes[2 + i * 2]]))
                    };
                    let (x, y, w, h) = (field(0), field(1), field(2), field(3));
                    let mut canvas = canvas.lock().unwrap();
                    for row in 0..h {
                        for col in 0..w {
                            let src = 9 + (row * w + col) * 4;
                            let dst = ((y + row) * 1024 + x + col) * 3;
                            if dst + 3 <= canvas.len() {
                                canvas[dst..dst + 3].copy_from_slice(&bytes[src..src + 3]);
                            }
                        }
                    }
                    let mut frames = frames.lock().unwrap();
                    frames.0 += (bytes.len() - 9) / 4;
                    frames.1 += bytes[9..]
                        .chunks(4)
                        .filter(|p| p[0] > 16 || p[1] > 16 || p[2] > 16)
                        .count();
                }
                Ok(())
            })
        };
        // Runs next to the session: waits for the desktop, checks the clipboard both ways, hangs up.
        let driver = async {
            let drawn = || {
                let frames = frames.lock().unwrap();
                frames.0 > 700_000 && frames.1 > 5_000
            };
            while !drawn() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            commands
                .send(Command::Input(vec![InputOp::MouseMove { x: 100, y: 100 }]))
                .unwrap();

            // Remote → local: copy on the server, expect it in our clipboard.
            docker(&[
                "exec",
                "-d",
                "-u",
                "abc",
                "-e",
                "DISPLAY=:10",
                "ht-rdp",
                "sh",
                "-c",
                "printf 'desde el servidor' | xclip -selection clipboard",
            ])
            .await;
            let mut remote_to_local = None;
            for _ in 0..100 {
                remote_to_local = memory.text();
                if remote_to_local.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            // Local → remote: copy here, paste on the server.
            memory.set_text("desde el Mac ñ");
            tokio::time::sleep(Duration::from_secs(2)).await;
            let local_to_remote = docker(&[
                "exec",
                "-u",
                "abc",
                "-e",
                "DISPLAY=:10",
                "ht-rdp",
                "timeout",
                "10",
                "xclip",
                "-o",
                "-selection",
                "clipboard",
            ])
            .await;

            // Images, remote → local: a BMP copied on the server arrives as a picture.
            let x = |args: &str| -> Vec<String> {
                [
                    "exec",
                    "-u",
                    "abc",
                    "-e",
                    "DISPLAY=:10",
                    "ht-rdp",
                    "sh",
                    "-c",
                    args,
                ]
                .map(String::from)
                .to_vec()
            };
            let run = |args: Vec<String>| async move {
                docker(&args.iter().map(String::as_str).collect::<Vec<_>>()).await
            };
            let picture = Image {
                width: 3,
                height: 2,
                rgba: [[200, 30, 40, 255], [10, 220, 30, 255], [0, 0, 250, 255]]
                    .repeat(2)
                    .concat(),
            };
            let mut bmp = b"BM".to_vec();
            let dib = crate::rdp_clipboard::dib::encode(&picture);
            bmp.extend_from_slice(&(14 + dib.len() as u32).to_le_bytes());
            bmp.extend_from_slice(&[0, 0, 0, 0]);
            bmp.extend_from_slice(&54u32.to_le_bytes());
            bmp.extend_from_slice(&dib);
            let encoded = data_encoding::BASE64.encode(&bmp);
            run(x(&format!("printf %s {encoded} | base64 -d > /tmp/ht.bmp"))).await;
            let mut copy = x("xclip -selection clipboard -t image/bmp -i /tmp/ht.bmp");
            copy.insert(1, "-d".into());
            run(copy).await;
            let mut image_to_local = None;
            for _ in 0..100 {
                image_to_local = memory.image();
                if image_to_local.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert_eq!(
                image_to_local.as_ref(),
                Some(&picture),
                "remote image not received"
            );

            // Images, local → remote: the server pastes our picture as a BMP.
            let flipped = Image {
                width: 40,
                height: 30,
                rgba: (0..40 * 30)
                    .flat_map(|i| [(i % 40 * 6) as u8, (i / 40 * 8) as u8, 99, 255])
                    .collect(),
            };
            memory.set_image(&flipped);
            tokio::time::sleep(Duration::from_secs(2)).await;
            let pasted = run(x(
                "timeout 10 xclip -o -selection clipboard -t image/bmp | base64 -w0",
            ))
            .await;
            let pasted = data_encoding::BASE64
                .decode(pasted.trim().as_bytes())
                .unwrap_or_default();
            // xrdp turns the DIB into a BMP file but drops its last 14 bytes (the size of the
            // file header it adds): compare what is there. Windows takes the DIB as is.
            let sent = crate::rdp_clipboard::dib::encode(&flipped);
            assert!(
                pasted.len() >= 14 + 40 + 4000 && sent.starts_with(&pasted[14..]),
                "the server did not get our image ({} bytes)",
                pasted.len()
            );

            // Files, remote → local: files copied on the server are listed, then saved here
            // (xrdp copies files only; Windows servers also copy folders).
            run(x("rm -rf /config/ht-clip && mkdir -p /config/ht-clip/carpeta/sub && printf uno > /config/ht-clip/carpeta/a.txt && head -c 700000 /dev/urandom > /config/ht-clip/carpeta/sub/b.bin")).await;
            let mut copy = x(
                "printf 'file:///config/ht-clip/carpeta/a.txt\\nfile:///config/ht-clip/carpeta/sub/b.bin\\n' | xclip -selection clipboard -t text/uri-list",
            );
            copy.insert(1, "-d".into());
            run(copy).await;
            let notice = |kind: &str| {
                sizes
                    .lock()
                    .unwrap()
                    .iter()
                    .rev()
                    .find(|event| event["notice"]["kind"] == kind)
                    .cloned()
            };
            for _ in 0..100 {
                if notice("remoteFiles").is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let listed = notice("remoteFiles").expect("the remote files were not listed");
            assert_eq!(
                listed["notice"]["names"],
                serde_json::json!(["a.txt", "b.bin"]),
                "{listed}"
            );
            commands
                .send(Command::ClipboardSave(saved_dir.clone()))
                .unwrap();
            for _ in 0..200 {
                if notice("saved").is_some() || notice("failed").is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(notice("failed").is_none(), "{:?}", notice("failed"));
            assert!(notice("saved").is_some(), "the remote files were not saved");
            let remote_sum = run(x(
                "sha256sum /config/ht-clip/carpeta/sub/b.bin | cut -c1-64",
            ))
            .await;
            let local = std::fs::read(saved_dir.join("b.bin")).unwrap();
            assert_eq!(
                data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(&local)),
                remote_sum.trim()
            );
            assert_eq!(
                std::fs::read_to_string(saved_dir.join("a.txt")).unwrap(),
                "uno"
            );

            // Audio: play a second of noise on the server, expect it to arrive here.
            docker(&["exec", "-u", "abc", "ht-rdp", "sh", "-c",
                "head -c 176400 /dev/urandom | PULSE_SERVER=unix:/config/.XDG/pulse/native pacat --format=s16le --rate=44100 --channels=2"]).await;
            for _ in 0..50 {
                if audio_bytes.load(Ordering::Relaxed) > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            // Shared drive: the server reads our file and writes one back.
            let mount = "D=$(ls -d /config/thinclient_drives/*/ 2>/dev/null | head -1)";
            let mut read_remotely = String::new();
            let attempts = if std::env::var("HT_TEST_RDP_DRIVE").is_ok() {
                30
            } else {
                1
            };
            for _ in 0..attempts {
                read_remotely = docker(&[
                    "exec",
                    "-u",
                    "abc",
                    "ht-rdp",
                    "sh",
                    "-c",
                    &format!("{mount}; cat \"$D/desde-el-mac.txt\" 2>/dev/null"),
                ])
                .await;
                if !read_remotely.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            docker(&["exec", "-u", "abc", "ht-rdp", "sh", "-c",
                &format!("{mount}; mkdir -p \"$D/nueva\" && printf 'hola desde el servidor' > \"$D/nueva/desde-el-servidor.txt\"")]).await;

            commands.send(Command::Close).unwrap();
            (remote_to_local, local_to_remote, read_remotely)
        };
        let session = tokio::time::timeout(
            Duration::from_secs(90),
            active_session(
                framed,
                connection,
                &mut command_rx,
                (&link, &mut clipboard_events),
                &on_display,
                &on_frame,
            ),
        );
        let (outcome, (remote_to_local, local_to_remote, read_remotely)) =
            tokio::join!(session, driver);
        // The server always sees the drive; reading and writing it needs an xrdp built with FUSE
        // (thinclient_drives) or a Windows server: set HT_TEST_RDP_DRIVE=1 against one of those.
        let chansrv_log = docker(&[
            "exec",
            "ht-rdp",
            "sh",
            "-c",
            "cat $(find / -name 'xrdp-chansrv*.log' 2>/dev/null | head -1)",
        ])
        .await;
        assert!(
            chansrv_log.contains("Detected remote drive"),
            "the server did not see the shared drive"
        );
        if std::env::var("HT_TEST_RDP_DRIVE").is_ok() {
            assert_eq!(
                read_remotely, "hola desde el Mac",
                "the server could not read the shared drive"
            );
            assert_eq!(
                std::fs::read_to_string(shared.join("nueva").join("desde-el-servidor.txt"))
                    .ok()
                    .as_deref(),
                Some("hola desde el servidor"),
                "the server could not write to the shared drive"
            );
        }
        assert_eq!(remote_to_local.as_deref(), Some("desde el servidor"));
        assert_eq!(local_to_remote.trim_end(), "desde el Mac ñ");
        // xrdp reuses the desktop session of the first login; if that login had no audio channel,
        // the session has no sound output either. Restart the container if this fails.
        assert!(
            audio_bytes.load(Ordering::Relaxed) > 10_000,
            "no remote audio received"
        );
        if let Ok(path) = std::env::var("HT_TEST_RDP_DUMP") {
            let mut ppm = b"P6\n1024 768\n255\n".to_vec();
            ppm.extend_from_slice(&canvas.lock().unwrap());
            std::fs::write(path, ppm).unwrap();
        }
        let reason = outcome.expect("session timed out").expect("session failed");

        assert!(reason.contains("user initiated"), "{reason}");
        let first_size = sizes.lock().unwrap().first().cloned().expect("size event");
        assert_eq!(
            (first_size["width"].as_u64(), first_size["height"].as_u64()),
            (Some(1024), Some(768))
        );
        let (pixels, coloured) = *frames.lock().unwrap();
        assert!(pixels > 200_000, "only {pixels} pixels received");
        assert!(
            coloured > 1000,
            "the desktop looks blank: {coloured} non-black pixels"
        );
    }

    /// Runs `docker` with the given arguments and returns its standard output.
    async fn docker(args: &[&str]) -> String {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        tokio::task::spawn_blocking(move || {
            let output = std::process::Command::new("docker")
                .args(&args)
                .output()
                .expect("docker");
            String::from_utf8_lossy(&output.stdout).into_owned()
        })
        .await
        .unwrap()
    }

    /// Counts the audio bytes the server sends instead of playing them.
    #[derive(Debug)]
    struct AudioRecorder(Arc<std::sync::atomic::AtomicUsize>);

    const PCM: [ironrdp::rdpsnd::pdu::AudioFormat; 1] = [ironrdp::rdpsnd::pdu::AudioFormat {
        format: ironrdp::rdpsnd::pdu::WaveFormat::PCM,
        n_channels: 2,
        n_samples_per_sec: 44100,
        n_avg_bytes_per_sec: 176_400,
        n_block_align: 4,
        bits_per_sample: 16,
        data: None,
    }];

    impl RdpsndClientHandler for AudioRecorder {
        fn get_formats(&self) -> &[ironrdp::rdpsnd::pdu::AudioFormat] {
            &PCM
        }

        fn wave(&mut self, _format_no: usize, _ts: u32, data: std::borrow::Cow<'_, [u8]>) {
            self.0.fetch_add(data.len(), Ordering::Relaxed);
        }

        fn set_volume(&mut self, _volume: ironrdp::rdpsnd::pdu::VolumePdu) {}

        fn set_pitch(&mut self, _pitch: ironrdp::rdpsnd::pdu::PitchPdu) {}

        fn close(&mut self) {}
    }
}
