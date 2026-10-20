//! The user's Hostealo account, through the panel's REST API (https://my.hostealo.es/api/v1,
//! spec at https://my.hostealo.es/api/openapi.json, `X-API-Key` header).
//!
//! - Sync: VPS and dedicated servers become hosts in `Hostealo/VPS` and `Hostealo/Dedicados`,
//!   linked to their service. A later sync updates their address; names the user changed and
//!   hosts the user deleted are respected.
//! - Status and power actions (start, stop, restart) for VPS.
//!
//! The API key lives in the keychain. A `read` key is enough to sync; power needs `read_write`.

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, State};

use crate::hosts::{AuthMethod, Host, HostStore, HostealoLink, HostsData, Protocol};
use crate::keystore;
use crate::secrets::{self, SecretKind};

const API: &str = "https://my.hostealo.es/api/v1";
const KEYCHAIN_SERVICE: &str = "es.hostealo.terminal";
const KEYCHAIN_ACCOUNT: &str = "hostealo:api-key";
const TIMEOUT: Duration = Duration::from_secs(20);
const VPS_FOLDER: &str = "Hostealo/VPS";
const DEDICATED_FOLDER: &str = "Hostealo/Dedicados";

fn key_entry() -> Result<keystore::Entry, String> {
    keystore::entry(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).map_err(|e| e.to_string())
}

fn stored_key() -> Result<Option<String>, String> {
    match key_entry()?.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(keystore::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("could not read from the system keychain: {e}")),
    }
}

/// A client for the API at `base` (the real one, or a test server).
struct Api {
    base: String,
    key: String,
    http: reqwest::Client,
}

impl Api {
    fn new(base: &str, key: &str) -> Result<Self, String> {
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            key: key.trim().to_string(),
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .user_agent(concat!("HostealoTerminal/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| e.to_string())?,
        })
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value, String> {
        let response = request
            .header("X-API-Key", &self.key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("could not reach Hostealo: {e}"))?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(body);
        }
        let message = body["message"].as_str().unwrap_or_default();
        Err(match status.as_u16() {
            401 => "the API key is not valid (or was revoked)".into(),
            403 => format!(
                "this API key does not allow it{}",
                if message.is_empty() {
                    String::new()
                } else {
                    format!(": {message}")
                }
            ),
            _ if !message.is_empty() => format!("{status}: {message}"),
            _ => status.to_string(),
        })
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        self.send(self.http.get(format!("{}{path}", self.base)))
            .await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        self.send(self.http.post(format!("{}{path}", self.base)).json(&body))
            .await
    }
}

/// Responses come as `{"data": ...}` or bare.
fn data(value: &Value) -> &Value {
    value.get("data").unwrap_or(value)
}

/// The first IPv4 address in a block of service details: well-known fields first, then any
/// string that looks like one.
fn find_ipv4(block: &Value) -> Option<String> {
    let is_ipv4 = |s: &str| s.parse::<std::net::Ipv4Addr>().is_ok();
    for key in ["ipv4", "primary_ip", "main_ip", "ip", "ip_address"] {
        if let Some(ip) = block[key].as_str().filter(|s| is_ipv4(s)) {
            return Some(ip.to_string());
        }
    }
    fn walk(value: &Value, is_ipv4: &dyn Fn(&str) -> bool) -> Option<String> {
        match value {
            Value::String(s) if is_ipv4(s) => Some(s.clone()),
            Value::Array(items) => items.iter().find_map(|v| walk(v, is_ipv4)),
            Value::Object(map) => map.values().find_map(|v| walk(v, is_ipv4)),
            _ => None,
        }
    }
    walk(block, &is_ipv4)
}

/// A server of the account, as the sync sees it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Server {
    pub service_id: String,
    pub kind: String,
    pub name: String,
    pub ip: Option<String>,
    pub status: String,
}

/// Every VPS and dedicated server of the account (all pages), with its address.
async fn servers(api: &Api) -> Result<Vec<Server>, String> {
    let mut services = Vec::new();
    let mut page = 1;
    loop {
        let body = api
            .get(&format!("/services?per_page=100&page={page}"))
            .await?;
        let items = body["data"].as_array().cloned().unwrap_or_default();
        services.extend(items);
        let total_pages = body["meta"]["total_pages"].as_u64().unwrap_or(1);
        if page >= total_pages || page >= 50 {
            break;
        }
        page += 1;
    }

    let mut out = Vec::new();
    for service in services {
        let kind = service["type"].as_str().unwrap_or_default().to_string();
        let status = service["status"].as_str().unwrap_or_default().to_string();
        if !matches!(kind.as_str(), "vps" | "dedicated")
            || matches!(status.as_str(), "cancelled" | "terminated")
        {
            continue;
        }
        let Some(service_id) = service["id"].as_str().map(String::from) else {
            continue;
        };
        let instance = &service["vps_instance"];
        let mut ip = find_ipv4(instance);
        let mut hostname = instance["hostname"].as_str().map(String::from);
        if ip.is_none() {
            // Dedicated servers (and VPS still being set up) have their details on the service.
            if let Ok(detail) = api.get(&format!("/services/{service_id}")).await {
                let detail = data(&detail);
                for key in ["dedicated_instance", "vps_instance"] {
                    let block = detail
                        .get(key)
                        .or_else(|| detail["service"].get(key))
                        .unwrap_or(&Value::Null);
                    ip = ip.or_else(|| find_ipv4(block));
                    hostname = hostname.or_else(|| block["hostname"].as_str().map(String::from));
                }
            }
        }
        let name = service["name"]
            .as_str()
            .filter(|n| !n.trim().is_empty())
            .map(String::from)
            .or(hostname)
            .unwrap_or_else(|| service_id.chars().take(8).collect());
        out.push(Server {
            service_id,
            kind,
            name,
            ip,
            status,
        });
    }
    Ok(out)
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
    /// Servers without an address yet (being set up).
    pub pending: usize,
    pub data: HostsData,
}

/// Merges the account's servers into the hosts.
fn merge(data: &mut HostsData, servers: &[Server], result: &mut SyncResult) {
    for server in servers {
        if data.hostealo_ignored.contains(&server.service_id) {
            continue;
        }
        let Some(ip) = &server.ip else {
            result.pending += 1;
            continue;
        };
        match data.hosts.iter_mut().find(|h| {
            h.hostealo
                .as_ref()
                .is_some_and(|link| link.service_id == server.service_id)
        }) {
            Some(host) if host.hostname == *ip => result.unchanged += 1,
            Some(host) => {
                host.hostname = ip.clone();
                host.updated_at = crate::hosts::now_ms();
                result.updated += 1;
            }
            None => {
                let folder = if server.kind == "vps" {
                    VPS_FOLDER
                } else {
                    DEDICATED_FOLDER
                };
                let _ = data.create_folder(folder);
                data.hosts.push(Host {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: server.name.clone(),
                    hostname: ip.clone(),
                    port: 22,
                    username: "root".into(),
                    protocol: Protocol::Ssh,
                    auth: AuthMethod::Password,
                    color: None,
                    folder: Some(folder.into()),
                    rdp_shared_folder: None,
                    rdp_commands_host: None,
                    has_password: false,
                    has_passphrase: false,
                    os: None,
                    hostealo: Some(HostealoLink {
                        service_id: server.service_id.clone(),
                        kind: server.kind.clone(),
                    }),
                    updated_at: crate::hosts::now_ms(),
                });
                result.added += 1;
            }
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub name: Option<String>,
    pub email: Option<String>,
}

fn account_of(body: &Value) -> Account {
    let user = data(body);
    let user = user.get("user").unwrap_or(user);
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| user[*k].as_str().map(String::from))
    };
    Account {
        name: text(&["name", "full_name", "first_name", "company"]),
        email: text(&["email"]),
    }
}

fn api() -> Result<Api, String> {
    let key = stored_key()?.ok_or("not connected to a Hostealo account")?;
    Api::new(API, &key)
}

/// The connected account, or `None` when no key is saved.
#[tauri::command]
pub async fn hostealo_account() -> Result<Option<Account>, String> {
    let Some(key) = stored_key()? else {
        return Ok(None);
    };
    Ok(Some(account_of(
        &Api::new(API, &key)?.get("/account").await?,
    )))
}

/// Checks the key against the account and saves it.
#[tauri::command]
pub async fn hostealo_connect(api_key: String) -> Result<Account, String> {
    let account = account_of(&Api::new(API, &api_key)?.get("/account").await?);
    key_entry()?
        .set_password(api_key.trim())
        .map_err(|e| format!("could not save to the system keychain: {e}"))?;
    Ok(account)
}

/// Forgets the key. Synced hosts stay.
#[tauri::command]
pub fn hostealo_disconnect() -> Result<(), String> {
    match key_entry()?.delete_credential() {
        Ok(()) | Err(keystore::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("could not delete from the system keychain: {e}")),
    }
}

/// Adds the account's VPS and dedicated servers as hosts, and updates their addresses.
#[tauri::command]
pub async fn hostealo_sync(
    app: AppHandle,
    store: State<'_, HostStore>,
) -> Result<SyncResult, String> {
    let servers = servers(&api()?).await?;
    let mut result = SyncResult::default();
    store.with_data(&app, |data| merge(data, &servers, &mut result))?;
    store.persist(&app)?;
    result.data = store.with_data(&app, |data| data.clone())?;
    Ok(result)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub status: Option<String>,
    pub busy: bool,
    pub cpu: Option<f64>,
    pub ram_used_mb: Option<f64>,
    pub ram_total_mb: Option<f64>,
}

/// Live status of a service (from the panel, not from the server itself).
#[tauri::command]
pub async fn hostealo_status(service_id: String) -> Result<ServerStatus, String> {
    let body = api()?
        .get(&format!("/services/{service_id}/status"))
        .await?;
    let status = data(&body);
    Ok(ServerStatus {
        status: status["status"].as_str().map(String::from),
        busy: status["is_processing"].as_bool().unwrap_or(false),
        cpu: status["cpu_usage"].as_f64(),
        ram_used_mb: status["ram_used_mb"].as_f64(),
        ram_total_mb: status["ram_total_mb"].as_f64(),
    })
}

/// Starts, stops or restarts a VPS.
#[tauri::command]
pub async fn hostealo_power(service_id: String, action: String) -> Result<String, String> {
    if !matches!(action.as_str(), "start" | "stop" | "restart") {
        return Err("unknown power action".into());
    }
    let body = api()?
        .post(
            &format!("/services/{service_id}/power"),
            serde_json::json!({ "action": action }),
        )
        .await?;
    Ok(data(&body)["message"].as_str().unwrap_or("ok").to_string())
}

/// Gives a Hostealo VPS a new root password and saves it in the keychain for its host, so the
/// next connection does not ask. The API cannot read the current password (it is not stored);
/// it can only set a new one. The password never reaches the UI.
#[tauri::command]
pub async fn hostealo_reset_password(
    app: AppHandle,
    store: State<'_, HostStore>,
    host_id: String,
) -> Result<(), String> {
    let host = store
        .get(&app, &host_id)?
        .ok_or("this server no longer exists")?;
    let link = host
        .hostealo
        .as_ref()
        .filter(|link| link.kind == "vps")
        .ok_or("only Hostealo VPS can reset their password from here")?;
    let body = api()?
        .post(
            &format!("/services/{}/reset-password", link.service_id),
            serde_json::json!({}),
        )
        .await?;
    let password = body["password"]
        .as_str()
        .or_else(|| data(&body)["password"].as_str())
        .filter(|p| !p.is_empty())
        .ok_or("the panel did not return the new password: check it in the client area")?;
    secrets::set(&host.id, SecretKind::Password, password)?;
    // The password is kept for the host as it is (its sign-in method does not change).
    store.with_data(&app, |data| {
        if let Some(saved) = data.hosts.iter_mut().find(|h| h.id == host.id) {
            saved.has_password = true;
            saved.updated_at = crate::hosts::now_ms();
        }
    })?;
    store.persist(&app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn addresses_are_found_in_documented_and_undocumented_blocks() {
        assert_eq!(
            find_ipv4(&serde_json::json!({"ipv4": "203.0.113.10"})).as_deref(),
            Some("203.0.113.10")
        );
        let dedicated = serde_json::json!({"hostname": "ds-1", "network": {"ips": [{"address": "2001:db8::1"}, {"address": "198.51.100.7"}]}});
        assert_eq!(find_ipv4(&dedicated).as_deref(), Some("198.51.100.7"));
        assert_eq!(find_ipv4(&serde_json::json!({"ipv4": null})), None);
    }

    fn server(id: &str, kind: &str, ip: Option<&str>) -> Server {
        Server {
            service_id: id.into(),
            kind: kind.into(),
            name: format!("srv-{id}"),
            ip: ip.map(String::from),
            status: "active".into(),
        }
    }

    #[test]
    fn sync_adds_updates_and_respects_user_changes() {
        let mut data = HostsData::default();
        let mut first = SyncResult::default();
        merge(
            &mut data,
            &[
                server("a", "vps", Some("203.0.113.10")),
                server("b", "dedicated", Some("198.51.100.7")),
                server("c", "vps", None),
            ],
            &mut first,
        );
        assert_eq!((first.added, first.pending), (2, 1));
        assert_eq!(data.hosts[0].folder.as_deref(), Some(VPS_FOLDER));
        assert_eq!(data.hosts[1].folder.as_deref(), Some(DEDICATED_FOLDER));
        assert!(data.folders.contains(&VPS_FOLDER.to_string()));

        // The user renames and moves a host; the VPS gets a new address.
        data.hosts[0].name = "web principal".into();
        data.hosts[0].folder = Some("Producción".into());
        let mut second = SyncResult::default();
        merge(
            &mut data,
            &[
                server("a", "vps", Some("203.0.113.99")),
                server("b", "dedicated", Some("198.51.100.7")),
            ],
            &mut second,
        );
        assert_eq!((second.added, second.updated, second.unchanged), (0, 1, 1));
        assert_eq!(data.hosts[0].hostname, "203.0.113.99");
        assert_eq!(data.hosts[0].name, "web principal");
        assert_eq!(data.hosts[0].folder.as_deref(), Some("Producción"));

        // A host the user deleted stays deleted.
        data.hosts
            .retain(|h| h.hostealo.as_ref().is_none_or(|l| l.service_id != "b"));
        data.hostealo_ignored.push("b".into());
        let mut third = SyncResult::default();
        merge(
            &mut data,
            &[server("b", "dedicated", Some("198.51.100.7"))],
            &mut third,
        );
        assert_eq!((third.added, data.hosts.len()), (0, 1));
    }

    /// A tiny HTTP server answering each request with the next canned response.
    async fn serve(
        responses: Vec<(u16, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = vec![0u8; 16 * 1024];
                let read = socket.read(&mut buffer).await.unwrap();
                requests.push(String::from_utf8_lossy(&buffer[..read]).into_owned());
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
            requests
        });
        (base, server)
    }

    #[tokio::test]
    async fn lists_servers_across_pages_and_reads_dedicated_details() {
        let page1 = r#"{"data":[
            {"id":"v1","type":"vps","name":"web-01","status":"active","vps_instance":{"hostname":"web-01.hostealo.es","ipv4":"203.0.113.10"}},
            {"id":"g1","type":"game","name":"minecraft","status":"active"},
            {"id":"old","type":"vps","name":"old","status":"terminated","vps_instance":{"ipv4":"203.0.113.50"}}
          ],"meta":{"page":1,"per_page":100,"total":4,"total_pages":2}}"#;
        let page2 = r#"{"data":[{"id":"d1","type":"dedicated","name":"","status":"active"}],"meta":{"page":2,"total_pages":2}}"#;
        let detail = r#"{"data":{"service":{"id":"d1"},"dedicated_instance":{"hostname":"ds-mad-1","primary_ip":"198.51.100.7"}}}"#;
        let (base, server) = serve(vec![
            (200, page1.into()),
            (200, page2.into()),
            (200, detail.into()),
        ])
        .await;
        let found = servers(&Api::new(&base, "hk_test").unwrap()).await.unwrap();
        assert_eq!(
            found,
            [
                Server {
                    service_id: "v1".into(),
                    kind: "vps".into(),
                    name: "web-01".into(),
                    ip: Some("203.0.113.10".into()),
                    status: "active".into()
                },
                Server {
                    service_id: "d1".into(),
                    kind: "dedicated".into(),
                    name: "ds-mad-1".into(),
                    ip: Some("198.51.100.7".into()),
                    status: "active".into()
                },
            ]
        );
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("GET /api/v1/services?per_page=100&page=1"));
        assert!(requests[0].to_lowercase().contains("x-api-key: hk_test"));
        assert!(requests[2].starts_with("GET /api/v1/services/d1 "));
    }

    #[tokio::test]
    async fn errors_say_what_is_wrong_with_the_key() {
        let (base, _server) = serve(vec![
            (
                401,
                r#"{"success":false,"message":"Unauthenticated."}"#.into(),
            ),
            (
                403,
                r#"{"success":false,"message":"Insufficient permissions"}"#.into(),
            ),
        ])
        .await;
        let api = Api::new(&base, "bad").unwrap();
        assert!(api.get("/account").await.unwrap_err().contains("not valid"));
        let error = api
            .post(
                "/services/v1/power",
                serde_json::json!({"action": "restart"}),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "this API key does not allow it: Insufficient permissions"
        );
    }
}
