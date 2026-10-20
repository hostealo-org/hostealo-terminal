//! Imports saved connections from other programs: OpenSSH's config, PuTTY, FileZilla, `.rdp`
//! files, MobaXterm and Royal TS / TSX.
//!
//! Reading a file gives a preview the UI shows for review; nothing is saved until the user picks
//! what to import. Passwords that a program stores readably (FileZilla) stay on this side: the
//! preview only says that there is one, and on import it goes straight to the keychain.

mod filezilla;
mod mobaxterm;
mod putty;
mod rdp_file;
mod royal;
mod ssh_config;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use serde::Serialize;
use tauri::{AppHandle, State};

use crate::hosts::{AuthMethod, Host, HostStore, HostsData, Protocol, normalize_folder};
use crate::secrets::{self, SecretKind};

/// Which program a file comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Format {
    SshConfig,
    Putty,
    FileZilla,
    RdpFile,
    MobaXterm,
    RoyalTs,
}

/// A connection read from another program.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedHost {
    pub name: String,
    pub hostname: String,
    pub port: u16,
    pub username: String,
    pub protocol: Protocol,
    /// Folder path inside the source program (`Clients/Acme`).
    pub folder: Option<String>,
    pub key_path: Option<String>,
    /// A colour tag from packages/ui/src/host-colors.ts, when the source had a close one.
    pub color: Option<String>,
    /// Never sent to the UI; saved to the keychain on import.
    #[serde(skip)]
    pub password: Option<String>,
    pub has_password: bool,
    /// The same connection is already in the app.
    pub duplicate: bool,
}

/// What a parser found in a file.
#[derive(Debug, Default)]
pub struct Parsed {
    pub hosts: Vec<ImportedHost>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    /// Pass back to `import_apply`.
    pub token: u32,
    pub format: Format,
    pub source: String,
    pub hosts: Vec<ImportedHost>,
    pub warnings: Vec<String>,
}

/// A file found where a program keeps its connections.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Detected {
    pub format: Format,
    pub path: String,
    pub count: usize,
}

#[derive(Default)]
pub struct ImportState {
    previews: Mutex<HashMap<u32, Vec<ImportedHost>>>,
    next_token: AtomicU32,
}

fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// Where programs keep their connections on this computer.
fn known_locations() -> Vec<PathBuf> {
    let Some(home) = home() else {
        return Vec::new();
    };
    let mut paths = vec![
        home.join(".ssh/config"),
        home.join(".config/filezilla/sitemanager.xml"),
        home.join(".putty/sessions"),
    ];
    if let Some(app_data) = std::env::var_os("APPDATA") {
        let app_data = PathBuf::from(app_data);
        paths.push(app_data.join("FileZilla/sitemanager.xml"));
        paths.push(app_data.join("MobaXterm/MobaXterm.ini"));
    }
    if cfg!(windows) {
        paths.push(home.join("Documents/MobaXterm/MobaXterm.ini"));
    }
    paths
}

/// Reads text in UTF-8, or UTF-16 with a byte order mark (Windows `.reg` exports).
fn read_text(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if let Some(utf16) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = utf16
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        return Ok(String::from_utf16_lossy(&units));
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    Ok(match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        // Older Windows programs write the ANSI code page: Latin-1 keeps accents readable.
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    })
}

/// Stands for PuTTY's sessions in the Windows registry.
const PUTTY_REGISTRY: &str = "registry:putty";

/// Recognises the file and parses it.
fn parse_path(path: &Path) -> Result<(Format, Parsed), String> {
    #[cfg(windows)]
    if path == Path::new(PUTTY_REGISTRY) {
        return putty::from_registry()
            .map(|parsed| (Format::Putty, parsed))
            .ok_or_else(|| "PuTTY has no saved sessions".to_string());
    }
    if path.is_dir() {
        return Ok((Format::Putty, putty::parse_dir(path)?));
    }
    let text = read_text(path)?;
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if extension == "rdp" {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("RDP");
        return Ok((Format::RdpFile, rdp_file::parse(name, &text)));
    }
    if royal::looks_like(&text) {
        return Ok((Format::RoyalTs, royal::parse(&text)?));
    }
    if !text.trim_start().starts_with('<') && matches!(extension.as_str(), "rtsz" | "rtsx") {
        return Err("this Royal TS document is encrypted: in Royal TS, turn off \"Encrypt complete file\" and save it again".into());
    }
    if mobaxterm::looks_like(&text) {
        return Ok((Format::MobaXterm, mobaxterm::parse(&text)));
    }
    if text.contains("<FileZilla3") {
        return Ok((Format::FileZilla, filezilla::parse(&text)?));
    }
    if text.contains("SimonTatham\\PuTTY\\Sessions") {
        return Ok((Format::Putty, putty::parse_reg(&text)));
    }
    if ssh_config::looks_like(&text) {
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        return Ok((Format::SshConfig, ssh_config::parse(&text, &base)));
    }
    Err(format!(
        "{}: not a format that can be imported",
        path.display()
    ))
}

/// The host colour tag closest to an RGB colour (other programs allow any colour).
pub fn nearest_color(r: u8, g: u8, b: u8) -> Option<String> {
    const COLOURS: [(&str, [i32; 3]); 9] = [
        ("red", [239, 68, 68]),
        ("orange", [249, 115, 22]),
        ("amber", [245, 158, 11]),
        ("green", [34, 197, 94]),
        ("cyan", [6, 182, 212]),
        ("blue", [59, 130, 246]),
        ("indigo", [99, 102, 241]),
        ("purple", [168, 85, 247]),
        ("pink", [236, 72, 153]),
    ];
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    // Greys, black and white are no colour at all.
    if r.max(g).max(b) - r.min(g).min(b) < 40 {
        return None;
    }
    COLOURS
        .iter()
        .min_by_key(|(_, [cr, cg, cb])| (r - cr).pow(2) + (g - cg).pow(2) + (b - cb).pow(2))
        .map(|(name, _)| name.to_string())
}

/// The same connection: protocol, address, port and user.
fn same_connection(host: &Host, imported: &ImportedHost) -> bool {
    host.protocol == imported.protocol
        && host.hostname.eq_ignore_ascii_case(&imported.hostname)
        && host.port == imported.port
        && host.username == imported.username
}

/// Files of known programs found on this computer, with how many connections each has.
#[tauri::command]
pub fn import_detect() -> Vec<Detected> {
    let mut locations = known_locations();
    if cfg!(windows) {
        locations.push(PathBuf::from(PUTTY_REGISTRY));
    }
    locations
        .into_iter()
        .filter(|path| path.exists() || path == Path::new(PUTTY_REGISTRY))
        .filter_map(|path| {
            let (format, parsed) = parse_path(&path).ok()?;
            (!parsed.hosts.is_empty()).then(|| Detected {
                format,
                path: path.display().to_string(),
                count: parsed.hosts.len(),
            })
        })
        .collect()
}

/// Reads files to import (several `.rdp` files at once, or one file of another program).
#[tauri::command]
pub fn import_read(
    app: AppHandle,
    state: State<'_, ImportState>,
    store: State<'_, HostStore>,
    paths: Vec<String>,
) -> Result<Preview, String> {
    let mut format = None;
    let mut hosts = Vec::new();
    let mut warnings = Vec::new();
    for path in &paths {
        let (found, parsed) = parse_path(&crate::device::picked_path(path))?;
        if format.is_some_and(|format| format != found) {
            return Err("choose files from one program at a time".into());
        }
        format = Some(found);
        hosts.extend(parsed.hosts);
        warnings.extend(parsed.warnings);
    }
    let format = format.ok_or("no file chosen")?;
    let existing = store.with_data(&app, |data| data.hosts.clone())?;
    for host in &mut hosts {
        host.has_password = host.password.is_some();
        host.duplicate = existing.iter().any(|known| same_connection(known, host));
    }

    let token = state.next_token.fetch_add(1, Ordering::Relaxed) + 1;
    state
        .previews
        .lock()
        .map_err(|e| e.to_string())?
        .insert(token, hosts.clone());
    let source = match paths.as_slice() {
        [one] => one.clone(),
        many => format!("{} files", many.len()),
    };
    Ok(Preview {
        token,
        format,
        source,
        hosts,
        warnings,
    })
}

/// A user name when the source had none: what each protocol would use by default.
fn default_username(protocol: Protocol) -> String {
    match protocol {
        Protocol::Ssh => std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" })
            .unwrap_or_else(|_| "root".into()),
        Protocol::Ftp | Protocol::Ftps => "anonymous".into(),
        Protocol::Rdp => "Administrator".into(),
    }
}

fn to_host(imported: &ImportedHost, into: Option<&str>) -> Host {
    let folder = match (into, &imported.folder) {
        (Some(into), Some(folder)) => normalize_folder(&format!("{into}/{folder}")),
        (Some(into), None) => normalize_folder(into),
        (None, folder) => folder.as_deref().and_then(normalize_folder),
    };
    let username = if imported.username.trim().is_empty() {
        default_username(imported.protocol)
    } else {
        imported.username.trim().to_string()
    };
    Host {
        id: uuid::Uuid::new_v4().to_string(),
        name: if imported.name.trim().is_empty() {
            imported.hostname.clone()
        } else {
            imported.name.trim().to_string()
        },
        hostname: imported.hostname.trim().to_string(),
        port: imported.port,
        username,
        protocol: imported.protocol,
        auth: match &imported.key_path {
            Some(key_path) if imported.protocol == Protocol::Ssh => AuthMethod::Key {
                key_path: key_path.clone(),
            },
            _ => AuthMethod::Password,
        },
        color: imported.color.clone(),
        folder,
        rdp_shared_folder: None,
        rdp_commands_host: None,
        has_password: false,
        has_passphrase: false,
        os: None,
        hostealo: None,
        updated_at: crate::hosts::now_ms(),
    }
}

/// Saves the chosen connections (`selected` indexes the preview), each inside `into` (a folder
/// path, or the top level) keeping the source program's folders below it.
#[tauri::command]
pub fn import_apply(
    app: AppHandle,
    state: State<'_, ImportState>,
    store: State<'_, HostStore>,
    token: u32,
    selected: Vec<usize>,
    into: Option<String>,
) -> Result<HostsData, String> {
    let preview = state
        .previews
        .lock()
        .map_err(|e| e.to_string())?
        .remove(&token)
        .ok_or("this preview has expired: read the file again")?;
    let into = into.as_deref().and_then(normalize_folder);
    let mut hosts = Vec::new();
    for imported in selected.iter().filter_map(|&i| preview.get(i)) {
        if imported.hostname.trim().is_empty() {
            continue;
        }
        let mut host = to_host(imported, into.as_deref());
        if let Some(password) = &imported.password
            && secrets::set(&host.id, SecretKind::Password, password).is_ok()
        {
            host.has_password = true;
        }
        hosts.push(host);
    }
    store.with_data(&app, |data| {
        for host in hosts {
            if let Some(folder) = &host.folder {
                let _ = data.create_folder(folder);
            }
            data.hosts.push(host);
        }
    })?;
    store.persist(&app)?;
    store.with_data(&app, |data| data.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_hosts_keep_their_folders_under_the_chosen_one() {
        let imported = ImportedHost {
            name: "web".into(),
            hostname: "203.0.113.10".into(),
            port: 22,
            username: String::new(),
            protocol: Protocol::Ssh,
            folder: Some("Clientes/Acme".into()),
            key_path: Some("~/.ssh/id_ed25519".into()),
            ..ImportedHost::default()
        };
        let host = to_host(&imported, Some("MobaXterm"));
        assert_eq!(host.folder.as_deref(), Some("MobaXterm/Clientes/Acme"));
        assert!(
            matches!(host.auth, AuthMethod::Key { ref key_path } if key_path == "~/.ssh/id_ed25519")
        );
        assert!(!host.username.is_empty(), "a user name is filled in");
        assert_eq!(
            to_host(&imported, None).folder.as_deref(),
            Some("Clientes/Acme")
        );
    }

    #[test]
    fn utf16_and_latin1_files_are_read() {
        let dir = std::env::temp_dir().join(format!("ht-import-text-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("Año".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        std::fs::write(dir.join("a.reg"), utf16).unwrap();
        assert_eq!(read_text(&dir.join("a.reg")).unwrap(), "Año");
        std::fs::write(dir.join("b.ini"), [b'A', 0xF1, b'o']).unwrap();
        assert_eq!(read_text(&dir.join("b.ini")).unwrap(), "Año");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
