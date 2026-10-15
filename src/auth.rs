//! Grok CLI session token from `~/.grok/auth.json`, refreshed when it is due.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

const REFRESH_SKEW: time::Duration = time::Duration::seconds(60);

pub struct Session {
    access_token: String,
}

impl Session {
    pub fn access_token(&self) -> &str {
        &self.access_token
    }
}

pub fn load_session() -> Result<Session, String> {
    let path = auth_path();
    if !path.is_file() {
        return Err("No Grok login found. Run `grok login`.".into());
    }
    let _lock = Lock::acquire(&path)?;
    let mut doc = read_doc(&path)?;
    let entry_key = select_entry_key(&doc)
        .ok_or_else(|| "No Grok login found in ~/.grok/auth.json. Run `grok login`.".to_string())?;
    if token_is_fresh(&doc, &entry_key) {
        let token = entry_string(&doc, &entry_key, "key").unwrap_or_default();
        return Ok(Session {
            access_token: token,
        });
    }
    refresh_entry(&path, &mut doc, &entry_key)?;
    let token = entry_string(&doc, &entry_key, "key").unwrap_or_default();
    if token.is_empty() {
        return Err("Grok login expired. Run `grok login`.".into());
    }
    Ok(Session {
        access_token: token,
    })
}

/// Refresh even when the stored expiry still looks valid. Used after a 401.
pub fn force_refresh() -> Result<Session, String> {
    let path = auth_path();
    let _lock = Lock::acquire(&path)?;
    let mut doc = read_doc(&path)?;
    let entry_key = select_entry_key(&doc)
        .ok_or("No Grok login found in ~/.grok/auth.json. Run `grok login`.")?;
    refresh_entry(&path, &mut doc, &entry_key)?;
    let token = entry_string(&doc, &entry_key, "key").unwrap_or_default();
    if token.is_empty() {
        return Err("Grok login expired. Run `grok login`.".into());
    }
    Ok(Session {
        access_token: token,
    })
}

fn refresh_entry(path: &Path, doc: &mut Value, entry_key: &str) -> Result<(), String> {
    let refresh_token = entry_string(doc, entry_key, "refresh_token")
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "Grok login expired. Run `grok login`.".to_string())?;
    let client_id = entry_string(doc, entry_key, "oidc_client_id")
        .filter(|value| !value.is_empty())
        .or_else(|| client_id_from_key(entry_key))
        .ok_or_else(|| "Grok login is missing a client id. Run `grok login`.".to_string())?;
    let issuer = entry_string(doc, entry_key, "oidc_issuer")
        .filter(|value| !value.is_empty())
        .or_else(|| issuer_from_key(entry_key))
        .ok_or_else(|| "Grok login is missing an issuer. Run `grok login`.".to_string())?;

    let token_endpoint = discover_token_endpoint(&issuer)?;
    let minted = request_refresh(&token_endpoint, &client_id, &refresh_token)?;
    let expires_at = expiry_stamp(minted.expires_in, &minted.access_token)?;
    apply_refresh(
        doc,
        entry_key,
        &minted.access_token,
        minted
            .refresh_token
            .as_deref()
            .or(Some(refresh_token.as_str())),
        &expires_at,
    );
    write_doc(path, doc)?;
    Ok(())
}

pub fn apply_refresh(
    doc: &mut Value,
    entry_key: &str,
    access_token: &str,
    refresh_token: Option<&str>,
    expires_at: &str,
) {
    let Some(entry) = doc.get_mut(entry_key).and_then(Value::as_object_mut) else {
        return;
    };
    entry.insert("key".to_string(), Value::String(access_token.to_string()));
    if let Some(refresh_token) = refresh_token {
        entry.insert(
            "refresh_token".to_string(),
            Value::String(refresh_token.to_string()),
        );
    }
    entry.insert(
        "expires_at".to_string(),
        Value::String(expires_at.to_string()),
    );
}

fn token_is_fresh(doc: &Value, entry_key: &str) -> bool {
    let Some(expires_at) = entry_string(doc, entry_key, "expires_at") else {
        return false;
    };
    let Ok(when) = OffsetDateTime::parse(&expires_at, &Rfc3339) else {
        return false;
    };
    when > OffsetDateTime::now_utc() + REFRESH_SKEW
}

pub fn select_entry_key(doc: &Value) -> Option<String> {
    let object = doc.as_object()?;
    let mut best: Option<(String, i64)> = None;
    for (key, value) in object {
        let Some(entry) = value.as_object() else {
            continue;
        };
        let access = entry.get("key").and_then(Value::as_str).unwrap_or("");
        if access.is_empty() {
            continue;
        }
        let stamp = entry
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(|text| OffsetDateTime::parse(text, &Rfc3339).ok())
            .map(|when| when.unix_timestamp())
            .unwrap_or(0);
        let replace = match &best {
            Some((_, best_stamp)) => stamp >= *best_stamp,
            None => true,
        };
        if replace {
            best = Some((key.clone(), stamp));
        }
    }
    best.map(|(key, _)| key)
}

fn entry_string(doc: &Value, entry_key: &str, field: &str) -> Option<String> {
    doc.get(entry_key)?.get(field)?.as_str().map(str::to_string)
}

fn issuer_from_key(entry_key: &str) -> Option<String> {
    let (issuer, _) = entry_key.split_once("::")?;
    if issuer.starts_with("https://") {
        Some(issuer.to_string())
    } else {
        None
    }
}

fn client_id_from_key(entry_key: &str) -> Option<String> {
    let (_, client_id) = entry_key.split_once("::")?;
    if client_id.is_empty() {
        None
    } else {
        Some(client_id.to_string())
    }
}

fn discover_token_endpoint(issuer: &str) -> Result<String, String> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let response = agent(Duration::from_secs(15))
        .get(&url)
        .call()
        .map_err(|error| format!("Could not reach the Grok login service: {error}"))?;
    let doc: Value = response
        .into_json()
        .map_err(|_| "The Grok login service returned an unreadable configuration.".to_string())?;
    doc.get("token_endpoint")
        .and_then(Value::as_str)
        .filter(|endpoint| endpoint.starts_with("https://"))
        .map(str::to_string)
        .ok_or_else(|| "The Grok login service has no token endpoint.".to_string())
}

#[derive(Deserialize)]
struct MintedToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

fn request_refresh(
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<MintedToken, String> {
    let body = form_encode(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ]);
    let response = agent(Duration::from_secs(20))
        .post(token_endpoint)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .set("Accept", "application/json")
        .send_string(&body);
    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(_, response)) => {
            let _ = response.into_string();
            return Err("Grok login expired. Run `grok login`.".into());
        }
        Err(error) => {
            return Err(format!("Could not refresh the Grok login: {error}"));
        }
    };
    response
        .into_json::<MintedToken>()
        .map_err(|_| "Grok login expired. Run `grok login`.".to_string())
}

fn expiry_stamp(expires_in: Option<i64>, access_token: &str) -> Result<String, String> {
    let seconds = expires_in
        .filter(|seconds| *seconds > 0)
        .or_else(|| jwt_exp_in(access_token));
    let Some(seconds) = seconds else {
        return Ok((OffsetDateTime::now_utc() + time::Duration::minutes(5))
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string()));
    };
    let when = OffsetDateTime::now_utc() + time::Duration::seconds(seconds);
    when.to_offset(UtcOffset::UTC)
        .format(&Rfc3339)
        .map_err(|_| "Could not store the refreshed Grok login expiry.".to_string())
}

fn jwt_exp_in(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload)?;
    let doc: Value = serde_json::from_slice(&bytes).ok()?;
    let exp = doc.get("exp")?.as_i64()?;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    Some((exp - now).max(1))
}

pub fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let mut filtered = Vec::with_capacity(text.len());
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        filtered.push(value(byte)?);
    }
    let mut out = Vec::with_capacity(filtered.len() * 3 / 4);
    let mut index = 0;
    while index + 4 <= filtered.len() {
        let chunk = ((filtered[index] as u32) << 18)
            | ((filtered[index + 1] as u32) << 12)
            | ((filtered[index + 2] as u32) << 6)
            | (filtered[index + 3] as u32);
        out.push((chunk >> 16) as u8);
        out.push((chunk >> 8) as u8);
        out.push(chunk as u8);
        index += 4;
    }
    let rest = filtered.len() - index;
    if rest == 2 {
        let chunk = ((filtered[index] as u32) << 18) | ((filtered[index + 1] as u32) << 12);
        out.push((chunk >> 16) as u8);
    } else if rest == 3 {
        let chunk = ((filtered[index] as u32) << 18)
            | ((filtered[index + 1] as u32) << 12)
            | ((filtered[index + 2] as u32) << 6);
        out.push((chunk >> 16) as u8);
        out.push((chunk >> 8) as u8);
    } else if rest != 0 {
        return None;
    }
    Some(out)
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(timeout).build()
}

fn auth_path() -> PathBuf {
    grok_home().join("auth.json")
}

fn grok_home() -> PathBuf {
    if let Some(home) = std::env::var_os("GROK_HOME") {
        return PathBuf::from(home);
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".grok")
}

fn read_doc(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path)
        .map_err(|_| "Could not read the Grok login. Run `grok login`.".to_string())?;
    serde_json::from_str(&text)
        .map_err(|_| "Could not read the Grok login. Run `grok login`.".to_string())
}

fn write_doc(path: &Path, doc: &Value) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not store the refreshed Grok login.".to_string())?;
    let tmp = parent.join(format!(".auth.json.cmd-{}", std::process::id()));
    let data = serde_json::to_vec_pretty(doc)
        .map_err(|_| "Could not store the refreshed Grok login.".to_string())?;
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|_| "Could not store the refreshed Grok login.".to_string())?;
        file.write_all(&data)
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_all())
            .map_err(|_| "Could not store the refreshed Grok login.".to_string())?;
        fs::rename(&tmp, path)
            .map_err(|_| "Could not store the refreshed Grok login.".to_string())?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

struct Lock {
    file: File,
}

impl Lock {
    fn acquire(auth_path: &Path) -> Result<Self, String> {
        let lock_path = auth_path.with_file_name("auth.json.lock");
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|_| "Could not lock the Grok login file.".to_string())?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err("Could not lock the Grok login file.".into());
        }
        Ok(Self { file })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_the_latest_login() {
        let doc = json!({
            "old": {"key": "a", "expires_at": "2020-01-01T00:00:00Z"},
            "new": {"key": "b", "expires_at": "2030-01-01T00:00:00Z", "email": "kept"}
        });
        assert_eq!(select_entry_key(&doc).as_deref(), Some("new"));
    }

    #[test]
    fn refresh_keeps_unrelated_fields() {
        let mut doc = json!({
            "https://auth.x.ai::client": {
                "key": "old",
                "refresh_token": "rt",
                "email": "person@example.com",
                "auth_mode": "oidc"
            }
        });
        apply_refresh(
            &mut doc,
            "https://auth.x.ai::client",
            "new-access",
            Some("new-refresh"),
            "2030-01-01T00:00:00Z",
        );
        let entry = &doc["https://auth.x.ai::client"];
        assert_eq!(entry["key"], "new-access");
        assert_eq!(entry["refresh_token"], "new-refresh");
        assert_eq!(entry["expires_at"], "2030-01-01T00:00:00Z");
        assert_eq!(entry["email"], "person@example.com");
        assert_eq!(entry["auth_mode"], "oidc");
    }

    #[test]
    fn decodes_a_jwt_payload() {
        // {"exp": 2000000000}
        let payload = base64url_decode("eyJleHAiOjIwMDAwMDAwMDB9").unwrap();
        let doc: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(doc["exp"], 2_000_000_000_i64);
    }

    #[test]
    fn percent_encoding_leaves_unreserved_bytes() {
        assert_eq!(percent_encode("abc-._~"), "abc-._~");
        assert_eq!(percent_encode("a b"), "a%20b");
    }
}
