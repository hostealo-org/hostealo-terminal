//! Syncing hosts between the user's devices over the local network, with no server or account.
//!
//! One device waits (advertised on the network over Bonjour/mDNS, only while its sync window is
//! open); another finds it and connects over TCP. Then:
//!
//! 1. Each side sends an ephemeral X25519 public key; both derive the same keys with HKDF.
//! 2. Both screens show a 6-digit code derived from that exchange. If someone sat in the middle,
//!    the codes would differ: the user compares them and accepts on both devices.
//! 3. Each side sends its hosts, folders and deletions (and, only when ticked, the saved
//!    passwords), encrypted with ChaCha20-Poly1305. Both merge the same way and end up equal.
//!
//! When the network hides devices from each other (guest Wi-Fi, office networks), the waiting
//! device also shows a pairing link, as a QR code for a phone or as text to paste on a computer.
//! It carries its addresses and a one-time secret; both ends prove they know it, bound to the
//! keys of this connection, so a man in the middle fails and there is no code to compare.
//!
//! For moving to a new computer, or with no network in common, the same data goes to a backup
//! file encrypted with a key derived from the user's passphrase (Argon2id).
//!
//! Crypto comes from aws-lc-rs, the provider rustls already uses here.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use aws_lc_rs::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use aws_lc_rs::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey, X25519};
use aws_lc_rs::hkdf::{HKDF_SHA256, KeyType, Salt};
use aws_lc_rs::hmac;
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};

use crate::hosts::{Host, HostStore, HostsData, Snippet, Tombstone};
use crate::secrets::{self, SecretKind};

const SERVICE_TYPE: &str = "_hostealo-sync._tcp.local.";
const PROTOCOL_VERSION: u32 = 1;
/// Largest message accepted (a few thousand hosts fit easily).
const MAX_FRAME: usize = 32 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the user has to compare the codes.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(180);
/// Pairing links: `hostealo-terminal://sync?a=<ip:port>,<ip:port>&t=<secret>`.
const PAIRING_PREFIX: &str = "hostealo-terminal://sync?";

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SyncEvent {
    /// Waiting for another device; it can also connect to one of `addresses`.
    Listening {
        device: String,
        addresses: Vec<String>,
        /// Link for the QR code (or to paste on the other device).
        pairing: String,
    },
    /// Another device waiting on the network.
    Found {
        id: String,
        name: String,
        /// `ip:port`, or several separated by commas (`sync_connect` tries each).
        address: String,
    },
    Lost {
        id: String,
    },
    /// Compare this code with the one on `peer` and confirm. `verified`: paired with the link,
    /// so there is nothing to compare, only the confirmation.
    Code {
        peer: String,
        code: String,
        verified: bool,
    },
    /// Both accepted: data is going across.
    Syncing,
    Done {
        summary: MergeSummary,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeSummary {
    pub added: usize,
    pub updated: usize,
    pub deleted: usize,
    pub secrets: usize,
}

// ---------------------------------------------------------------------------------------------
// Merging

/// Two devices' hosts as one: the newest version of each host wins, a deletion wins over any
/// older version, folders are united (minus deleted ones nobody uses). Machine-specific flags
/// (saved secrets) stay as they are here.
pub fn merge(local: &HostsData, remote: &HostsData) -> (HostsData, MergeSummary) {
    let mut deleted: HashMap<String, u64> = HashMap::new();
    for tomb in local.deleted.iter().chain(&remote.deleted) {
        let at = deleted.entry(tomb.id.clone()).or_default();
        *at = (*at).max(tomb.deleted_at);
    }

    let mut hosts: Vec<Host> = Vec::new();
    let mut summary = MergeSummary::default();
    let remote_by_id: HashMap<&str, &Host> =
        remote.hosts.iter().map(|h| (h.id.as_str(), h)).collect();
    let alive = |host: &Host| deleted.get(&host.id).is_none_or(|at| host.updated_at > *at);

    for mine in &local.hosts {
        let chosen = match remote_by_id.get(mine.id.as_str()) {
            Some(theirs) if theirs.updated_at > mine.updated_at => {
                let mut theirs = (*theirs).clone();
                theirs.has_password = mine.has_password;
                theirs.has_passphrase = mine.has_passphrase;
                if alive(&theirs) {
                    summary.updated += 1;
                }
                theirs
            }
            _ => mine.clone(),
        };
        if alive(&chosen) {
            hosts.push(chosen);
        } else {
            summary.deleted += 1;
        }
    }
    for theirs in &remote.hosts {
        if local.hosts.iter().all(|h| h.id != theirs.id) && alive(theirs) {
            let mut new = theirs.clone();
            // The secrets, if any, come separately.
            new.has_password = false;
            new.has_passphrase = false;
            hosts.push(new);
            summary.added += 1;
        }
    }

    let gone: Vec<&String> = local
        .deleted_folders
        .iter()
        .chain(&remote.deleted_folders)
        .collect();
    let mut folders: Vec<String> = local
        .folders
        .iter()
        .chain(&remote.folders)
        .filter(|f| !gone.contains(f))
        .cloned()
        .collect();
    folders.extend(hosts.iter().filter_map(|h| h.folder.clone()));
    folders.sort();
    folders.dedup();

    let mut deleted_folders: Vec<String> = gone
        .into_iter()
        .filter(|f| !folders.contains(f))
        .cloned()
        .collect();
    deleted_folders.sort();
    deleted_folders.dedup();
    let mut ignored: Vec<String> = local
        .hostealo_ignored
        .iter()
        .chain(&remote.hostealo_ignored)
        .cloned()
        .collect();
    ignored.sort();
    ignored.dedup();
    // Quick commands: the same rule as hosts, without counting them in the summary.
    let mut snippets: Vec<Snippet> = Vec::new();
    for snippet in local.snippets.iter().chain(&remote.snippets) {
        match snippets.iter_mut().find(|s| s.id == snippet.id) {
            Some(slot) if snippet.updated_at > slot.updated_at => *slot = snippet.clone(),
            Some(_) => {}
            None => snippets.push(snippet.clone()),
        }
    }
    snippets.retain(|s| deleted.get(&s.id).is_none_or(|at| s.updated_at > *at));
    snippets.sort_by_key(|s| s.name.to_lowercase());

    let mut tombstones: Vec<Tombstone> = deleted
        .into_iter()
        .map(|(id, deleted_at)| Tombstone { id, deleted_at })
        .collect();
    tombstones.sort_by(|a, b| a.id.cmp(&b.id));

    let data = HostsData {
        hosts,
        folders,
        hostealo_ignored: ignored,
        deleted: tombstones,
        deleted_folders,
        snippets,
    };
    (data, summary)
}

// ---------------------------------------------------------------------------------------------
// The encrypted channel

/// A secret that travels with the hosts (only when the user ticked it).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SharedSecret {
    host_id: String,
    passphrase: bool,
    value: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum Message {
    Hello {
        version: u32,
        name: String,
        key: String,
    },
    /// Proof of the pairing secret, when connecting with a link.
    Auth {
        proof: Option<String>,
    },
    Confirm {
        accepted: bool,
    },
    Data {
        hosts: HostsData,
        secrets: Vec<SharedSecret>,
    },
}

struct Len(usize);

impl KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

/// Keys for both directions and the code the user compares.
struct Keys {
    send: LessSafeKey,
    receive: LessSafeKey,
    code: String,
}

fn derive(
    shared: &[u8],
    initiator_key: &[u8],
    responder_key: &[u8],
    initiator: bool,
) -> Result<Keys, String> {
    let prk = Salt::new(HKDF_SHA256, b"hostealo-terminal sync v1").extract(shared);
    let expand = |label: &[u8], len: usize| -> Result<Vec<u8>, String> {
        let mut out = vec![0u8; len];
        prk.expand(&[label, initiator_key, responder_key], Len(len))
            .and_then(|okm| okm.fill(&mut out))
            .map_err(|_| "key derivation failed".to_string())?;
        Ok(out)
    };
    let key = |bytes: Vec<u8>| -> Result<LessSafeKey, String> {
        UnboundKey::new(&CHACHA20_POLY1305, &bytes)
            .map(LessSafeKey::new)
            .map_err(|_| "bad key".to_string())
    };
    let from_initiator = key(expand(b"initiator to responder", 32)?)?;
    let from_responder = key(expand(b"responder to initiator", 32)?)?;
    let code_bytes = expand(b"verification code", 4)?;
    let code = u32::from_be_bytes(code_bytes.try_into().expect("4 bytes")) % 1_000_000;
    let (send, receive) = if initiator {
        (from_initiator, from_responder)
    } else {
        (from_responder, from_initiator)
    };
    Ok(Keys {
        send,
        receive,
        code: format!("{:03} {:03}", code / 1000, code % 1000),
    })
}

/// Proves knowledge of the pairing secret for this very connection (both public keys): a proof
/// made for a connection through someone in the middle is useless on the other leg.
fn pairing_proof(
    secret: &[u8],
    role: &[u8],
    initiator_key: &[u8],
    responder_key: &[u8],
) -> hmac::Tag {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let mut context = hmac::Context::with_key(&key);
    for part in [
        b"hostealo-terminal pairing v1".as_slice(),
        role,
        initiator_key,
        responder_key,
    ] {
        context.update(part);
    }
    context.sign()
}

fn pairing_check(
    secret: &[u8],
    role: &[u8],
    initiator_key: &[u8],
    responder_key: &[u8],
    proof: Option<&str>,
) -> bool {
    let Some(proof) = proof.and_then(|p| data_encoding::BASE64.decode(p.as_bytes()).ok()) else {
        return false;
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let message = [
        b"hostealo-terminal pairing v1".as_slice(),
        role,
        initiator_key,
        responder_key,
    ]
    .concat();
    hmac::verify(&key, &message, &proof).is_ok()
}

/// Builds a pairing link.
fn pairing_link(addresses: &[String], secret: &[u8]) -> String {
    format!(
        "{PAIRING_PREFIX}a={}&t={}",
        addresses.join(","),
        data_encoding::BASE64URL_NOPAD.encode(secret)
    )
}

/// Reads a pairing link: (addresses, secret).
fn parse_pairing(link: &str) -> Option<(Vec<String>, Vec<u8>)> {
    let query = link.trim().strip_prefix(PAIRING_PREFIX)?;
    let (mut addresses, mut secret) = (Vec::new(), None);
    for pair in query.split('&') {
        match pair.split_once('=')? {
            ("a", value) => {
                addresses = value
                    .split(',')
                    .filter(|a| !a.is_empty())
                    .map(String::from)
                    .collect()
            }
            ("t", value) => secret = data_encoding::BASE64URL_NOPAD.decode(value.as_bytes()).ok(),
            _ => {}
        }
    }
    let secret = secret.filter(|s| s.len() >= 16)?;
    (!addresses.is_empty()).then_some((addresses, secret))
}

/// Length-prefixed frames; after the hello, every frame is sealed with a per-direction counter
/// as the nonce (never reused: one key per direction, one counter per key).
struct Channel2 {
    stream: TcpStream,
    keys: Option<Keys>,
    sent: u64,
    received: u64,
}

impl Channel2 {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            keys: None,
            sent: 0,
            received: 0,
        }
    }

    fn nonce(counter: u64) -> Nonce {
        let mut bytes = [0u8; 12];
        bytes[4..].copy_from_slice(&counter.to_be_bytes());
        Nonce::assume_unique_for_key(bytes)
    }

    async fn send(&mut self, message: &Message) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(message).map_err(|e| e.to_string())?;
        if let Some(keys) = &self.keys {
            keys.send
                .seal_in_place_append_tag(Self::nonce(self.sent), Aad::empty(), &mut bytes)
                .map_err(|_| "encryption failed".to_string())?;
            self.sent += 1;
        }
        self.stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .map_err(|e| e.to_string())?;
        self.stream
            .write_all(&bytes)
            .await
            .map_err(|e| e.to_string())?;
        self.stream.flush().await.map_err(|e| e.to_string())
    }

    async fn receive(&mut self) -> Result<Message, String> {
        let mut len = [0u8; 4];
        self.stream
            .read_exact(&mut len)
            .await
            .map_err(|_| "the other device closed the connection".to_string())?;
        let len = u32::from_be_bytes(len) as usize;
        if len > MAX_FRAME {
            return Err("message too large".into());
        }
        let mut bytes = vec![0u8; len];
        self.stream
            .read_exact(&mut bytes)
            .await
            .map_err(|e| e.to_string())?;
        let plain = match &self.keys {
            Some(keys) => {
                let plain = keys
                    .receive
                    .open_in_place(Self::nonce(self.received), Aad::empty(), &mut bytes)
                    .map_err(|_| {
                        "the data was tampered with or the keys do not match".to_string()
                    })?;
                self.received += 1;
                plain.to_vec()
            }
            None => bytes,
        };
        serde_json::from_slice(&plain).map_err(|e| format!("unexpected message: {e}"))
    }
}

// ---------------------------------------------------------------------------------------------
// The exchange

fn device_name() -> String {
    crate::device::display_name().unwrap_or_else(|| "Hostealo Terminal".into())
}

/// Marks this app's own advertisements, so that browsing does not list this device.
fn own_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| uuid::Uuid::new_v4().simple().to_string())
}

/// An mDNS daemon that stops with its owner: dropping the crate's handle leaves its thread
/// running, and with it the advertisement (the same device listed again every time the
/// dialog opened).
struct Mdns(mdns_sd::ServiceDaemon);

impl Drop for Mdns {
    fn drop(&mut self) {
        let _ = self.0.shutdown();
    }
}

/// The addresses of a device found on the network, the usual home and office ones first
/// (192.168/16, then 10/8 and 172.16/12) and without link-local ones.
fn found_addresses(ips: impl IntoIterator<Item = std::net::Ipv4Addr>, port: u16) -> Vec<String> {
    let mut ips: Vec<_> = ips.into_iter().filter(|ip| !ip.is_link_local() && !ip.is_loopback()).collect();
    ips.sort_by_key(|ip| match ip.octets() {
        [192, 168, ..] => 0,
        [10, ..] => 1,
        [172, b, ..] if (16..32).contains(&b) => 2,
        _ => 3,
    });
    ips.dedup();
    ips.into_iter().map(|ip| format!("{ip}:{port}")).collect()
}

/// Whether `link` is a pairing link (opened from the QR code with the camera, for example).
pub fn is_pairing_link(link: &str) -> bool {
    link.trim().starts_with(PAIRING_PREFIX)
}

/// Where a side reads and writes its hosts and secrets (the app's store and the keychain, or
/// memory in tests).
trait SyncStore: Send + Sync + 'static {
    fn load(&self) -> Result<HostsData, String>;
    fn save(&self, data: HostsData) -> Result<(), String>;
    fn secret(&self, host_id: &str, kind: SecretKind) -> Option<String>;
    fn set_secret(&self, host_id: &str, kind: SecretKind, value: &str) -> bool;
}

struct AppStore(AppHandle);

impl SyncStore for AppStore {
    fn load(&self) -> Result<HostsData, String> {
        self.0
            .state::<HostStore>()
            .with_data(&self.0, |data| data.clone())
    }

    fn save(&self, data: HostsData) -> Result<(), String> {
        let store = self.0.state::<HostStore>();
        store.with_data(&self.0, |current| *current = data)?;
        store.persist(&self.0)
    }

    fn secret(&self, host_id: &str, kind: SecretKind) -> Option<String> {
        secrets::get(host_id, kind).ok().flatten()
    }

    fn set_secret(&self, host_id: &str, kind: SecretKind, value: &str) -> bool {
        secrets::set(host_id, kind, value).is_ok()
    }
}

/// What one side needs to run a sync.
struct Side {
    store: Box<dyn SyncStore>,
    /// The pairing secret: the one this device shows (waiting) or the one read from a link.
    pairing: Option<Vec<u8>>,
    /// The user's answer after comparing the codes: (accepted, also send saved passwords).
    confirm: oneshot::Receiver<(bool, bool)>,
    events: Channel<SyncEvent>,
}

/// The saved passwords of these hosts, when the user chose to share them.
fn read_secrets(store: &dyn SyncStore, hosts: &[Host]) -> Vec<SharedSecret> {
    let mut out = Vec::new();
    for host in hosts {
        for (flag, kind, passphrase) in [
            (host.has_password, SecretKind::Password, false),
            (host.has_passphrase, SecretKind::Passphrase, true),
        ] {
            if flag && let Some(value) = store.secret(&host.id, kind) {
                out.push(SharedSecret {
                    host_id: host.id.clone(),
                    passphrase,
                    value,
                });
            }
        }
    }
    out
}

async fn exchange(stream: TcpStream, initiator: bool, side: Side) -> Result<MergeSummary, String> {
    let mut channel = Channel2::new(stream);
    let rng = SystemRandom::new();
    let private = EphemeralPrivateKey::generate(&X25519, &rng)
        .map_err(|_| "key generation failed".to_string())?;
    let public = private
        .compute_public_key()
        .map_err(|_| "key generation failed".to_string())?;
    let public = public.as_ref().to_vec();
    let encode = |bytes: &[u8]| data_encoding::BASE64.encode(bytes);

    channel
        .send(&Message::Hello {
            version: PROTOCOL_VERSION,
            name: device_name(),
            key: encode(&public),
        })
        .await?;
    let (peer, peer_key) = match channel.receive().await? {
        Message::Hello { version, name, key } if version == PROTOCOL_VERSION => (
            name,
            data_encoding::BASE64
                .decode(key.as_bytes())
                .map_err(|_| "bad key".to_string())?,
        ),
        Message::Hello { .. } => {
            return Err("the other device runs an incompatible version: update both".into());
        }
        _ => return Err("unexpected message".into()),
    };
    let (initiator_key, responder_key) = if initiator {
        (public.as_slice(), peer_key.as_slice())
    } else {
        (peer_key.as_slice(), public.as_slice())
    };
    let shared = agreement::agree_ephemeral(
        private,
        UnparsedPublicKey::new(&X25519, &peer_key),
        "key agreement failed",
        |shared| Ok(shared.to_vec()),
    )
    .map_err(|e: &str| e.to_string())?;
    let keys = derive(&shared, initiator_key, responder_key, initiator)?;
    let code = keys.code.clone();
    channel.keys = Some(keys);

    // With a pairing link, both ends prove they know its secret.
    let prove = |role: &[u8]| {
        side.pairing.as_deref().map(|secret| {
            data_encoding::BASE64
                .encode(pairing_proof(secret, role, initiator_key, responder_key).as_ref())
        })
    };
    let check = |role: &[u8], proof: Option<&str>| {
        side.pairing
            .as_deref()
            .is_some_and(|secret| pairing_check(secret, role, initiator_key, responder_key, proof))
    };
    let mismatch =
        "the pairing link is not from this device or has expired: show the QR code again";
    let verified = if initiator {
        channel
            .send(&Message::Auth {
                proof: prove(b"initiator"),
            })
            .await?;
        let Message::Auth { proof } = channel.receive().await? else {
            return Err("unexpected message".into());
        };
        if side.pairing.is_some() && !check(b"responder", proof.as_deref()) {
            return Err(mismatch.into());
        }
        side.pairing.is_some()
    } else {
        let Message::Auth { proof } = channel.receive().await? else {
            return Err("unexpected message".into());
        };
        match proof {
            None => {
                channel.send(&Message::Auth { proof: None }).await?;
                false
            }
            Some(proof) if check(b"initiator", Some(&proof)) => {
                channel
                    .send(&Message::Auth {
                        proof: prove(b"responder"),
                    })
                    .await?;
                true
            }
            Some(_) => {
                channel.send(&Message::Auth { proof: None }).await?;
                return Err(mismatch.into());
            }
        }
    };
    let _ = side.events.send(SyncEvent::Code {
        peer,
        code,
        verified,
    });

    // Both users compare the code; nothing else crosses until both accepted.
    let (accepted, include_secrets) = tokio::time::timeout(CONFIRM_TIMEOUT, side.confirm)
        .await
        .ok()
        .and_then(|answer| answer.ok())
        .unwrap_or((false, false));
    channel.send(&Message::Confirm { accepted }).await?;
    if !accepted {
        return Err("cancelled".into());
    }
    match tokio::time::timeout(CONFIRM_TIMEOUT, channel.receive())
        .await
        .map_err(|_| "the other device did not answer".to_string())??
    {
        Message::Confirm { accepted: true } => {}
        Message::Confirm { accepted: false } => {
            return Err("the other device did not accept the code".into());
        }
        _ => return Err("unexpected message".into()),
    }
    let _ = side.events.send(SyncEvent::Syncing);

    let local = side.store.load()?;
    let shared_secrets = if include_secrets {
        read_secrets(side.store.as_ref(), &local.hosts)
    } else {
        Vec::new()
    };
    channel
        .send(&Message::Data {
            hosts: local.clone(),
            secrets: shared_secrets,
        })
        .await?;
    let (remote, incoming) = match channel.receive().await? {
        Message::Data { hosts, secrets } => (hosts, secrets),
        _ => return Err("unexpected message".into()),
    };
    apply(side.store.as_ref(), &local, &remote, incoming)
}

/// Merges what came from the other device (or a backup) into this one, with its secrets.
fn apply(
    store: &dyn SyncStore,
    local: &HostsData,
    remote: &HostsData,
    incoming: Vec<SharedSecret>,
) -> Result<MergeSummary, String> {
    let (mut merged, mut summary) = merge(local, remote);
    for secret in incoming {
        let Some(host) = merged.hosts.iter_mut().find(|h| h.id == secret.host_id) else {
            continue;
        };
        let (kind, flag) = if secret.passphrase {
            (SecretKind::Passphrase, &mut host.has_passphrase)
        } else {
            (SecretKind::Password, &mut host.has_password)
        };
        if store.set_secret(&host.id, kind, &secret.value) {
            *flag = true;
            summary.secrets += 1;
        }
    }
    store.save(merged)?;
    Ok(summary)
}

// ---------------------------------------------------------------------------------------------
// Backup files

/// `HTSYNC` + format version; also authenticated as associated data.
const BACKUP_MAGIC: &[u8; 8] = b"HTSYNC\x00\x01";
const BACKUP_SALT: usize = 16;
const BACKUP_NONCE: usize = 12;
/// Shortest passphrase accepted for a backup.
pub const MIN_PASSPHRASE: usize = 8;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Backup {
    device: String,
    created_at: u64,
    hosts: HostsData,
    secrets: Vec<SharedSecret>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSummary {
    pub hosts: usize,
    pub secrets: usize,
}

/// Argon2id (64 MiB, 3 passes): slow to guess on purpose.
fn backup_key(passphrase: &str, salt: &[u8]) -> Result<LessSafeKey, String> {
    let params = argon2::Params::new(64 * 1024, 3, 1, Some(32)).map_err(|e| e.to_string())?;
    let mut key = [0u8; 32];
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| e.to_string())?;
    UnboundKey::new(&CHACHA20_POLY1305, &key)
        .map(LessSafeKey::new)
        .map_err(|_| "bad key".to_string())
}

fn seal_backup(backup: &Backup, passphrase: &str) -> Result<Vec<u8>, String> {
    if passphrase.chars().count() < MIN_PASSPHRASE {
        return Err(format!(
            "the passphrase needs at least {MIN_PASSPHRASE} characters"
        ));
    }
    let rng = SystemRandom::new();
    let mut salt = [0u8; BACKUP_SALT];
    let mut nonce = [0u8; BACKUP_NONCE];
    rng.fill(&mut salt)
        .and_then(|_| rng.fill(&mut nonce))
        .map_err(|_| "no randomness".to_string())?;
    let mut data = serde_json::to_vec(backup).map_err(|e| e.to_string())?;
    backup_key(passphrase, &salt)?
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(BACKUP_MAGIC),
            &mut data,
        )
        .map_err(|_| "encryption failed".to_string())?;
    Ok([BACKUP_MAGIC.as_slice(), &salt, &nonce, &data].concat())
}

fn open_backup(bytes: &[u8], passphrase: &str) -> Result<Backup, String> {
    let rest = bytes
        .strip_prefix(BACKUP_MAGIC.as_slice())
        .ok_or("this is not a Hostealo Terminal backup, or it is from a newer version")?;
    if rest.len() < BACKUP_SALT + BACKUP_NONCE {
        return Err("the file is damaged".into());
    }
    let (salt, rest) = rest.split_at(BACKUP_SALT);
    let (nonce, sealed) = rest.split_at(BACKUP_NONCE);
    let mut data = sealed.to_vec();
    let nonce =
        Nonce::try_assume_unique_for_key(nonce).map_err(|_| "the file is damaged".to_string())?;
    let plain = backup_key(passphrase, salt)?
        .open_in_place(nonce, Aad::from(BACKUP_MAGIC), &mut data)
        .map_err(|_| "wrong passphrase, or the file is damaged".to_string())?;
    serde_json::from_slice(plain).map_err(|e| format!("the file is damaged: {e}"))
}

/// Saves every host (and, if asked, the saved passwords) to an encrypted backup file.
#[tauri::command]
pub async fn sync_export(
    app: AppHandle,
    path: String,
    passphrase: String,
    include_secrets: bool,
) -> Result<BackupSummary, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let store = AppStore(app);
        let hosts = store.load()?;
        let secrets = if include_secrets {
            read_secrets(&store, &hosts.hosts)
        } else {
            Vec::new()
        };
        let summary = BackupSummary {
            hosts: hosts.hosts.len(),
            secrets: secrets.len(),
        };
        let backup = Backup {
            device: device_name(),
            created_at: crate::hosts::now_ms(),
            hosts,
            secrets,
        };
        std::fs::write(&path, seal_backup(&backup, &passphrase)?).map_err(|e| e.to_string())?;
        Ok(summary)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Merges a backup file into this device, the same way as a sync.
#[tauri::command]
pub async fn sync_import(
    app: AppHandle,
    path: String,
    passphrase: String,
) -> Result<MergeSummary, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let bytes = std::fs::read(crate::device::picked_path(&path)).map_err(|e| e.to_string())?;
        let backup = open_backup(&bytes, &passphrase)?;
        let store = AppStore(app);
        let local = store.load()?;
        apply(&store, &local, &backup.hosts, backup.secrets)
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------------------------------------
// Commands

struct Pending {
    confirm: Option<oneshot::Sender<(bool, bool)>>,
    cancel: watch::Sender<bool>,
}

#[derive(Default)]
pub struct SyncState {
    sessions: Mutex<HashMap<u32, Pending>>,
    next_id: AtomicU32,
    /// A pairing link the app was opened with, until the window takes it.
    opened_link: Mutex<Option<String>>,
}

/// The app was opened with a pairing link (the QR code scanned with the camera): the window
/// gets it now with the `sync-link` event, or asks for it once it has loaded.
pub fn open_link(app: &AppHandle, link: String) {
    if let Ok(mut opened) = app.state::<SyncState>().opened_link.lock() {
        *opened = Some(link.clone());
    }
    let _ = tauri::Emitter::emit(app, "sync-link", link);
}

/// The pairing link the app was opened with, once.
#[tauri::command]
pub fn sync_opened_link(state: State<'_, SyncState>) -> Option<String> {
    state.opened_link.lock().ok()?.take()
}

impl SyncState {
    fn open(&self) -> (u32, oneshot::Receiver<(bool, bool)>, watch::Receiver<bool>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (confirm, confirm_rx) = oneshot::channel();
        let (cancel, cancel_rx) = watch::channel(false);
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.insert(
                id,
                Pending {
                    confirm: Some(confirm),
                    cancel,
                },
            );
        }
        (id, confirm_rx, cancel_rx)
    }
}

/// Runs a session until it ends or is cancelled, then reports how it went.
fn spawn_session(
    app: AppHandle,
    id: u32,
    mut cancel: watch::Receiver<bool>,
    events: Channel<SyncEvent>,
    task: impl std::future::Future<Output = Result<MergeSummary, String>> + Send + 'static,
) {
    tauri::async_runtime::spawn(async move {
        let outcome = tokio::select! {
            result = task => Some(result),
            _ = cancel.wait_for(|c| *c) => None,
        };
        match outcome {
            Some(Ok(summary)) => {
                let _ = events.send(SyncEvent::Done { summary });
            }
            Some(Err(message)) if message != "cancelled" => {
                let _ = events.send(SyncEvent::Error { message });
            }
            _ => {}
        }
        if let Ok(mut sessions) = app.state::<SyncState>().sessions.lock() {
            sessions.remove(&id);
        }
    });
}

/// This computer's addresses on the local network (for connecting by hand).
fn local_addresses(port: u16) -> Vec<String> {
    let mut out: Vec<String> = crate::device::local_ipv4()
        .into_iter()
        .map(|ip| format!("{ip}:{port}"))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Waits for another device. Advertised on the network until a device connects or the session
/// is cancelled.
#[tauri::command]
pub async fn sync_listen(
    app: AppHandle,
    state: State<'_, SyncState>,
    on_event: Channel<SyncEvent>,
) -> Result<u32, String> {
    let listener = TcpListener::bind("0.0.0.0:0")
        .await
        .map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let (id, confirm, cancel) = state.open();
    let device = device_name();
    let mut secret = vec![0u8; 16];
    SystemRandom::new()
        .fill(&mut secret)
        .map_err(|_| "no randomness".to_string())?;
    let addresses = local_addresses(port);
    let _ = on_event.send(SyncEvent::Listening {
        device: device.clone(),
        pairing: pairing_link(&addresses, &secret),
        addresses,
    });

    // Advertise; the advertisement ends with the daemon.
    let daemon = mdns_sd::ServiceDaemon::new().ok().map(Mdns);
    if let Some(Mdns(daemon)) = &daemon {
        let instance = format!(
            "{device}-{}",
            uuid::Uuid::new_v4()
                .simple()
                .to_string()
                .get(..6)
                .unwrap_or("x")
        );
        if let Ok(info) = mdns_sd::ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &format!("{instance}.local."),
            "",
            port,
            &[("name", device.as_str()), ("id", own_id())][..],
        ) {
            let _ = daemon.register(info.enable_addr_auto());
        }
    }

    let side = Side {
        store: Box::new(AppStore(app.clone())),
        pairing: Some(secret),
        confirm,
        events: on_event.clone(),
    };
    spawn_session(app, id, cancel, on_event, async move {
        let (stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
        drop(daemon);
        exchange(stream, false, side).await
    });
    Ok(id)
}

/// Lists the devices waiting on the network, until cancelled.
#[tauri::command]
pub fn sync_browse(
    app: AppHandle,
    state: State<'_, SyncState>,
    on_event: Channel<SyncEvent>,
) -> Result<u32, String> {
    let daemon = Mdns(mdns_sd::ServiceDaemon::new().map_err(|e| e.to_string())?);
    let receiver = daemon.0.browse(SERVICE_TYPE).map_err(|e| e.to_string())?;
    let (id, _confirm, cancel) = state.open();
    let events = on_event.clone();
    spawn_session(app, id, cancel, on_event, async move {
        while let Ok(event) = receiver.recv_async().await {
            match event {
                mdns_sd::ServiceEvent::ServiceResolved(service) => {
                    if service.get_property_val_str("id") == Some(own_id()) {
                        continue;
                    }
                    let addresses =
                        found_addresses(service.get_addresses_v4(), service.get_port());
                    if addresses.is_empty() {
                        continue;
                    }
                    let name = service
                        .get_property_val_str("name")
                        .unwrap_or(service.get_fullname())
                        .to_string();
                    let _ = events.send(SyncEvent::Found {
                        id: service.get_fullname().to_string(),
                        name,
                        address: addresses.join(","),
                    });
                }
                mdns_sd::ServiceEvent::ServiceRemoved(_, fullname) => {
                    let _ = events.send(SyncEvent::Lost { id: fullname });
                }
                _ => {}
            }
        }
        drop(daemon);
        Err("cancelled".into())
    });
    Ok(id)
}

/// Connects to a device found on the network, typed by hand (`ip:port`) or from a pairing link
/// (tries each of its addresses).
#[tauri::command]
pub fn sync_connect(
    app: AppHandle,
    state: State<'_, SyncState>,
    address: String,
    on_event: Channel<SyncEvent>,
) -> Result<u32, String> {
    let (addresses, pairing) = match parse_pairing(&address) {
        Some((addresses, secret)) => (addresses, Some(secret)),
        None if address.trim().starts_with(PAIRING_PREFIX) => {
            return Err("this pairing link is incomplete: copy it again".into());
        }
        // One address, or the several of a device found on the network.
        None => (
            address
                .split(',')
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty())
                .collect(),
            None,
        ),
    };
    let (id, confirm, cancel) = state.open();
    let side = Side {
        store: Box::new(AppStore(app.clone())),
        pairing,
        confirm,
        events: on_event.clone(),
    };
    spawn_session(app, id, cancel, on_event, async move {
        let mut last = "the other device did not answer".to_string();
        for address in addresses {
            match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&address)).await {
                Ok(Ok(stream)) => return exchange(stream, true, side).await,
                Ok(Err(e)) => last = format!("could not connect: {e}"),
                Err(_) => {}
            }
        }
        Err(last)
    });
    Ok(id)
}

/// The user compared the codes: accept or reject.
#[tauri::command]
pub fn sync_confirm(
    state: State<'_, SyncState>,
    id: u32,
    accepted: bool,
    include_secrets: bool,
) -> Result<(), String> {
    let mut sessions = state.sessions.lock().map_err(|e| e.to_string())?;
    let session = sessions.get_mut(&id).ok_or("this sync has ended")?;
    if let Some(confirm) = session.confirm.take() {
        let _ = confirm.send((accepted, include_secrets));
    }
    Ok(())
}

#[tauri::command]
pub fn sync_cancel(state: State<'_, SyncState>, id: u32) -> Result<(), String> {
    if let Some(session) = state
        .sessions
        .lock()
        .map_err(|e| e.to_string())?
        .remove(&id)
    {
        let _ = session.cancel.send(true);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::{AuthMethod, Protocol};

    fn host(id: &str, name: &str, updated_at: u64) -> Host {
        Host {
            id: id.into(),
            name: name.into(),
            hostname: format!("{id}.example.com"),
            port: 22,
            username: "root".into(),
            protocol: Protocol::Ssh,
            auth: AuthMethod::Password,
            color: None,
            folder: Some("Clientes".into()),
            rdp_shared_folder: None,
            rdp_commands_host: None,
            has_password: false,
            has_passphrase: false,
            os: None,
            hostealo: None,
            updated_at,
        }
    }

    #[test]
    fn newest_versions_and_deletions_win_on_both_sides() {
        let mut mine_a = host("a", "web (mío)", 200);
        mine_a.has_password = true;
        let local = HostsData {
            hosts: vec![mine_a, host("b", "db", 100), host("c", "old", 100)],
            folders: vec!["Clientes".into(), "Vacía".into()],
            deleted: vec![Tombstone {
                id: "d".into(),
                deleted_at: 300,
            }],
            ..HostsData::default()
        };
        let remote = HostsData {
            hosts: vec![
                host("a", "web (suyo)", 100),
                host("b", "db renamed", 150),
                host("d", "deleted here", 250),
                host("e", "new there", 50),
            ],
            folders: vec!["Clientes".into()],
            deleted: vec![Tombstone {
                id: "c".into(),
                deleted_at: 120,
            }],
            deleted_folders: vec!["Vacía".into()],
            ..HostsData::default()
        };
        let (merged, summary) = merge(&local, &remote);
        let names: Vec<_> = merged
            .hosts
            .iter()
            .map(|h| (h.id.as_str(), h.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [("a", "web (mío)"), ("b", "db renamed"), ("e", "new there")]
        );
        assert_eq!(
            summary,
            MergeSummary {
                added: 1,
                updated: 1,
                deleted: 1,
                secrets: 0
            }
        );
        assert!(merged.hosts[0].has_password, "local secret flags are kept");
        assert_eq!(merged.folders, ["Clientes"]);
        assert_eq!(merged.deleted.len(), 2);

        // The other side reaches the same hosts.
        let (theirs, _) = merge(&remote, &local);
        let mut a: Vec<_> = merged.hosts.iter().map(|h| (&h.id, &h.name)).collect();
        let mut b: Vec<_> = theirs.hosts.iter().map(|h| (&h.id, &h.name)).collect();
        a.sort();
        b.sort();
        assert_eq!(a, b);
    }

    #[test]
    fn quick_commands_merge_like_hosts() {
        let snippet = |id: &str, command: &str, updated_at: u64| Snippet {
            id: id.into(),
            name: id.into(),
            command: command.into(),
            updated_at,
        };
        let laptop = HostsData {
            snippets: vec![
                snippet("logs", "journalctl -f", 100),
                snippet("old", "uptime", 50),
            ],
            ..HostsData::default()
        };
        // The phone deleted "old" after its last change.
        let phone = HostsData {
            snippets: vec![
                snippet("logs", "journalctl -fu nginx", 200),
                snippet("disk", "df -h", 10),
            ],
            deleted: vec![Tombstone {
                id: "old".into(),
                deleted_at: 60,
            }],
            ..HostsData::default()
        };

        for (a, b) in [(&laptop, &phone), (&phone, &laptop)] {
            let (merged, summary) = merge(a, b);
            let got: Vec<(&str, &str)> = merged
                .snippets
                .iter()
                .map(|s| (s.id.as_str(), s.command.as_str()))
                .collect();
            assert_eq!(got, [("disk", "df -h"), ("logs", "journalctl -fu nginx")]);
            assert_eq!(summary, MergeSummary::default());
        }
    }

    #[test]
    fn both_sides_derive_the_same_keys_and_code() {
        let rng = SystemRandom::new();
        let a = EphemeralPrivateKey::generate(&X25519, &rng).unwrap();
        let b = EphemeralPrivateKey::generate(&X25519, &rng).unwrap();
        let (pa, pb) = (
            a.compute_public_key().unwrap().as_ref().to_vec(),
            b.compute_public_key().unwrap().as_ref().to_vec(),
        );
        let sa = agreement::agree_ephemeral(a, UnparsedPublicKey::new(&X25519, &pb), (), |s| {
            Ok(s.to_vec())
        })
        .unwrap();
        let sb = agreement::agree_ephemeral(b, UnparsedPublicKey::new(&X25519, &pa), (), |s| {
            Ok(s.to_vec())
        })
        .unwrap();
        let ka = derive(&sa, &pa, &pb, true).unwrap();
        let kb = derive(&sb, &pa, &pb, false).unwrap();
        assert_eq!(ka.code, kb.code);
        assert_eq!(ka.code.len(), 7);

        let mut sealed = b"hola".to_vec();
        ka.send
            .seal_in_place_append_tag(Channel2::nonce(0), Aad::empty(), &mut sealed)
            .unwrap();
        assert_eq!(
            kb.receive
                .open_in_place(Channel2::nonce(0), Aad::empty(), &mut sealed.clone())
                .unwrap(),
            b"hola"
        );
        // A man in the middle would have a different exchange: another code, and keys that fail.
        let c = EphemeralPrivateKey::generate(&X25519, &rng).unwrap();
        let pc = c.compute_public_key().unwrap().as_ref().to_vec();
        let sc = agreement::agree_ephemeral(c, UnparsedPublicKey::new(&X25519, &pa), (), |s| {
            Ok(s.to_vec())
        })
        .unwrap();
        let kc = derive(&sc, &pa, &pc, false).unwrap();
        assert_ne!(kc.code, ka.code);
        assert!(
            kc.receive
                .open_in_place(Channel2::nonce(0), Aad::empty(), &mut sealed)
                .is_err()
        );
    }

    /// Two ends of a real TCP connection run the protocol up to the confirmation.
    #[tokio::test]
    async fn the_handshake_shows_the_same_code_on_both_ends() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut channel = Channel2::new(stream);
            let rng = SystemRandom::new();
            let private = EphemeralPrivateKey::generate(&X25519, &rng).unwrap();
            let public = private.compute_public_key().unwrap().as_ref().to_vec();
            channel
                .send(&Message::Hello {
                    version: PROTOCOL_VERSION,
                    name: "B".into(),
                    key: data_encoding::BASE64.encode(&public),
                })
                .await
                .unwrap();
            let Message::Hello { key, .. } = channel.receive().await.unwrap() else {
                panic!()
            };
            let peer = data_encoding::BASE64.decode(key.as_bytes()).unwrap();
            let shared = agreement::agree_ephemeral(
                private,
                UnparsedPublicKey::new(&X25519, &peer),
                (),
                |s| Ok(s.to_vec()),
            )
            .unwrap();
            let keys = derive(&shared, &peer, &public, false).unwrap();
            let code = keys.code.clone();
            channel.keys = Some(keys);
            channel
                .send(&Message::Confirm { accepted: false })
                .await
                .unwrap();
            code
        });
        let stream = TcpStream::connect(address).await.unwrap();
        let mut channel = Channel2::new(stream);
        let rng = SystemRandom::new();
        let private = EphemeralPrivateKey::generate(&X25519, &rng).unwrap();
        let public = private.compute_public_key().unwrap().as_ref().to_vec();
        channel
            .send(&Message::Hello {
                version: PROTOCOL_VERSION,
                name: "A".into(),
                key: data_encoding::BASE64.encode(&public),
            })
            .await
            .unwrap();
        let Message::Hello { key, name, .. } = channel.receive().await.unwrap() else {
            panic!()
        };
        assert_eq!(name, "B");
        let peer = data_encoding::BASE64.decode(key.as_bytes()).unwrap();
        let shared =
            agreement::agree_ephemeral(private, UnparsedPublicKey::new(&X25519, &peer), (), |s| {
                Ok(s.to_vec())
            })
            .unwrap();
        let keys = derive(&shared, &public, &peer, true).unwrap();
        let code = keys.code.clone();
        channel.keys = Some(keys);
        assert!(matches!(
            channel.receive().await.unwrap(),
            Message::Confirm { accepted: false }
        ));
        assert_eq!(server.await.unwrap(), code);
    }

    #[derive(Default)]
    struct MemoryStore {
        data: Mutex<HostsData>,
        secrets: Mutex<HashMap<(String, bool), String>>,
    }

    impl SyncStore for std::sync::Arc<MemoryStore> {
        fn load(&self) -> Result<HostsData, String> {
            Ok(self.data.lock().unwrap().clone())
        }
        fn save(&self, data: HostsData) -> Result<(), String> {
            *self.data.lock().unwrap() = data;
            Ok(())
        }
        fn secret(&self, host_id: &str, kind: SecretKind) -> Option<String> {
            let key = (host_id.to_string(), matches!(kind, SecretKind::Passphrase));
            self.secrets.lock().unwrap().get(&key).cloned()
        }
        fn set_secret(&self, host_id: &str, kind: SecretKind, value: &str) -> bool {
            let key = (host_id.to_string(), matches!(kind, SecretKind::Passphrase));
            self.secrets.lock().unwrap().insert(key, value.to_string());
            true
        }
    }

    /// Two complete syncs against each other over TCP: codes, confirmation, data and secrets.
    #[tokio::test]
    async fn two_devices_end_up_with_the_same_hosts() {
        let laptop = std::sync::Arc::new(MemoryStore::default());
        let mut web = host("web", "web-01", 100);
        web.has_password = true;
        laptop.data.lock().unwrap().hosts = vec![web];
        laptop
            .secrets
            .lock()
            .unwrap()
            .insert(("web".into(), false), "s3cr3t".into());
        let phone = std::sync::Arc::new(MemoryStore::default());
        phone.data.lock().unwrap().hosts = vec![host("db", "db-01", 100)];

        let codes = std::sync::Arc::new(Mutex::new(Vec::new()));
        let side = |store: std::sync::Arc<MemoryStore>, include_secrets: bool| {
            let (confirm, confirm_rx) = oneshot::channel();
            let codes = codes.clone();
            let confirm = Mutex::new(Some(confirm));
            let events = Channel::new(move |body| {
                if let tauri::ipc::InvokeResponseBody::Json(json) = body {
                    let event: serde_json::Value = serde_json::from_str(&json).unwrap();
                    if event["type"] == "code" {
                        codes
                            .lock()
                            .unwrap()
                            .push(event["code"].as_str().unwrap().to_string());
                        // The user compares the code and accepts.
                        let _ = confirm
                            .lock()
                            .unwrap()
                            .take()
                            .unwrap()
                            .send((true, include_secrets));
                    }
                }
                Ok(())
            });
            Side {
                store: Box::new(store),
                pairing: None,
                confirm: confirm_rx,
                events,
            }
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let waiting = side(phone.clone(), false);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            exchange(stream, false, waiting).await
        });
        let stream = TcpStream::connect(address).await.unwrap();
        let client = exchange(stream, true, side(laptop.clone(), true))
            .await
            .unwrap();
        let server = server.await.unwrap().unwrap();

        assert_eq!((client.added, server.added, server.secrets), (1, 1, 1));
        let codes = codes.lock().unwrap().clone();
        assert_eq!(codes.len(), 2);
        assert_eq!(codes[0], codes[1]);
        let names = |store: &MemoryStore| {
            let mut names: Vec<String> = store
                .data
                .lock()
                .unwrap()
                .hosts
                .iter()
                .map(|h| h.name.clone())
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(&laptop), ["db-01", "web-01"]);
        assert_eq!(names(&phone), ["db-01", "web-01"]);
        let key = ("web".to_string(), false);
        assert_eq!(
            phone.secrets.lock().unwrap().get(&key).map(String::as_str),
            Some("s3cr3t")
        );
        assert!(
            phone
                .data
                .lock()
                .unwrap()
                .hosts
                .iter()
                .any(|h| h.id == "web" && h.has_password)
        );
    }

    /// A side that accepts as soon as it is asked, reporting whether it was verified by the link.
    fn auto_side(
        store: std::sync::Arc<MemoryStore>,
        pairing: Option<Vec<u8>>,
        verified: std::sync::Arc<Mutex<Vec<bool>>>,
    ) -> Side {
        let (confirm, confirm_rx) = oneshot::channel();
        let confirm = Mutex::new(Some(confirm));
        let events = Channel::new(move |body| {
            if let tauri::ipc::InvokeResponseBody::Json(json) = body {
                let event: serde_json::Value = serde_json::from_str(&json).unwrap();
                if event["type"] == "code" {
                    verified
                        .lock()
                        .unwrap()
                        .push(event["verified"].as_bool().unwrap());
                    if let Some(confirm) = confirm.lock().unwrap().take() {
                        let _ = confirm.send((true, false));
                    }
                }
            }
            Ok(())
        });
        Side {
            store: Box::new(store),
            pairing,
            confirm: confirm_rx,
            events,
        }
    }

    async fn pair(
        shown: &[u8],
        scanned: Option<Vec<u8>>,
    ) -> (
        Result<MergeSummary, String>,
        Result<MergeSummary, String>,
        Vec<bool>,
    ) {
        let verified = std::sync::Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let waiting = auto_side(Default::default(), Some(shown.to_vec()), verified.clone());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            exchange(stream, false, waiting).await
        });
        let stream = TcpStream::connect(address).await.unwrap();
        let client = exchange(
            stream,
            true,
            auto_side(Default::default(), scanned, verified.clone()),
        )
        .await;
        let server = server.await.unwrap();
        let verified = verified.lock().unwrap().clone();
        (client, server, verified)
    }

    #[tokio::test]
    async fn a_pairing_link_verifies_both_ends_without_a_code() {
        let secret = [7u8; 16];
        let (client, server, verified) = pair(&secret, Some(secret.to_vec())).await;
        assert!(client.is_ok() && server.is_ok());
        assert_eq!(verified, [true, true]);

        // Without a link it still works, with the code to compare.
        let (client, server, verified) = pair(&secret, None).await;
        assert!(client.is_ok() && server.is_ok());
        assert_eq!(verified, [false, false]);
    }

    #[tokio::test]
    async fn a_link_from_another_device_is_refused_on_both_ends() {
        let (client, server, verified) = pair(&[7u8; 16], Some(vec![8u8; 16])).await;
        assert!(client.unwrap_err().contains("pairing link"));
        assert!(server.unwrap_err().contains("pairing link"));
        assert!(verified.is_empty());
    }

    #[test]
    fn found_addresses_put_the_home_network_first() {
        let ips = [
            "100.64.0.7".parse().unwrap(),
            "169.254.3.4".parse().unwrap(),
            "10.8.0.2".parse().unwrap(),
            "192.168.1.52".parse().unwrap(),
        ];
        assert_eq!(
            found_addresses(ips, 5000),
            ["192.168.1.52:5000", "10.8.0.2:5000", "100.64.0.7:5000"]
        );
    }

    #[test]
    fn pairing_links_round_trip() {
        let addresses = vec!["192.168.1.20:5000".to_string(), "10.0.0.3:5000".to_string()];
        let link = pairing_link(&addresses, &[1u8; 16]);
        assert!(link.starts_with("hostealo-terminal://sync?a=192.168.1.20:5000,10.0.0.3:5000&t="));
        assert_eq!(parse_pairing(&link), Some((addresses, vec![1u8; 16])));
        assert_eq!(
            parse_pairing("hostealo-terminal://sync?a=1.2.3.4:5&t=AQ"),
            None
        );
        assert_eq!(parse_pairing("192.168.1.20:5000"), None);
    }

    #[test]
    fn a_backup_opens_only_with_its_passphrase() {
        let mut hosts = HostsData::default();
        hosts.hosts.push(host("web", "web-01", 100));
        let backup = Backup {
            device: "portátil".into(),
            created_at: 1,
            hosts,
            secrets: vec![SharedSecret {
                host_id: "web".into(),
                passphrase: false,
                value: "s3cr3t".into(),
            }],
        };
        assert!(seal_backup(&backup, "corta").is_err());
        let sealed = seal_backup(&backup, "caballo grapa batería").unwrap();
        assert!(!sealed.windows(6).any(|w| w == b"s3cr3t"));
        assert!(!sealed.windows(6).any(|w| w == b"web-01"));
        let opened = open_backup(&sealed, "caballo grapa batería").unwrap();
        assert_eq!(opened.hosts.hosts[0].name, "web-01");
        assert_eq!(opened.secrets[0].value, "s3cr3t");
        assert!(
            open_backup(&sealed, "caballo grapa bateria")
                .unwrap_err()
                .contains("wrong passphrase")
        );
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open_backup(&tampered, "caballo grapa batería").is_err());
        assert!(open_backup(b"PK\x03\x04 zip", "caballo grapa batería").is_err());
    }

    /// Real Bonjour on this machine's network: what `sync_listen` advertises, `sync_browse` finds.
    /// cargo test -- --ignored mdns
    #[tokio::test]
    #[ignore]
    async fn mdns_finds_a_device_waiting_to_sync() {
        let advertiser = mdns_sd::ServiceDaemon::new().unwrap();
        let info = mdns_sd::ServiceInfo::new(
            SERVICE_TYPE,
            "ht-test-device",
            "ht-test-device.local.",
            "",
            45123,
            &[("name", "Equipo de prueba")][..],
        )
        .unwrap()
        .enable_addr_auto();
        advertiser.register(info).unwrap();

        let browser = mdns_sd::ServiceDaemon::new().unwrap();
        let events = browser.browse(SERVICE_TYPE).unwrap();
        let found = tokio::time::timeout(Duration::from_secs(10), async {
            while let Ok(event) = events.recv_async().await {
                if let mdns_sd::ServiceEvent::ServiceResolved(service) = event
                    && service.get_property_val_str("name") == Some("Equipo de prueba")
                {
                    return Some((service.get_port(), service.get_addresses_v4().len()));
                }
            }
            None
        })
        .await
        .expect("not found within 10 s");
        let (port, addresses) = found.unwrap();
        assert_eq!(port, 45123);
        assert!(addresses > 0);
        let _ = advertiser.shutdown();
        let _ = browser.shutdown();
    }
}
