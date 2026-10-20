//! End-to-end tests against a real OpenSSH server. Ignored by default; run them with:
//!
//! ```sh
//! docker run -d --name ht-sshd -p 2222:2222 -e PASSWORD_ACCESS=true \
//!   -e USER_NAME=hostealo -e USER_PASSWORD=secret -e PUBLIC_KEY="$(cat test_key.pub)" \
//!   lscr.io/linuxserver/openssh-server
//! HT_TEST_SSH_PORT=2222 HT_TEST_SSH_PASSWORD=secret HT_TEST_SSH_KEY=$PWD/test_key \
//!   HT_TEST_SSH_PASSPHRASE=passphrase cargo test -- --ignored ssh::tests
//! ```

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tauri::ipc::{Channel, InvokeResponseBody};
use tokio::sync::mpsc;

use super::prompts::{PromptRegistry, PromptReply, Prompter};
use super::{Command, Connector, MonitorEvent, SecretSaver, Session};
use crate::hosts::{AuthMethod, Host};
use crate::secrets::SecretKind;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn test_host(auth: AuthMethod) -> Option<Host> {
    Some(Host {
        id: "test".into(),
        name: "test".into(),
        hostname: env("HT_TEST_SSH_HOST").unwrap_or_else(|| "127.0.0.1".into()),
        port: env("HT_TEST_SSH_PORT")?.parse().ok()?,
        username: env("HT_TEST_SSH_USER").unwrap_or_else(|| "hostealo".into()),
        protocol: crate::hosts::Protocol::Ssh,
        auth,
        color: None,
        folder: None,
        rdp_shared_folder: None,
        rdp_commands_host: None,
        has_password: false,
        has_passphrase: false,
        os: None,
        hostealo: None,
        updated_at: 0,
    })
}

#[derive(Default)]
struct Recorded {
    events: Vec<Value>,
    output: Vec<u8>,
    snapshots: usize,
    unsupported: bool,
    saved_secrets: Vec<(String, String)>,
    /// What the agent-mode command printed.
    exec: Option<crate::exec::ExecOutput>,
}

impl Recorded {
    /// Everything a session test waits for before hanging up.
    fn complete(&self) -> bool {
        (self.snapshots > 0 || self.unsupported)
            && self.exec.is_some()
            && self.events.iter().any(|e| e["type"] == "os")
    }
}

/// Runs one session: answers host key prompts with `accept_host_key` and secret prompts with
/// `answers` (in order), types `echo` once connected, and closes after the first monitor sample
/// once the server's operating system is known.
async fn run_session(
    host: Host,
    known_hosts: &std::path::Path,
    accept_host_key: bool,
    answers: Vec<&str>,
) -> (anyhow::Result<Option<u32>>, Recorded) {
    let recorded = Arc::new(Mutex::new(Recorded::default()));
    let registry = Arc::new(PromptRegistry::default());
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();

    let on_event = {
        let (recorded, registry, commands_tx) =
            (recorded.clone(), registry.clone(), commands_tx.clone());
        let answers = Mutex::new(answers.into_iter().map(String::from).collect::<Vec<_>>());
        Channel::new(move |body| {
            let InvokeResponseBody::Json(json) = body else {
                return Ok(());
            };
            let event: Value = serde_json::from_str(&json).unwrap();
            let request_id = event["requestId"].as_u64();
            match (event["type"].as_str(), request_id) {
                (Some("hostKey"), Some(id)) => registry
                    .reply(
                        id,
                        PromptReply::HostKey {
                            accept: accept_host_key,
                        },
                    )
                    .unwrap(),
                (Some("prompt"), Some(id)) => {
                    let mut answers = answers.lock().unwrap();
                    let reply = if answers.is_empty() {
                        PromptReply::Cancel
                    } else {
                        PromptReply::Answers {
                            answers: vec![answers.remove(0)],
                            remember: true,
                        }
                    };
                    registry.reply(id, reply).unwrap();
                }
                (Some("status"), _) if event["stage"] == "connected" => {
                    commands_tx
                        .send(Command::Data(b"echo hello-$((40+2)) term=$TERM colors=$(tput colors 2>/dev/null || echo none)\n".to_vec()))
                        .unwrap();
                    // A command on its own channel, as the assistant's agent mode runs them.
                    let (reply, result) = tokio::sync::oneshot::channel();
                    commands_tx
                        .send(Command::Exec {
                            command: "echo out; echo err >&2; read x; echo \"[$x]\"; exit 7".into(),
                            timeout: Duration::from_secs(10),
                            reply,
                        })
                        .unwrap();
                    let (recorded, commands_tx) = (recorded.clone(), commands_tx.clone());
                    tokio::spawn(async move {
                        let output = result.await.unwrap().unwrap();
                        let mut recorded = recorded.lock().unwrap();
                        recorded.exec = Some(output);
                        if recorded.complete() {
                            let _ = commands_tx.send(Command::Close);
                        }
                    });
                }
                _ => {}
            }
            let mut recorded = recorded.lock().unwrap();
            recorded.events.push(event);
            if recorded.complete() {
                let _ = commands_tx.send(Command::Close);
            }
            Ok(())
        })
    };
    let on_data = {
        let recorded = recorded.clone();
        Channel::new(move |body| {
            if let InvokeResponseBody::Raw(bytes) = body {
                recorded.lock().unwrap().output.extend(bytes);
            }
            Ok(())
        })
    };
    let on_monitor: Channel<MonitorEvent> = {
        let (recorded, commands_tx) = (recorded.clone(), commands_tx.clone());
        Channel::new(move |body| {
            let InvokeResponseBody::Json(json) = body else {
                return Ok(());
            };
            let event: Value = serde_json::from_str(&json).unwrap();
            let mut recorded = recorded.lock().unwrap();
            match event["type"].as_str() {
                Some("snapshot") => {
                    assert!(
                        event["snapshot"]["memTotal"].as_u64().unwrap() > 0,
                        "{event}"
                    );
                    recorded.snapshots += 1;
                }
                _ => recorded.unsupported = true,
            }
            if recorded.complete() {
                let _ = commands_tx.send(Command::Close);
            }
            Ok(())
        })
    };

    let save_secret: Arc<SecretSaver> = {
        let recorded = recorded.clone();
        Arc::new(move |kind: SecretKind, value: &str| {
            recorded
                .lock()
                .unwrap()
                .saved_secrets
                .push((format!("{kind:?}"), value.to_string()));
            Ok(())
        })
    };

    let session = Session {
        connector: Connector {
            host,
            known_hosts: known_hosts.to_path_buf(),
            prompter: Prompter {
                events: on_event,
                registry,
            },
            save_secret,
        },
        cols: 80,
        rows: 24,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        session.run(commands_rx, on_data, on_monitor),
    )
    .await
    .expect("session timed out");
    // The monitor task may still hold its channel for a moment, so take the data out.
    let recorded = std::mem::take(&mut *recorded.lock().unwrap());
    (result, recorded)
}

fn event_types(recorded: &Recorded) -> Vec<&str> {
    recorded
        .events
        .iter()
        .filter_map(|e| e["type"].as_str())
        .collect()
}

fn temp_known_hosts(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ht-test-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("known_hosts");
    let _ = std::fs::remove_file(&file);
    file
}

#[tokio::test]
#[ignore = "needs a test SSH server, see module docs"]
async fn password_login_runs_shell_and_monitor_and_trusts_host_key_once() {
    let (Some(host), Some(password)) =
        (test_host(AuthMethod::Password), env("HT_TEST_SSH_PASSWORD"))
    else {
        panic!("set HT_TEST_SSH_PORT and HT_TEST_SSH_PASSWORD");
    };
    let known_hosts = temp_known_hosts("password");

    // First a wrong password, then the right one: exercises the retry path.
    let (result, recorded) =
        run_session(host.clone(), &known_hosts, true, vec!["wrong", &password]).await;
    result.expect("session failed");
    let output = String::from_utf8_lossy(&recorded.output);
    assert!(output.contains("hello-42"), "shell output: {output}");
    // Full-screen and coloured programs (htop, ls --color) rely on these.
    assert!(
        output.contains("term=xterm-256color"),
        "shell output: {output}"
    );
    assert!(
        output.contains("colors=256") || output.contains("colors=none"),
        "shell output: {output}"
    );
    assert!(recorded.snapshots >= 1 && !recorded.unsupported);
    let prompts: Vec<&Value> = recorded
        .events
        .iter()
        .filter(|e| e["type"] == "prompt")
        .collect();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[1]["retry"], true);
    assert_eq!(
        recorded.saved_secrets,
        [("Password".to_string(), password.clone())]
    );
    assert!(event_types(&recorded).contains(&"hostKey"));
    let exec = recorded.exec.as_ref().expect("agent command ran");
    assert_eq!(
        (
            exec.stdout.as_str(),
            exec.stderr.as_str(),
            exec.exit_code,
            exec.timed_out
        ),
        ("out\n[]\n", "err\n", Some(7), false),
        "stdin is closed, so `read` gets nothing"
    );
    let os = recorded
        .events
        .iter()
        .find(|e| e["type"] == "os")
        .expect("os detected");
    assert_eq!(os["os"]["id"], "alpine", "{os}");
    assert!(
        os["os"]["name"]
            .as_str()
            .unwrap()
            .starts_with("Alpine Linux"),
        "{os}"
    );
    assert!(
        std::fs::read_to_string(&known_hosts)
            .unwrap()
            .contains("ssh-")
    );

    // Second connection: the host key is now trusted, and a saved password skips the prompt.
    let saved = Host {
        has_password: true,
        ..host
    };
    let (result, recorded) = run_session(saved, &known_hosts, false, vec![password.as_str()]).await;
    result.expect("second session failed");
    assert!(
        !event_types(&recorded).contains(&"hostKey"),
        "host key asked again"
    );
}

#[tokio::test]
#[ignore = "needs a test SSH server, see module docs"]
async fn rejecting_the_host_key_cancels_the_session() {
    let Some(host) = test_host(AuthMethod::Password) else {
        panic!("set HT_TEST_SSH_PORT")
    };
    let known_hosts = temp_known_hosts("reject");
    let (result, recorded) = run_session(host, &known_hosts, false, vec![]).await;
    let error = result.expect_err("session should fail");
    assert!(error.is::<super::auth::Cancelled>(), "{error:#}");
    assert!(!event_types(&recorded).contains(&"prompt"));
    assert!(!known_hosts.exists() || std::fs::read_to_string(&known_hosts).unwrap().is_empty());
}

#[tokio::test]
#[ignore = "needs a test SSH server, see module docs"]
async fn encrypted_key_asks_for_passphrase() {
    let (Some(key), Some(passphrase)) = (env("HT_TEST_SSH_KEY"), env("HT_TEST_SSH_PASSPHRASE"))
    else {
        panic!("set HT_TEST_SSH_KEY and HT_TEST_SSH_PASSPHRASE");
    };
    let host = test_host(AuthMethod::Key { key_path: key }).expect("set HT_TEST_SSH_PORT");
    let known_hosts = temp_known_hosts("key");
    let (result, recorded) = run_session(host, &known_hosts, true, vec![&passphrase]).await;
    result.expect("key session failed");
    assert!(String::from_utf8_lossy(&recorded.output).contains("hello-42"));
    let prompt = recorded
        .events
        .iter()
        .find(|e| e["type"] == "prompt")
        .expect("passphrase prompt");
    assert_eq!(prompt["kind"], "passphrase");
    assert_eq!(
        recorded.saved_secrets,
        [("Passphrase".to_string(), passphrase)]
    );
}

#[tokio::test]
#[ignore = "needs a test SSH server, see module docs"]
async fn changed_host_key_is_flagged_and_replaced_when_accepted() {
    let (Some(host), Some(password)) =
        (test_host(AuthMethod::Password), env("HT_TEST_SSH_PASSWORD"))
    else {
        panic!("set HT_TEST_SSH_PORT and HT_TEST_SSH_PASSWORD");
    };
    let known_hosts = temp_known_hosts("changed");
    let (result, _) = run_session(host.clone(), &known_hosts, true, vec![&password]).await;
    result.expect("first session failed");

    // Swap the recorded key for a different one of the same type, as a MITM would present.
    let recorded = std::fs::read_to_string(&known_hosts).unwrap();
    let (host_field, rest) = recorded.trim().split_once(' ').unwrap();
    let algorithm = rest.split(' ').next().unwrap();
    let forged = format!(
        "{host_field} {}\n",
        generate_public_key(algorithm, &known_hosts)
    );
    std::fs::write(&known_hosts, format!("# comment\n{forged}")).unwrap();

    let (result, recorded) = run_session(host, &known_hosts, true, vec![&password]).await;
    result.expect("session after accepting new key failed");
    let prompt = recorded
        .events
        .iter()
        .find(|e| e["type"] == "hostKey")
        .expect("host key prompt");
    assert_eq!(prompt["changed"], true);
    let content = std::fs::read_to_string(&known_hosts).unwrap();
    assert!(
        !content.contains(&forged),
        "forged key still trusted:\n{content}"
    );
    assert_eq!(
        content.lines().filter(|l| !l.starts_with('#')).count(),
        1,
        "{content}"
    );
}

/// A fresh public key of the given type, in `type base64` form.
fn generate_public_key(algorithm: &str, dir: &std::path::Path) -> String {
    let key_type: &[&str] = match algorithm {
        "ssh-ed25519" => &["-t", "ed25519"],
        a if a.starts_with("ecdsa-sha2-nistp") => {
            &["-t", "ecdsa", "-b", &a["ecdsa-sha2-nistp".len()..]]
        }
        _ => &["-t", "rsa", "-b", "2048"],
    };
    let path = dir.with_file_name("forged_key");
    let _ = std::fs::remove_file(&path);
    let status = std::process::Command::new("ssh-keygen")
        .args(key_type)
        .args(["-q", "-N", "", "-f"])
        .arg(&path)
        .status()
        .expect("ssh-keygen");
    assert!(status.success());
    let public = std::fs::read_to_string(path.with_extension("pub")).unwrap();
    let mut fields = public.split_whitespace();
    // ssh-keygen names RSA keys "ssh-rsa"; keep the algorithm russh recorded.
    let (_, data) = (fields.next(), fields.next().unwrap());
    format!("{algorithm} {data}")
}

/// A connector that trusts the host key and answers the password prompt by itself.
fn auto_connector(host: Host, known_hosts: &std::path::Path, password: String) -> Connector {
    let registry = Arc::new(PromptRegistry::default());
    let events = {
        let registry = registry.clone();
        Channel::new(move |body| {
            let InvokeResponseBody::Json(json) = body else {
                return Ok(());
            };
            let event: Value = serde_json::from_str(&json).unwrap();
            if let Some(id) = event["requestId"].as_u64() {
                let reply = match event["type"].as_str() {
                    Some("hostKey") => PromptReply::HostKey { accept: true },
                    _ => PromptReply::Answers {
                        answers: vec![password.clone()],
                        remember: false,
                    },
                };
                registry.reply(id, reply).unwrap();
            }
            Ok(())
        })
    };
    Connector {
        host,
        known_hosts: known_hosts.to_path_buf(),
        prompter: Prompter { events, registry },
        save_secret: Arc::new(|_, _| Ok(())),
    }
}

#[tokio::test]
#[ignore = "needs a test SSH server, see module docs"]
async fn sftp_round_trip_of_a_directory_tree() {
    use std::sync::atomic::AtomicBool;

    use super::sftp::{TransferProgress, download, remove_recursive, upload};
    use russh_sftp::client::SftpSession;

    let (Some(host), Some(password)) =
        (test_host(AuthMethod::Password), env("HT_TEST_SSH_PASSWORD"))
    else {
        panic!("set HT_TEST_SSH_PORT and HT_TEST_SSH_PASSWORD");
    };
    let known_hosts = temp_known_hosts("sftp");
    let ssh = auto_connector(host, &known_hosts, password)
        .connect()
        .await
        .expect("connect");
    let channel = ssh.channel_open_session().await.unwrap();
    channel.request_subsystem(true, "sftp").await.unwrap();
    let sftp = SftpSession::new(channel.into_stream()).await.unwrap();
    let home = sftp.canonicalize(".").await.unwrap();

    // Local tree: project/a.txt, project/sub/b.bin (1 MiB, several chunks), project/sub/deeper/c.txt
    let local = std::env::temp_dir().join(format!("ht-sftp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&local);
    let tree = local.join("project");
    std::fs::create_dir_all(tree.join("sub/deeper")).unwrap();
    std::fs::write(tree.join("a.txt"), "hola").unwrap();
    let big: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(tree.join("sub/b.bin"), &big).unwrap();
    std::fs::write(tree.join("sub/deeper/c.txt"), "deep").unwrap();

    let updates = Arc::new(Mutex::new(Vec::<TransferProgress>::new()));
    let progress = {
        let updates = updates.clone();
        Channel::new(move |body| {
            if let InvokeResponseBody::Json(json) = body {
                let value: Value = serde_json::from_str(&json).unwrap();
                updates.lock().unwrap().push(TransferProgress {
                    transferred: value["transferred"].as_u64().unwrap(),
                    total: value["total"].as_u64().unwrap(),
                    files_done: value["filesDone"].as_u64().unwrap() as usize,
                    files_total: value["filesTotal"].as_u64().unwrap() as usize,
                    current: value["current"].as_str().unwrap().into(),
                });
            }
            Ok(())
        })
    };
    let not_cancelled = AtomicBool::new(false);

    let remote_dir = format!("{home}/ht-upload");
    let _ = remove_recursive(&sftp, remote_dir.clone()).await;
    sftp.create_dir(&remote_dir).await.unwrap();
    upload(
        &sftp,
        &[tree.to_string_lossy().into_owned()],
        &remote_dir,
        &progress,
        &not_cancelled,
    )
    .await
    .expect("upload");
    let last = updates
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("progress reported");
    assert_eq!((last.files_done, last.files_total), (3, 3));
    assert_eq!(last.transferred, last.total);
    assert_eq!(last.total, 4 + big.len() as u64 + 4);
    assert_eq!(
        sftp.read(format!("{remote_dir}/project/sub/b.bin"))
            .await
            .unwrap(),
        big
    );

    // Download the tree back and compare.
    let back = local.join("back");
    std::fs::create_dir_all(&back).unwrap();
    download(
        &sftp,
        &format!("{remote_dir}/project"),
        &back,
        &progress,
        &not_cancelled,
    )
    .await
    .expect("download");
    assert_eq!(
        std::fs::read_to_string(back.join("project/a.txt")).unwrap(),
        "hola"
    );
    assert_eq!(std::fs::read(back.join("project/sub/b.bin")).unwrap(), big);
    assert_eq!(
        std::fs::read_to_string(back.join("project/sub/deeper/c.txt")).unwrap(),
        "deep"
    );

    // A cancelled transfer stops and leaves no partial file behind.
    let cancelled = AtomicBool::new(true);
    let partial = local.join("partial");
    std::fs::create_dir_all(&partial).unwrap();
    let error = download(
        &sftp,
        &format!("{remote_dir}/project/sub/b.bin"),
        &partial,
        &progress,
        &cancelled,
    )
    .await
    .expect_err("should be cancelled");
    assert!(format!("{error:#}").contains("cancelled"));
    assert!(!partial.join("b.bin").exists());

    // Rename, then delete the whole tree recursively.
    sftp.rename(
        format!("{remote_dir}/project"),
        format!("{remote_dir}/renamed"),
    )
    .await
    .unwrap();
    assert!(
        sftp.try_exists(format!("{remote_dir}/renamed/sub/deeper/c.txt"))
            .await
            .unwrap()
    );
    remove_recursive(&sftp, remote_dir.clone())
        .await
        .expect("recursive delete");
    assert!(!sftp.try_exists(&remote_dir).await.unwrap());
    let _ = std::fs::remove_dir_all(&local);
}
