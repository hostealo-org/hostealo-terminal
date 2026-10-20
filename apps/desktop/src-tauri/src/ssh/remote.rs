//! SSH connections without a terminal, for tools that only run commands on a server (the Docker
//! manager): one connection per tool tab, commands on their own exec channels.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::Handle;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State};

use super::{ClientHandler, Connector, PromptRegistry, SshEvent, Stage, auth};
use crate::exec::{self, ExecOutput, LARGE_OUTPUT};

#[derive(Default)]
pub struct RemoteState {
    connections: Mutex<HashMap<u32, Arc<Handle<ClientHandler>>>>,
    next_id: AtomicU32,
}

/// Connects to a saved host. Events go to `on_event` as for a terminal session (prompts, status,
/// `closed` with the error if it fails); the id is usable once `connected` arrives.
#[tauri::command]
pub fn remote_open(
    app: AppHandle,
    state: State<'_, RemoteState>,
    registry: State<'_, Arc<PromptRegistry>>,
    host_id: String,
    on_event: Channel<SshEvent>,
) -> Result<u32, String> {
    let connector = Connector::for_host(&app, &host_id, &on_event, registry.inner())?;
    let id = state.next_id.fetch_add(1, Ordering::Relaxed) + 1;
    tauri::async_runtime::spawn(async move {
        match connector.connect().await {
            Ok(handle) => {
                if let Ok(mut connections) = app.state::<RemoteState>().connections.lock() {
                    connections.insert(id, Arc::new(handle));
                }
                let _ = on_event.send(SshEvent::Status {
                    stage: Stage::Connected,
                });
            }
            Err(e) => {
                let error = (!e.is::<auth::Cancelled>()).then(|| format!("{e:#}"));
                let _ = on_event.send(SshEvent::Closed {
                    exit_code: None,
                    error,
                });
            }
        }
    });
    Ok(id)
}

/// Runs a command on the connection's server and returns all it printed (up to 8 MB).
#[tauri::command]
pub async fn remote_exec(
    state: State<'_, RemoteState>,
    id: u32,
    command: String,
    timeout_secs: u64,
) -> Result<ExecOutput, String> {
    let handle = state
        .connections
        .lock()
        .map_err(|e| e.to_string())?
        .get(&id)
        .cloned()
        .ok_or("not connected")?;
    let timeout = Duration::from_secs(timeout_secs.clamp(1, 3600));
    let output = exec::ssh_limited(&handle, &command, timeout, LARGE_OUTPUT).await;
    // A dead connection: forget it, so the UI shows "disconnected" and can reconnect.
    if output.is_err() && handle.is_closed() {
        state
            .connections
            .lock()
            .map_err(|e| e.to_string())?
            .remove(&id);
    }
    output
}

#[tauri::command]
pub fn remote_close(state: State<'_, RemoteState>, id: u32) -> Result<(), String> {
    state
        .connections
        .lock()
        .map_err(|e| e.to_string())?
        .remove(&id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::{AuthMethod, Host, Protocol};
    use crate::ssh::{PromptReply, Prompter};
    use tauri::ipc::InvokeResponseBody;

    /// The Docker manager's commands over a real SSH connection. A server where the user needs
    /// `sudo` for Docker (see the recipe); the outputs are saved for the UI's parser tests.
    ///
    /// ```sh
    /// docker run -d --name ht-docker-ssh -p 127.0.0.1:2223:2222 -e PASSWORD_ACCESS=true \
    ///   -e USER_NAME=hostealo -e USER_PASSWORD=dockertest \
    ///   -v /var/run/docker.sock:/var/run/docker.sock lscr.io/linuxserver/openssh-server
    /// docker exec ht-docker-ssh sh -c 'apk add docker-cli sudo; \
    ///   echo "hostealo ALL=(ALL) NOPASSWD: /usr/bin/docker" > /etc/sudoers.d/docker'
    /// HT_TEST_DOCKER_PORT=2223 cargo test -- --ignored remote::tests
    /// ```
    #[tokio::test]
    #[ignore = "needs the Docker-over-SSH test server, see the test docs"]
    async fn docker_commands_over_ssh() {
        let port: u16 = std::env::var("HT_TEST_DOCKER_PORT")
            .expect("set HT_TEST_DOCKER_PORT")
            .parse()
            .unwrap();
        let registry = Arc::new(PromptRegistry::default());
        let on_event = {
            let registry = registry.clone();
            Channel::new(move |body| {
                if let InvokeResponseBody::Json(json) = body {
                    let event: serde_json::Value = serde_json::from_str(&json).unwrap();
                    if let Some(id) = event["requestId"].as_u64() {
                        let reply = if event["type"] == "hostKey" {
                            PromptReply::HostKey { accept: true }
                        } else {
                            PromptReply::Answers {
                                answers: vec!["dockertest".into()],
                                remember: false,
                            }
                        };
                        registry.reply(id, reply).unwrap();
                    }
                }
                Ok(())
            })
        };
        let known_hosts =
            std::env::temp_dir().join(format!("ht-docker-known-{}", std::process::id()));
        let connector = Connector {
            host: Host {
                id: "docker-test".into(),
                name: "docker".into(),
                hostname: "127.0.0.1".into(),
                port,
                username: "hostealo".into(),
                protocol: Protocol::Ssh,
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
            known_hosts,
            prompter: Prompter {
                events: on_event,
                registry,
            },
            save_secret: Arc::new(|_, _| Ok(())),
        };
        let handle = connector.connect().await.expect("connect");
        let run = |command: &'static str| {
            let handle = &handle;
            async move {
                exec::ssh_limited(handle, command, Duration::from_secs(60), LARGE_OUTPUT)
                    .await
                    .unwrap()
            }
        };

        let direct = run("docker version --format '{{.Server.Version}}' 2>&1").await;
        assert_ne!(direct.exit_code, Some(0));
        assert!(
            direct.stdout.contains("permission denied"),
            "{}",
            direct.stdout
        );
        let sudo = run("sudo -n docker version --format '{{.Server.Version}}' 2>&1").await;
        assert_eq!(sudo.exit_code, Some(0), "{}", sudo.stdout);
        let ps = run("sudo -n docker ps -a --no-trunc --format '{{json .}}'").await;
        assert_eq!(ps.exit_code, Some(0));
        assert!(ps.stdout.lines().count() >= 2, "{}", ps.stdout);
        let stats = run("sudo -n docker stats --no-stream --format '{{json .}}'").await;
        assert_eq!(stats.exit_code, Some(0));
        if let Ok(dir) = std::env::var("HT_TEST_DOCKER_DUMP") {
            std::fs::write(format!("{dir}/ps.jsonl"), &ps.stdout).unwrap();
            std::fs::write(format!("{dir}/stats.jsonl"), &stats.stdout).unwrap();
        }
    }
}
