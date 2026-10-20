//! Claude through the Claude Code CLI installed on this computer: it answers with the user's own
//! Claude plan and login, so no API key is needed.
//!
//! Each turn runs `claude -p` with no tools, no MCP servers, no hooks and no saved session, in a
//! directory of its own (so no project instructions are picked up). The conversation goes in on
//! stdin; the reply streams back as `stream-json`, whose `stream_event` lines carry the same
//! events as the Messages API.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use super::{AiEvent, AiSettings, AnthropicStream, ChatMessage, Effort, Role, StreamFormat};

pub const NOT_INSTALLED: &str = "Claude Code is not installed (or not found): install it from https://claude.com/claude-code, \
     sign in with `claude`, or set its path in the assistant settings";

/// Aliases the CLI resolves to the latest model of each family.
pub const MODELS: [&str; 3] = ["opus", "sonnet", "haiku"];

#[cfg(windows)]
const NAMES: [&str; 2] = ["claude.exe", "claude.cmd"];
#[cfg(not(windows))]
const NAMES: [&str; 1] = ["claude"];

fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// Where installers put the CLI. Apps started from the Dock or Start menu do not get the
/// shell's PATH, so these are searched as well.
fn usual_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home() {
        for dir in [
            ".local/bin",
            ".claude/local",
            ".npm-global/bin",
            ".bun/bin",
            ".volta/bin",
        ] {
            dirs.push(home.join(dir));
        }
    }
    if cfg!(windows) {
        if let Some(app_data) = std::env::var_os("APPDATA") {
            dirs.push(PathBuf::from(app_data).join("npm"));
        }
    } else {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    }
    dirs
}

fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    dirs.extend(usual_dirs());
    dirs
}

/// The CLI: the configured path (`~` allowed), or the first one found.
pub fn find(configured: &str) -> Option<PathBuf> {
    let configured = configured.trim();
    if !configured.is_empty() {
        let path = match configured.strip_prefix("~/") {
            Some(rest) => home()?.join(rest),
            None => PathBuf::from(configured),
        };
        return path.is_file().then_some(path);
    }
    search_dirs()
        .into_iter()
        .flat_map(|dir| NAMES.map(|name| dir.join(name)))
        .find(|path| path.is_file())
}

/// PATH for the CLI: an npm-installed `claude` is a script that needs `node` next to it.
fn child_path(claude: &Path) -> OsString {
    let mut dirs = search_dirs();
    if let Some(dir) = claude.parent() {
        dirs.insert(0, dir.to_path_buf());
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

/// The conversation as one prompt: earlier turns as a transcript, then the new question.
fn prompt(messages: &[ChatMessage]) -> String {
    let Some((last, earlier)) = messages.split_last() else {
        return String::new();
    };
    if earlier.is_empty() {
        return last.content.clone();
    }
    let mut prompt = String::from("<conversation_so_far>\n");
    for message in earlier {
        let tag = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        prompt.push_str(&format!("<{tag}>\n{}\n</{tag}>\n", message.content));
    }
    prompt.push_str("</conversation_so_far>\n\n");
    prompt.push_str(&last.content);
    prompt
}

/// What goes in on stdin: the prompt as text, or, when screenshots come along, one stream-json
/// user message holding them and the prompt (the CLI's only way to take pictures without tools).
fn stdin_payload(messages: &[ChatMessage]) -> (Vec<u8>, bool) {
    let text = prompt(messages);
    let images: Vec<Value> = messages
        .iter()
        .flat_map(|message| message.images())
        .map(|image| image.anthropic_block())
        .collect();
    if images.is_empty() {
        return (text.into_bytes(), false);
    }
    let mut content = images;
    content.push(json!({"type": "text", "text": text}));
    let line = json!({"type": "user", "message": {"role": "user", "content": content}});
    (format!("{line}\n").into_bytes(), true)
}

fn args(settings: &AiSettings, dir: &Path, prompt_file: &str, json_input: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-p",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--verbose",
        // An assistant that only talks: no tools, no MCP servers, nothing remembered.
        "--tools",
        "",
        "--strict-mcp-config",
        "--no-session-persistence",
    ]
    .map(OsString::from)
    .to_vec();
    if json_input {
        args.extend(["--input-format".into(), "stream-json".into()]);
    }
    // In files, so no shell or batch-file quoting is involved.
    args.extend([
        "--system-prompt-file".into(),
        dir.join(prompt_file).into(),
        "--settings".into(),
        dir.join("settings.json").into(),
    ]);
    let model = settings.model.trim();
    if !model.is_empty() {
        args.extend(["--model".into(), model.into()]);
    }
    let effort = match settings.effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
    };
    args.extend(["--effort".into(), effort.into()]);
    args
}

/// A working directory of its own, with the system prompt and settings files.
/// The prompt goes in a file named after its content, so conversations in different modes
/// running at once never overwrite each other's.
fn prepare_dir(system: &str) -> Result<(PathBuf, String), String> {
    let dir = std::env::temp_dir().join("hostealo-terminal-assistant");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let prompt_file = format!("system-prompt-{:016x}.md", super::hash_text(system));
    std::fs::write(dir.join(&prompt_file), system).map_err(|e| e.to_string())?;
    // The user's hooks (notifications, loggers...) have nothing to do with this assistant.
    std::fs::write(dir.join("settings.json"), r#"{"disableAllHooks": true}"#)
        .map_err(|e| e.to_string())?;
    Ok((dir, prompt_file))
}

/// Runs one turn through the CLI, sending events as they come.
pub async fn stream_chat(
    settings: &AiSettings,
    messages: &[ChatMessage],
    system: &str,
    on_event: &(dyn Fn(AiEvent) + Sync),
) -> Result<(), String> {
    let claude = find(&settings.base_url).ok_or(NOT_INSTALLED)?;
    let (dir, prompt_file) = prepare_dir(system)?;
    let (payload, json_input) = stdin_payload(messages);
    let mut child = Command::new(&claude)
        .args(args(settings, &dir, &prompt_file, json_input))
        .current_dir(&dir)
        .env("PATH", child_path(&claude))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Stopping the reply (or closing the app) ends the process.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", claude.display()))?;

    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    stdin
        .write_all(&payload)
        .await
        .map_err(|e| e.to_string())?;
    drop(stdin);
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let stderr = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    });

    let mut lines = BufReader::new(child.stdout.take().ok_or("no stdout")?).lines();
    let mut format = AnthropicStream::default();
    let mut result_error = None;
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match message["type"].as_str() {
            Some("stream_event") => {
                let (events, done) = format.event(&message["event"].to_string());
                events.into_iter().for_each(on_event);
                if let Some(stop_reason) = done {
                    on_event(AiEvent::Done { stop_reason });
                    return Ok(());
                }
            }
            Some("result") => {
                if message["is_error"].as_bool() == Some(true) {
                    result_error = Some(
                        message["result"]
                            .as_str()
                            .map(String::from)
                            .unwrap_or_else(|| message["errors"].to_string()),
                    );
                    break;
                }
                on_event(AiEvent::Done {
                    stop_reason: message["stop_reason"].as_str().map(String::from),
                });
                return Ok(());
            }
            _ => {}
        }
    }

    let status = child.wait().await.map_err(|e| e.to_string())?;
    let stderr = stderr.await.unwrap_or_default();
    Err(result_error
        .or_else(|| (!stderr.trim().is_empty()).then(|| stderr.trim().to_string()))
        .unwrap_or_else(|| format!("Claude Code ended without answering ({status})")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn chat(role: Role, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.into(),
            images: Vec::new(),
        }
    }

    #[test]
    fn screenshots_go_in_as_one_stream_json_message() {
        let (payload, json_input) = stdin_payload(&[chat(Role::User, "hola")]);
        assert!(!json_input);
        assert_eq!(payload, b"hola");

        let with_image = ChatMessage {
            images: vec![super::super::ChatImage {
                media_type: "image/jpeg".into(),
                data: "QUJD".into(),
            }],
            ..chat(Role::User, "¿qué hay?")
        };
        let (payload, json_input) = stdin_payload(&[with_image]);
        assert!(json_input);
        let line: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(line["type"], "user");
        assert_eq!(line["message"]["content"][0]["source"]["data"], "QUJD");
        assert_eq!(line["message"]["content"][1]["text"], "¿qué hay?");
    }

    #[test]
    fn earlier_turns_go_in_as_a_transcript() {
        assert_eq!(prompt(&[chat(Role::User, "hola")]), "hola");
        assert_eq!(
            prompt(&[
                chat(Role::User, "uno"),
                chat(Role::Assistant, "dos"),
                chat(Role::User, "tres")
            ]),
            "<conversation_so_far>\n<user>\nuno\n</user>\n<assistant>\ndos\n</assistant>\n</conversation_so_far>\n\ntres"
        );
    }

    #[test]
    fn a_configured_path_must_exist() {
        assert_eq!(find("/nonexistent/claude"), None);
        let exe = std::env::current_exe().unwrap();
        assert_eq!(find(exe.to_str().unwrap()), Some(exe));
    }

    /// A fake `claude` that prints what the real one prints, to test the whole run offline.
    #[cfg(unix)]
    fn fake_claude(name: &str, output: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path =
            std::env::temp_dir().join(format!("ht-fake-claude-{name}-{}", std::process::id()));
        let script = format!(
            "#!/bin/sh\ncat > \"$0.stdin\"\necho \"$@\" > \"$0.args\"\ncat <<'EOF'\n{output}\nEOF\n"
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streams_a_reply_through_the_cli() {
        let output = [
            r#"{"type":"system","subtype":"init","model":"claude-opus-5-5"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-5-5"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Usa "}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"`uptime`"}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
        ]
        .join("\n");
        let claude = fake_claude("ok", &output);
        let settings = AiSettings {
            provider: super::super::Provider::ClaudeCode,
            model: "opus".into(),
            base_url: claude.to_string_lossy().into_owned(),
            effort: Effort::Low,
        };
        let events = Mutex::new(Vec::new());
        stream_chat(
            &settings,
            &[chat(Role::User, "¿cuánto lleva encendido?")],
            "sys",
            &|event| events.lock().unwrap().push(event),
        )
        .await
        .unwrap();

        let events = events.into_inner().unwrap();
        assert_eq!(
            events[0],
            AiEvent::Model {
                model: "claude-opus-5-5".into()
            }
        );
        assert_eq!(
            events[1],
            AiEvent::Text {
                text: "Usa ".into()
            }
        );
        assert_eq!(
            events.last(),
            Some(&AiEvent::Done {
                stop_reason: Some("end_turn".into())
            })
        );
        let stdin = std::fs::read_to_string(format!("{}.stdin", claude.display())).unwrap();
        assert_eq!(stdin, "¿cuánto lleva encendido?");
        let args = std::fs::read_to_string(format!("{}.args", claude.display())).unwrap();
        assert!(args.contains("--tools  --strict-mcp-config"), "{args}");
        assert!(args.contains("--model opus --effort low"), "{args}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_the_cli_error() {
        let claude = fake_claude(
            "error",
            r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login"}"#,
        );
        let settings = AiSettings {
            provider: super::super::Provider::ClaudeCode,
            base_url: claude.to_string_lossy().into_owned(),
            ..AiSettings::default()
        };
        let error = stream_chat(&settings, &[chat(Role::User, "hola")], "sys", &|_| {})
            .await
            .unwrap_err();
        assert_eq!(error, "Not logged in · Please run /login");
    }

    /// Against the real CLI, on the user's plan (a few tokens of Haiku):
    /// HT_TEST_CLAUDE_CODE=1 cargo test -- --ignored claude_code
    #[tokio::test]
    #[ignore]
    async fn answers_through_the_installed_cli() {
        let settings = AiSettings {
            provider: super::super::Provider::ClaudeCode,
            model: "haiku".into(),
            base_url: String::new(),
            effort: Effort::Low,
        };
        let text = Mutex::new(String::new());
        stream_chat(
            &settings,
            &[chat(Role::User, "Reply with just the word: pong")],
            "Be brief.",
            &|event| {
                if let AiEvent::Text { text: piece } = event {
                    text.lock().unwrap().push_str(&piece);
                }
            },
        )
        .await
        .unwrap();
        assert!(text.into_inner().unwrap().to_lowercase().contains("pong"));
    }
}
