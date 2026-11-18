use std::borrow::Cow;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value, json};

use crate::logging::is_credential_key;
use crate::text::truncate_at_char_boundary;

const MAX_ERROR_CAPTURE_BYTES: usize = 256 * 1024;
const MAX_ERROR_CAPTURES: usize = 200;
const ERROR_CAPTURE_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

pub(super) async fn write(req_id: &str, document: Value) -> Option<PathBuf> {
    let dir = crate::paths::state_dir().join("errors");
    let req_id = req_id.to_string();
    tokio::task::spawn_blocking(move || {
        write_error_capture(&dir, &req_id, &redact_error_value(document))
    })
    .await
    .ok()
    .flatten()
}

fn write_error_capture(dir: &Path, req_id: &str, document: &Value) -> Option<PathBuf> {
    fs::create_dir_all(dir).ok()?;
    set_mode(dir, 0o700);
    let path = dir.join(format!(
        "{}-{}.json",
        super::current_millis(),
        sanitize_path_part(req_id)
    ));
    let mut file = File::create(&path).ok()?;
    set_mode(&path, 0o600);
    let payload = capped_payload(document);
    file.write_all(&payload).ok()?;
    file.write_all(b"\n").ok()?;
    prune_error_captures(dir, SystemTime::now());
    Some(path)
}

fn capped_payload(document: &Value) -> Vec<u8> {
    let payload = serde_json::to_vec_pretty(document).unwrap_or_default();
    let Some(fields) = document
        .as_object()
        .filter(|_| payload.len() > MAX_ERROR_CAPTURE_BYTES)
    else {
        return payload;
    };
    let rendered: Vec<(&String, &Value, Cow<'_, str>)> = fields
        .iter()
        .map(|(key, value)| {
            let text = value
                .as_str()
                .map_or_else(|| Cow::Owned(value.to_string()), Cow::Borrowed);
            (key, value, text)
        })
        .collect();
    let mut budget = MAX_ERROR_CAPTURE_BYTES / 4;
    loop {
        let capped: Map<String, Value> = rendered
            .iter()
            .map(|(key, value, text)| {
                let scalar =
                    !matches!(value, Value::String(_) | Value::Array(_) | Value::Object(_));
                let kept = if scalar || text.len() <= budget {
                    (*value).clone()
                } else {
                    json!({
                        "truncatedBytes": text.len(),
                        "prefix": truncate_at_char_boundary(text, budget),
                    })
                };
                ((*key).clone(), kept)
            })
            .collect();
        let payload = serde_json::to_vec_pretty(&capped).unwrap_or_default();
        if payload.len() <= MAX_ERROR_CAPTURE_BYTES || budget == 0 {
            return payload;
        }
        budget /= 2;
    }
}

fn prune_error_captures(dir: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut captures: Vec<(SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            meta.is_file().then(|| (modified, entry.path()))
        })
        .collect();
    captures.sort_unstable_by(|left, right| right.cmp(left));
    for (index, (modified, path)) in captures.into_iter().enumerate() {
        let expired = now
            .duration_since(modified)
            .is_ok_and(|age| age > ERROR_CAPTURE_RETENTION);
        if index >= MAX_ERROR_CAPTURES || expired {
            let _ = fs::remove_file(path);
        }
    }
}

fn sanitize_path_part(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn redact_error_value(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(redact_error_value).collect()),
        Value::Object(fields) => {
            let mut out = Map::new();
            for (key, value) in fields {
                if is_credential_key(&key) {
                    out.insert(key, redact_error_key(value));
                } else {
                    out.insert(key, redact_error_value(value));
                }
            }
            Value::Object(out)
        }
        value => value,
    }
}

fn redact_error_key(value: Value) -> Value {
    match value {
        Value::String(value) => Value::String(format!("[redacted len={}]", value.len())),
        _ => Value::String("[redacted]".to_string()),
    }
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(mode);
            let _ = fs::set_permissions(path, perm);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_capture_is_capped_and_prunes_old_and_excess_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("errors");
        fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();
        let stale = dir.join("stale.json");
        fs::write(&stale, b"{}").unwrap();
        File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(now - Duration::from_secs(8 * 24 * 60 * 60))
            .unwrap();
        for index in 0..MAX_ERROR_CAPTURES {
            let path = dir.join(format!("recent-{index:03}.json"));
            fs::write(&path, b"{}").unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(now - Duration::from_secs(60 + index as u64))
                .unwrap();
        }

        let huge_model = "m".repeat(2 * 1024 * 1024);
        let document = json!({
            "reqId": "req-1",
            "model": huge_model,
            "status": 400,
            "message": format!("Unknown model \"{huge_model}\""),
            "response": {"json": {"error": {"message": huge_model}}},
        });
        let path = write_error_capture(&dir, "req-1", &document).unwrap();

        let written = fs::read(&path).unwrap();
        assert!(written.len() <= MAX_ERROR_CAPTURE_BYTES + 1);
        let stored: Value = serde_json::from_slice(&written).unwrap();
        assert_eq!(stored["reqId"], "req-1");
        assert_eq!(stored["status"], 400);
        assert_eq!(
            stored["response"]["truncatedBytes"],
            json!(document["response"].to_string().len())
        );
        assert!(!stale.exists());
        assert!(
            !dir.join(format!("recent-{:03}.json", MAX_ERROR_CAPTURES - 1))
                .exists()
        );
        assert!(dir.join("recent-000.json").exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), MAX_ERROR_CAPTURES);
    }
}
