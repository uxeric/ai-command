//! One completion against the Grok CLI account.

use std::time::Duration;

use serde_json::{json, Value};

use crate::auth::{self, Session};

pub fn complete(system: &str, user: &str) -> Result<String, String> {
    let model = model_name();
    let effort = effort_name();
    let mut session = auth::load_session()?;
    match send(&model, &effort, system, user, &session, true) {
        Ok(text) => Ok(text),
        Err(CallError::Unauthorized) => {
            session = auth::force_refresh()?;
            send(&model, &effort, system, user, &session, true).map_err(CallError::into_message)
        }
        Err(error) => Err(error.into_message()),
    }
}

enum CallError {
    Unauthorized,
    Message(String),
}

impl CallError {
    fn into_message(self) -> String {
        match self {
            Self::Unauthorized => "Grok login expired. Run `grok login`.".into(),
            Self::Message(message) => message,
        }
    }
}

fn send(
    model: &str,
    effort: &str,
    system: &str,
    user_text: &str,
    session: &Session,
    allow_simpler_retry: bool,
) -> Result<String, CallError> {
    let full = json!({
        "model": model,
        "store": false,
        "temperature": 0.2,
        "max_output_tokens": 1600,
        "reasoning": {"effort": effort},
        "input": [
            {"role": "system", "content": system},
            {"role": "user", "content": user_text}
        ]
    });
    match post(&full, model, session) {
        Ok(text) => Ok(text),
        Err(CallError::Message(message))
            if allow_simpler_retry && message.starts_with("Grok rejected the request") =>
        {
            let simpler = json!({
                "model": model,
                "stream": true,
                "max_output_tokens": 1600,
                "input": [
                    {"role": "system", "content": system},
                    {"role": "user", "content": user_text}
                ]
            });
            post(&simpler, model, session)
        }
        Err(error) => Err(error),
    }
}

fn post(body: &Value, model: &str, session: &Session) -> Result<String, CallError> {
    let url = format!("{}/responses", proxy_base());
    let version = client_version();
    let response = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(90))
        .build()
        .post(&url)
        .set(
            "Authorization",
            &format!("Bearer {}", session.access_token()),
        )
        .set("Content-Type", "application/json")
        .set("Accept", "application/json, text/event-stream")
        .set("X-XAI-Token-Auth", "xai-grok-cli")
        .set("x-grok-client-version", &version)
        .set("x-grok-client-identifier", "grok-shell")
        .set("x-authenticateresponse", "authenticate-response")
        .set("x-grok-model-override", model)
        .set("User-Agent", &format!("xai-grok-workspace/{version}"))
        .send_json(body);

    let (status, text) = match response {
        Ok(response) => {
            let status = response.status();
            let text = response.into_string().unwrap_or_default();
            (status, text)
        }
        Err(ureq::Error::Status(status, response)) => {
            let text = response.into_string().unwrap_or_default();
            (status, text)
        }
        Err(error) => {
            return Err(CallError::Message(format!("Could not reach Grok: {error}")));
        }
    };

    if status == 401 {
        return Err(CallError::Unauthorized);
    }
    if !(200..300).contains(&status) {
        return Err(CallError::Message(http_error(status, &text)));
    }
    let extracted = extract_any(&text);
    if extracted.trim().is_empty() {
        return Err(CallError::Message("Grok returned an empty reply.".into()));
    }
    Ok(extracted)
}

fn http_error(status: u16, body: &str) -> String {
    let detail = error_detail(body);
    if status == 400 || status == 422 {
        return format!("Grok rejected the request ({status}). {detail}");
    }
    if status == 426 {
        return format!("Grok refused this client version. {detail}");
    }
    format!("Grok could not answer ({status}). {detail}")
}

fn error_detail(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "No details came back.".into();
    }
    if let Ok(doc) = serde_json::from_str::<Value>(trimmed) {
        if let Some(message) = error_message(&doc) {
            return clip(&message, 400);
        }
    }
    clip(trimmed, 400)
}

fn error_message(doc: &Value) -> Option<String> {
    match doc.get("error")? {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => map
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

fn clip(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out.replace('\n', " ")
}

fn extract_any(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.starts_with('{') {
        if let Ok(doc) = serde_json::from_str::<Value>(trimmed) {
            if let Some(text) = extract_text(&doc) {
                return text;
            }
        }
    }
    parse_sse(trimmed)
}

fn parse_sse(body: &str) -> String {
    let mut deltas = String::new();
    let mut full = String::new();
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(doc) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if let Some(delta) = doc.get("delta").and_then(Value::as_str) {
            deltas.push_str(delta);
        }
        if let Some(text) = extract_text(&doc) {
            full = text;
        }
        if let Some(response) = doc.get("response") {
            if let Some(text) = extract_text(response) {
                full = text;
            }
        }
    }
    if !full.trim().is_empty() {
        full
    } else {
        deltas
    }
}

fn extract_text(doc: &Value) -> Option<String> {
    if let Some(text) = doc.get("output_text").and_then(Value::as_str) {
        if !text.trim().is_empty() {
            return Some(text.to_string());
        }
    }
    let output = doc.get("output")?.as_array()?;
    let mut parts = Vec::new();
    for item in output {
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            continue;
        }
        let Some(content) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in content {
            let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
            if kind == "output_text" || kind == "text" {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    parts.push(text);
                }
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.concat())
    }
}

fn proxy_base() -> String {
    std::env::var("GROK_CLI_CHAT_PROXY_BASE_URL")
        .unwrap_or_else(|_| "https://cli-chat-proxy.grok.com/v1".to_string())
        .trim_end_matches('/')
        .to_string()
}

fn model_name() -> String {
    if let Ok(model) = std::env::var("CMD_MODEL") {
        let model = model.trim();
        if !model.is_empty() {
            return model.to_string();
        }
    }
    model_from_config().unwrap_or_else(|| "grok-4.7".to_string())
}

fn effort_name() -> String {
    std::env::var("CMD_EFFORT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "low".to_string())
}

fn model_from_config() -> Option<String> {
    let path = grok_home().join("config.toml");
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_models = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_models = trimmed == "[models]";
            continue;
        }
        if !in_models {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("default") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        return unquote(rest.trim());
    }
    None
}

fn unquote(text: &str) -> Option<String> {
    let text = text.trim().trim_end_matches(',');
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        return Some(text[1..text.len() - 1].to_string());
    }
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn grok_home() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("GROK_HOME") {
        return std::path::PathBuf::from(home);
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    std::path::PathBuf::from(home).join(".grok")
}

pub fn client_version() -> String {
    if let Ok(version) = std::env::var("CMD_GROK_VERSION") {
        let version = version.trim();
        if !version.is_empty() {
            return version.to_string();
        }
    }
    discover_client_version().unwrap_or_else(|| "1.0.50".to_string())
}

fn discover_client_version() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let nodes = std::path::PathBuf::from(home).join(".local/share/mise/installs/node");
    let mut best: Option<String> = None;
    let entries = std::fs::read_dir(nodes).ok()?;
    for entry in entries.flatten() {
        let package = entry
            .path()
            .join("lib/node_modules/@xai-official/grok/package.json");
        let Some(version) = package_version(&package) else {
            continue;
        };
        let replace = match &best {
            Some(current) => version_cmp(&version, current) == std::cmp::Ordering::Greater,
            None => true,
        };
        if replace {
            best = Some(version);
        }
    }
    best
}

fn package_version(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    let version = doc.get("version")?.as_str()?.trim();
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

fn version_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let parse = |text: &str| {
        text.split('.')
            .map(|part| {
                part.chars()
                    .take_while(|ch| ch.is_ascii_digit())
                    .collect::<String>()
                    .parse::<u64>()
                    .unwrap_or(0)
            })
            .collect::<Vec<_>>()
    };
    parse(left).cmp(&parse(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_output_text_and_skips_reasoning() {
        let doc = json!({
            "output": [
                {"type": "reasoning", "content": [{"type": "output_text", "text": "skip"}]},
                {"type": "message", "content": [{"type": "output_text", "text": "{\"options\":[]}"}]}
            ]
        });
        assert_eq!(extract_text(&doc).as_deref(), Some("{\"options\":[]}"));
    }

    #[test]
    fn prefers_the_completed_sse_response() {
        let body = "\
data: {\"delta\":\"{\\\"options\\\"\"}\n\
data: {\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"{\\\"options\\\":[]}\"}]}]}}\n";
        assert_eq!(parse_sse(body), "{\"options\":[]}");
    }

    #[test]
    fn reads_the_default_model_from_a_toml_snippet() {
        let dir = std::env::temp_dir().join(format!("cmd-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".grok")).unwrap();
        std::fs::write(
            dir.join(".grok/config.toml"),
            "[cli]\ninstaller = \"npm\"\n\n[models]\ndefault = \"grok-4.7\"\n",
        )
        .unwrap();
        let previous = std::env::var_os("GROK_HOME");
        std::env::set_var("GROK_HOME", dir.join(".grok"));
        assert_eq!(model_from_config().as_deref(), Some("grok-4.7"));
        match previous {
            Some(value) => std::env::set_var("GROK_HOME", value),
            None => std::env::remove_var("GROK_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn orders_dotted_versions() {
        assert!(version_cmp("1.0.50", "1.0.49") > std::cmp::Ordering::Equal);
        assert_eq!(version_cmp("1.0.50", "1.0.50"), std::cmp::Ordering::Equal);
    }
}
