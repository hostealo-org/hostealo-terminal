//! Saved hosts and their folders. Connection details are stored as JSON in the app data
//! directory; secrets go to the OS keychain (see `secrets`), and the JSON only records whether
//! one is saved.
//!
//! Folders are paths such as `Clients/Acme`. A host belongs to at most one folder; folders are
//! also kept in their own list so that empty ones survive.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::secrets::{self, SecretKind};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AuthMethod {
    Password,
    #[serde(rename_all = "camelCase")]
    Key {
        key_path: String,
    },
    Agent,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Ssh,
    Ftp,
    /// FTP with explicit TLS (AUTH TLS).
    Ftps,
    /// Remote desktop.
    Rdp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Host {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub protocol: Protocol,
    pub auth: AuthMethod,
    /// Colour tag (see packages/ui/src/host-colors.ts); `None` means the default colour.
    #[serde(default)]
    pub color: Option<String>,
    /// Folder path (`Clients/Acme`), or `None` for the top level.
    #[serde(default)]
    pub folder: Option<String>,
    /// RDP only: local folder shared with the remote desktop as a drive (`~` allowed).
    #[serde(default)]
    pub rdp_shared_folder: Option<String>,
    /// RDP only: the saved SSH host (the same server, with OpenSSH) the AI agent runs commands on.
    #[serde(default)]
    pub rdp_commands_host: Option<String>,
    #[serde(default)]
    pub has_password: bool,
    #[serde(default)]
    pub has_passphrase: bool,
    /// The operating system seen on the last connection (SSH only).
    #[serde(default)]
    pub os: Option<HostOs>,
    /// The Hostealo service this host was added from (see `hostealo`).
    #[serde(default)]
    pub hostealo: Option<HostealoLink>,
    /// Last change, in milliseconds since the Unix epoch: the newest version wins a sync.
    #[serde(default)]
    pub updated_at: u64,
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A deleted host, remembered so a sync with another device does not bring it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tombstone {
    pub id: String,
    pub deleted_at: u64,
}

/// A saved command (a quick command), typed into a terminal with one click.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    pub id: String,
    pub name: String,
    pub command: String,
    /// Last change, in milliseconds since the Unix epoch: the newest version wins a sync.
    #[serde(default)]
    pub updated_at: u64,
}

/// A host that belongs to a service of the user's Hostealo account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostealoLink {
    pub service_id: String,
    /// `vps` or `dedicated`.
    pub kind: String,
}

/// An operating system: `id` picks the logo (see packages/ui/src/os-icons.ts), `name` is shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostOs {
    pub id: String,
    pub name: String,
}

/// What to do with a secret when saving a host.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum SecretChange {
    Keep,
    Set { value: String },
    Clear,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostsData {
    #[serde(default)]
    pub hosts: Vec<Host>,
    #[serde(default)]
    pub folders: Vec<String>,
    /// Hostealo services whose host the user deleted: the sync does not bring them back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hostealo_ignored: Vec<String>,
    /// Hosts and folders deleted here, for syncing with other devices.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted: Vec<Tombstone>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted_folders: Vec<String>,
    /// Quick commands. Deleted ones leave a tombstone in `deleted` too (ids are UUIDs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snippets: Vec<Snippet>,
}

/// `hosts.json` was a plain array of hosts before folders existed.
#[derive(Deserialize)]
#[serde(untagged)]
enum OnDisk {
    V1(Vec<Host>),
    V2(HostsData),
}

/// Trims each segment and drops empty ones: `" Clients / /Acme "` → `Clients/Acme`.
pub fn normalize_folder(path: &str) -> Option<String> {
    let segments: Vec<&str> = path
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    (!segments.is_empty()).then(|| segments.join("/"))
}

fn is_within(path: &str, folder: &str) -> bool {
    path == folder
        || path
            .strip_prefix(folder)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Replaces the `from` prefix of `path` with `to` (`None` = top level).
fn reparent(path: &str, from: &str, to: Option<&str>) -> Option<String> {
    let rest = path[from.len()..].trim_start_matches('/');
    match (to, rest.is_empty()) {
        (Some(to), true) => Some(to.to_string()),
        (Some(to), false) => Some(format!("{to}/{rest}")),
        (None, true) => None,
        (None, false) => Some(rest.to_string()),
    }
}

impl HostsData {
    pub(crate) fn create_folder(&mut self, path: &str) -> Result<(), String> {
        let path = normalize_folder(path).ok_or("empty folder name")?;
        self.deleted_folders.retain(|f| *f != path);
        if !self.folders.contains(&path) {
            self.folders.push(path);
            self.folders.sort();
        }
        Ok(())
    }

    /// Renames or moves a folder with everything inside it.
    fn rename_folder(&mut self, from: &str, to: &str) -> Result<(), String> {
        let from = normalize_folder(from).ok_or("empty folder name")?;
        let to = normalize_folder(to).ok_or("empty folder name")?;
        if is_within(&to, &from) && to != from {
            return Err("a folder cannot be moved inside itself".into());
        }
        self.move_subtree(&from, Some(&to));
        Ok(())
    }

    /// Deletes a folder. Its hosts and subfolders move up one level; no host is deleted.
    fn delete_folder(&mut self, path: &str) -> Result<(), String> {
        let path = normalize_folder(path).ok_or("empty folder name")?;
        let parent = path.rsplit_once('/').map(|(parent, _)| parent.to_string());
        // Its subfolders get new paths: the old ones are gone too.
        let gone: Vec<String> = self
            .folders
            .iter()
            .filter(|f| is_within(f, &path))
            .cloned()
            .collect();
        self.deleted_folders.extend(gone);
        self.deleted_folders.sort();
        self.deleted_folders.dedup();
        self.folders.retain(|f| *f != path);
        self.move_subtree(&path, parent.as_deref());
        Ok(())
    }

    fn move_subtree(&mut self, from: &str, to: Option<&str>) {
        let now = now_ms();
        for host in &mut self.hosts {
            if let Some(folder) = host.folder.as_deref().filter(|f| is_within(f, from)) {
                host.folder = reparent(folder, from, to);
                host.updated_at = now;
            }
        }
        self.folders = self
            .folders
            .iter()
            .filter_map(|f| {
                if is_within(f, from) {
                    reparent(f, from, to)
                } else {
                    Some(f.clone())
                }
            })
            .collect();
        if let Some(to) = to {
            self.folders.push(to.to_string());
        }
        self.dedupe_folders();
    }

    fn dedupe_folders(&mut self) {
        self.folders.sort();
        self.folders.dedup();
    }
}

#[derive(Default)]
pub struct HostStore {
    data: Mutex<Option<HostsData>>,
}

fn hosts_file(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("hosts.json"))
}

impl HostStore {
    /// Runs `f` on the stored data, loading it from disk on first use.
    pub(crate) fn with_data<T>(
        &self,
        app: &AppHandle,
        f: impl FnOnce(&mut HostsData) -> T,
    ) -> Result<T, String> {
        let mut guard = self.data.lock().map_err(|e| e.to_string())?;
        if guard.is_none() {
            let path = hosts_file(app)?;
            let data = match std::fs::read_to_string(&path) {
                Ok(json) => match serde_json::from_str(&json)
                    .map_err(|e| format!("{}: {e}", path.display()))?
                {
                    OnDisk::V1(hosts) => HostsData {
                        hosts,
                        ..HostsData::default()
                    },
                    OnDisk::V2(data) => data,
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => HostsData::default(),
                Err(e) => return Err(e.to_string()),
            };
            *guard = Some(data);
        }
        Ok(f(guard.as_mut().expect("loaded above")))
    }

    fn with_hosts<T>(
        &self,
        app: &AppHandle,
        f: impl FnOnce(&mut Vec<Host>) -> T,
    ) -> Result<T, String> {
        self.with_data(app, |data| f(&mut data.hosts))
    }

    pub(crate) fn persist(&self, app: &AppHandle) -> Result<(), String> {
        let json = self.with_data(app, |data| serde_json::to_string_pretty(data))?;
        let path = hosts_file(app)?;
        // Write then rename, so a crash mid-write cannot corrupt the list.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }

    pub fn get(&self, app: &AppHandle, id: &str) -> Result<Option<Host>, String> {
        self.with_hosts(app, |hosts| hosts.iter().find(|h| h.id == id).cloned())
    }

    /// Records that a secret was saved outside the host dialog (e.g. "remember password" at login).
    pub fn mark_secret(
        &self,
        app: &AppHandle,
        id: &str,
        kind: SecretKind,
        saved: bool,
    ) -> Result<(), String> {
        self.with_hosts(app, |hosts| {
            if let Some(host) = hosts.iter_mut().find(|h| h.id == id) {
                match kind {
                    SecretKind::Password => host.has_password = saved,
                    SecretKind::Passphrase => host.has_passphrase = saved,
                }
            }
        })?;
        self.persist(app)
    }
}

fn apply_secret(host: &mut Host, kind: SecretKind, change: SecretChange) -> Result<(), String> {
    let saved = match change {
        SecretChange::Keep => return Ok(()),
        SecretChange::Set { value } if !value.is_empty() => {
            secrets::set(&host.id, kind, &value)?;
            true
        }
        SecretChange::Set { .. } | SecretChange::Clear => {
            secrets::delete(&host.id, kind)?;
            false
        }
    };
    match kind {
        SecretKind::Password => host.has_password = saved,
        SecretKind::Passphrase => host.has_passphrase = saved,
    }
    Ok(())
}

#[tauri::command]
pub fn hosts_list(app: AppHandle, store: State<'_, HostStore>) -> Result<HostsData, String> {
    store.with_data(&app, |data| data.clone())
}

/// Applies a folder change, saves, and returns the new hosts and folders.
fn change_folders(
    app: &AppHandle,
    store: &HostStore,
    change: impl FnOnce(&mut HostsData) -> Result<(), String>,
) -> Result<HostsData, String> {
    store.with_data(app, change)??;
    store.persist(app)?;
    store.with_data(app, |data| data.clone())
}

#[tauri::command]
pub fn folders_create(
    app: AppHandle,
    store: State<'_, HostStore>,
    path: String,
) -> Result<HostsData, String> {
    change_folders(&app, &store, |data| data.create_folder(&path))
}

#[tauri::command]
pub fn folders_rename(
    app: AppHandle,
    store: State<'_, HostStore>,
    from: String,
    to: String,
) -> Result<HostsData, String> {
    change_folders(&app, &store, |data| data.rename_folder(&from, &to))
}

#[tauri::command]
pub fn folders_delete(
    app: AppHandle,
    store: State<'_, HostStore>,
    path: String,
) -> Result<HostsData, String> {
    change_folders(&app, &store, |data| data.delete_folder(&path))
}

/// Creates the host when `host.id` is empty, otherwise updates it. Returns the saved host.
#[tauri::command]
pub fn hosts_save(
    app: AppHandle,
    store: State<'_, HostStore>,
    mut host: Host,
    password: SecretChange,
    passphrase: SecretChange,
) -> Result<Host, String> {
    if host.hostname.trim().is_empty() || host.username.trim().is_empty() {
        return Err("hostname and username are required".into());
    }
    host.hostname = host.hostname.trim().to_string();
    host.username = host.username.trim().to_string();
    if host.name.trim().is_empty() {
        host.name = host.hostname.clone();
    }
    host.folder = host.folder.as_deref().and_then(normalize_folder);

    let existing = if host.id.is_empty() {
        host.id = uuid::Uuid::new_v4().to_string();
        None
    } else {
        store.get(&app, &host.id)?
    };
    // Secret flags are owned by the backend; never trust the ones sent by the UI.
    host.has_password = existing.as_ref().is_some_and(|h| h.has_password);
    host.has_passphrase = existing.as_ref().is_some_and(|h| h.has_passphrase);
    host.updated_at = now_ms();
    // Detected on connection and set by the Hostealo sync, not edited in the form.
    host.os = existing.as_ref().and_then(|h| h.os.clone());
    host.hostealo = existing.as_ref().and_then(|h| h.hostealo.clone());

    apply_secret(&mut host, SecretKind::Password, password)?;
    apply_secret(&mut host, SecretKind::Passphrase, passphrase)?;

    let saved = host.clone();
    store.with_data(&app, |data| {
        if let Some(folder) = &host.folder
            && !data.folders.contains(folder)
        {
            data.folders.push(folder.clone());
            data.folders.sort();
        }
        match data.hosts.iter_mut().find(|h| h.id == host.id) {
            Some(slot) => *slot = host,
            None => data.hosts.push(host),
        }
    })?;
    store.persist(&app)?;
    Ok(saved)
}

/// Records the operating system detected on a connection.
#[tauri::command]
pub fn hosts_set_os(
    app: AppHandle,
    store: State<'_, HostStore>,
    id: String,
    os: HostOs,
) -> Result<(), String> {
    let changed = store.with_hosts(&app, |hosts| match hosts.iter_mut().find(|h| h.id == id) {
        Some(host) if host.os.as_ref() != Some(&os) => {
            host.os = Some(os);
            true
        }
        _ => false,
    })?;
    if changed {
        store.persist(&app)?;
    }
    Ok(())
}

#[tauri::command]
pub fn hosts_delete(app: AppHandle, store: State<'_, HostStore>, id: String) -> Result<(), String> {
    secrets::delete(&id, SecretKind::Password)?;
    secrets::delete(&id, SecretKind::Passphrase)?;
    store.with_data(&app, |data| {
        if let Some(link) = data
            .hosts
            .iter()
            .find(|h| h.id == id)
            .and_then(|h| h.hostealo.clone())
        {
            data.hostealo_ignored.push(link.service_id);
        }
        if data.hosts.iter().any(|h| h.id == id) {
            data.deleted.push(Tombstone {
                id: id.clone(),
                deleted_at: now_ms(),
            });
        }
        data.hosts.retain(|h| h.id != id);
    })?;
    store.persist(&app)
}

/// A saved password or passphrase, for showing it in the host editor. On macOS the user confirms
/// with Touch ID or the account password first (`reason` is shown in that dialog).
#[tauri::command]
pub async fn hosts_reveal_secret(
    app: AppHandle,
    store: State<'_, HostStore>,
    id: String,
    kind: String,
    reason: String,
) -> Result<String, String> {
    let kind = match kind.as_str() {
        "password" => SecretKind::Password,
        "passphrase" => SecretKind::Passphrase,
        _ => return Err("unknown secret".into()),
    };
    store
        .get(&app, &id)?
        .ok_or("this server no longer exists")?;
    crate::device_auth::confirm_owner(reason).await?;
    secrets::get(&id, kind)?.ok_or_else(|| "nothing is saved for this server".into())
}

/// Creates the quick command when `snippet.id` is empty, otherwise updates it. Returns them all.
#[tauri::command]
pub fn snippets_save(
    app: AppHandle,
    store: State<'_, HostStore>,
    mut snippet: Snippet,
) -> Result<Vec<Snippet>, String> {
    snippet.command = snippet.command.trim_end().to_string();
    if snippet.command.trim().is_empty() {
        return Err("the command is empty".into());
    }
    snippet.name = snippet.name.trim().to_string();
    if snippet.name.is_empty() {
        snippet.name = snippet
            .command
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
    }
    if snippet.id.is_empty() {
        snippet.id = uuid::Uuid::new_v4().to_string();
    }
    snippet.updated_at = now_ms();
    let all = store.with_data(&app, |data| {
        match data.snippets.iter_mut().find(|s| s.id == snippet.id) {
            Some(slot) => *slot = snippet,
            None => data.snippets.push(snippet),
        }
        data.snippets.clone()
    })?;
    store.persist(&app)?;
    Ok(all)
}

#[tauri::command]
pub fn snippets_delete(
    app: AppHandle,
    store: State<'_, HostStore>,
    id: String,
) -> Result<Vec<Snippet>, String> {
    let all = store.with_data(&app, |data| {
        if data.snippets.iter().any(|s| s.id == id) {
            data.deleted.push(Tombstone {
                id: id.clone(),
                deleted_at: now_ms(),
            });
        }
        data.snippets.retain(|s| s.id != id);
        data.snippets.clone()
    })?;
    store.persist(&app)?;
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str, folder: Option<&str>) -> Host {
        Host {
            id: id.into(),
            name: id.into(),
            hostname: "h".into(),
            port: 22,
            username: "u".into(),
            protocol: Protocol::Ssh,
            auth: AuthMethod::Password,
            color: None,
            folder: folder.map(String::from),
            rdp_shared_folder: None,
            rdp_commands_host: None,
            has_password: false,
            has_passphrase: false,
            os: None,
            hostealo: None,
            updated_at: 0,
        }
    }

    fn folders_of(data: &HostsData) -> Vec<(&str, Option<&str>)> {
        data.hosts
            .iter()
            .map(|h| (h.id.as_str(), h.folder.as_deref()))
            .collect()
    }

    #[test]
    fn old_files_without_folders_still_load() {
        let v1 = r#"[{"id":"a","name":"a","hostname":"h","port":22,"username":"u","auth":{"type":"password"}}]"#;
        let OnDisk::V1(hosts) = serde_json::from_str(v1).unwrap() else {
            panic!("expected v1")
        };
        assert_eq!(hosts[0].folder, None);
        let v2 = r#"{"hosts":[],"folders":["Clients"]}"#;
        assert!(
            matches!(serde_json::from_str(v2).unwrap(), OnDisk::V2(d) if d.folders == ["Clients"])
        );
    }

    #[test]
    fn folder_paths_are_normalised() {
        assert_eq!(
            normalize_folder(" Clients / /Acme ").as_deref(),
            Some("Clients/Acme")
        );
        assert_eq!(normalize_folder(" / "), None);
    }

    #[test]
    fn renaming_moves_hosts_and_subfolders_but_not_similar_names() {
        let mut data = HostsData {
            hosts: vec![
                host("a", Some("Clients")),
                host("b", Some("Clients/Acme")),
                host("c", Some("ClientsOld")),
            ],
            folders: vec![
                "Clients".into(),
                "Clients/Acme".into(),
                "ClientsOld".into(),
                "Empty".into(),
            ],
            ..HostsData::default()
        };
        data.rename_folder("Clients", "Customers").unwrap();
        assert_eq!(
            folders_of(&data),
            [
                ("a", Some("Customers")),
                ("b", Some("Customers/Acme")),
                ("c", Some("ClientsOld"))
            ]
        );
        assert_eq!(
            data.folders,
            ["ClientsOld", "Customers", "Customers/Acme", "Empty"]
        );
        assert!(
            data.rename_folder("Customers", "Customers/Acme/Inner")
                .is_err()
        );
    }

    #[test]
    fn deleting_a_folder_moves_its_contents_up() {
        let mut data = HostsData {
            hosts: vec![
                host("a", Some("Clients")),
                host("b", Some("Clients/Acme")),
                host("c", Some("Clients/Acme/Web")),
            ],
            folders: vec![
                "Clients".into(),
                "Clients/Acme".into(),
                "Clients/Acme/Web".into(),
            ],
            ..HostsData::default()
        };
        data.delete_folder("Clients/Acme").unwrap();
        assert_eq!(
            folders_of(&data),
            [
                ("a", Some("Clients")),
                ("b", Some("Clients")),
                ("c", Some("Clients/Web"))
            ]
        );
        assert_eq!(data.folders, ["Clients", "Clients/Web"]);

        data.delete_folder("Clients").unwrap();
        assert_eq!(
            folders_of(&data),
            [("a", None), ("b", None), ("c", Some("Web"))]
        );
        assert_eq!(data.folders, ["Web"]);
    }
}
