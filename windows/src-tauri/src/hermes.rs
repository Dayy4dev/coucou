// Hermes Agent integration — chat through the locally installed Hermes CLI.
//
// Unlike the raw API providers, this runs `hermes chat -q --oneshot` as a child
// process, so the reply comes from the full agent: its memory, skills and tools.
// Multi-turn works via --resume on the session Hermes reports back.
//
// No API key is needed — Hermes carries its own credentials.

use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::claude::{ChatContext, ChatReply};
use crate::settings::ProviderConfig;

/// Resolved once, cached for the life of the app.
static EXE: Mutex<Option<PathBuf>> = Mutex::new(None);

fn exe_path() -> Result<PathBuf, String> {
    if let Some(p) = EXE.lock().unwrap().clone() {
        return Ok(p);
    }
    // 1. Explicit override, for non-standard installs.
    if let Ok(custom) = std::env::var("HERMES_EXE") {
        let p = PathBuf::from(custom);
        if p.is_file() {
            *EXE.lock().unwrap() = Some(p.clone());
            return Ok(p);
        }
    }
    // 2. %LOCALAPPDATA%\hermes\bin\hermes.exe (default installer layout).
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        let p = PathBuf::from(base).join(r"hermes\bin\hermes.exe");
        if p.is_file() {
            *EXE.lock().unwrap() = Some(p.clone());
            return Ok(p);
        }
    }
    // 3. PATH lookup.
    if let Ok(out) = std::process::Command::new("where").arg("hermes").output() {
        if out.status.success() {
            let first = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !first.is_empty() {
                let p = PathBuf::from(&first);
                if p.is_file() {
                    *EXE.lock().unwrap() = Some(p);
                    return Ok(p);
                }
            }
        }
    }
    Err("Hermes Agent not found. Install it from https://hermes-agent.nousresearch.com".into())
}

/// Chat state for the Hermes backend: just the Hermes session id. The full
/// history lives inside Hermes itself; we only need the handle to resume it.
#[derive(Default)]
pub struct HermesChat {
    session_id: Mutex<Option<String>>,
}

impl HermesChat {
    pub fn reset(&self) {
        *self.session_id.lock().unwrap() = None;
    }

    fn session(&self) -> Option<String> {
        self.session_id.lock().unwrap().clone()
    }

    fn set_session(&self, id: String) {
        *self.session_id.lock().unwrap() = Some(id);
    }
}

/// Pull the `session_id: <id>` line out of Hermes' output.
fn extract_session_id(output: &str) -> Option<String> {
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("session_id:") {
            let id = rest.trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// Strip the "↻ Resumed session ..." banner so it never reaches the chat bubble.
fn strip_banner(output: &str) -> String {
    output
        .lines()
        .filter(|l| !l.starts_with("↻ Resumed session") && !l.starts_with("session_id:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// One chat turn through Hermes. Context (dropped file) is described inline —
/// Hermes reads the file itself with its own tools.
pub async fn send(
    chat: &HermesChat,
    query: String,
    context: Option<ChatContext>,
    _provider: &ProviderConfig,
) -> Result<ChatReply, String> {
    let exe = exe_path()?;

    // Build the prompt. On the first turn, describe the attached context so
    // Hermes can pull the file in with its own Read tool.
    let mut prompt = String::new();
    if chat.session().is_none() {
        if let Some(ChatContext::File { name, path }) = &context {
            prompt.push_str(&format!(
                "[The user dropped a file: {name} at \"{path}\". Read it with your file tools before answering.]\n\n"
            ));
        }
        if let Some(ChatContext::Window { app_name, title, url }) = &context {
            prompt.push_str(&format!(
                "[Context — App: {app_name}, Window: {title}{}]\n\n",
                url.as_deref().map(|u| format!(", URL: {u}")).unwrap_or_default()
            ));
        }
    }
    prompt.push_str(&query);

    // Spawn hermes. CREATE_NO_WINDOW keeps a console from flashing.
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["chat", "-q", &prompt, "--oneshot", "--quiet"])
        .creation_flags(CREATE_NO_WINDOW);
    if let Some(sid) = chat.session() {
        cmd.arg("--resume").arg(sid);
    }

    let output = tauri::async_runtime::spawn_blocking(move || {
        cmd.output()
            .map_err(|e| format!("Could not run Hermes: {e}"))
    })
    .await
    .map_err(|e| format!("Hermes task failed: {e}"))??;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let out = String::from_utf8_lossy(&output.stdout);
        let detail = if err.trim().is_empty() { out.to_string() } else { err.to_string() };
        return Err(format!("Hermes error: {}", detail.trim().chars().take(300).collect::<String>()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if let Some(sid) = extract_session_id(&stdout) {
        chat.set_session(sid);
    }
    let text = strip_banner(&stdout);
    if text.is_empty() {
        return Err("Hermes returned an empty reply.".into());
    }
    Ok(ChatReply { text })
}

/// Is Hermes installed? Used by the settings UI for a live status dot.
pub fn installed() -> bool {
    exe_path().is_ok()
}

/// Which model/provider Hermes itself uses — surfaced in settings as a hint.
pub fn current_model_hint() -> Option<String> {
    let exe = exe_path().ok()?;
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new(&exe)
        .args(["config", "get", "agent.default_model"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// JSON shape the settings window reads for the Hermes status row.
pub fn status_json() -> Value {
    json!({
        "installed": installed(),
        "model": current_model_hint(),
    })
}
