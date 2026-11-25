use http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const MAX_PROFILES: usize = 64;
const MAX_FILE: u64 = 64 * 1024;
const MAX_USED: f64 = 100.0;
const FILE: &str = "claude-usage.json";
const REFRESH_SECONDS: u64 = 60;

#[derive(Clone, Debug)]
pub(crate) struct LaunchProfile(pub(crate) Arc<str>);

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Window {
    pub(crate) used: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resets_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) five_hour: Option<Window>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) seven_day: Option<Window>,
    pub(crate) observed_at: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Document {
    #[serde(default)]
    profiles: BTreeMap<String, Limits>,
}

static OBSERVED: Mutex<BTreeMap<String, Limits>> = Mutex::new(BTreeMap::new());
static FLUSH_LOCK: Mutex<()> = Mutex::new(());
static FLUSHING: AtomicBool = AtomicBool::new(false);

fn number(headers: &HeaderMap, name: &str) -> Option<f64> {
    let mut values = headers.get_all(name).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return None;
    };
    value
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite() && *number >= 0.0)
}

fn window(headers: &HeaderMap, period: &str) -> Option<Window> {
    let used = number(
        headers,
        &format!("anthropic-ratelimit-unified-{period}-utilization"),
    )
    .filter(|used| *used <= MAX_USED)?;
    let resets_at = number(
        headers,
        &format!("anthropic-ratelimit-unified-{period}-reset"),
    )
    .filter(|seconds| *seconds <= 1e11)
    .map(|seconds| seconds.round() as u64);
    Some(Window { used, resets_at })
}

fn valid_window(window: &Window) -> bool {
    window.used.is_finite() && (0.0..=MAX_USED).contains(&window.used)
}

fn valid(name: &str, limits: &Limits) -> bool {
    super::valid_profile_tag(name)
        && limits.five_hour.as_ref().is_none_or(valid_window)
        && limits.seven_day.as_ref().is_none_or(valid_window)
}

fn merge(into: &mut BTreeMap<String, Limits>, from: BTreeMap<String, Limits>) {
    for (name, limits) in from {
        if !valid(&name, &limits) {
            continue;
        }
        match into.get(&name) {
            Some(current) if current.observed_at > limits.observed_at => {}
            _ => {
                into.insert(name, limits);
            }
        }
    }
    while into.len() > MAX_PROFILES {
        let Some(oldest) = into
            .iter()
            .min_by_key(|(_, limits)| limits.observed_at)
            .map(|(name, _)| name.clone())
        else {
            break;
        };
        into.remove(&oldest);
    }
}

fn observe(
    observed: &mut BTreeMap<String, Limits>,
    profile: Option<&str>,
    headers: &HeaderMap,
    now: u64,
) -> bool {
    let name = profile.unwrap_or("default");
    let five_hour = window(headers, "5h");
    let seven_day = window(headers, "7d");
    if (five_hour.is_none() && seven_day.is_none()) || !super::valid_profile_tag(name) {
        return false;
    }
    let previous = observed.get(name).copied();
    let limits = Limits {
        five_hour: five_hour.or(previous.and_then(|limits| limits.five_hour)),
        seven_day: seven_day.or(previous.and_then(|limits| limits.seven_day)),
        observed_at: now,
    };
    let changed = previous.is_none_or(|previous| {
        previous.five_hour != limits.five_hour
            || previous.seven_day != limits.seven_day
            || now.saturating_sub(previous.observed_at) >= REFRESH_SECONDS
    });
    if changed {
        merge(observed, BTreeMap::from([(name.to_owned(), limits)]));
    }
    changed
}

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

pub(crate) fn record(profile: Option<&str>, headers: &HeaderMap) {
    let changed = OBSERVED
        .lock()
        .is_ok_and(|mut observed| observe(&mut observed, profile, headers, now()));
    if !changed || FLUSHING.swap(true, Ordering::SeqCst) {
        return;
    }
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            runtime.spawn_blocking(flush);
        }
        Err(_) => flush(),
    }
}

fn flush() {
    let Ok(_guard) = FLUSH_LOCK.lock() else {
        FLUSHING.store(false, Ordering::SeqCst);
        return;
    };
    FLUSHING.store(false, Ordering::SeqCst);
    let Ok(observed) = OBSERVED.lock().map(|observed| observed.clone()) else {
        return;
    };
    if let Err(error) = persist(observed) {
        crate::logging::create_logger("claude-usage").warn(
            "claude_usage_not_saved",
            Some(serde_json::Map::from_iter([(
                "error".to_string(),
                serde_json::Value::String(error.to_string()),
            )])),
        );
    }
}

fn decode(bytes: &[u8]) -> BTreeMap<String, Limits> {
    let mut profiles = BTreeMap::new();
    if let Ok(document) = serde_json::from_slice::<Document>(bytes) {
        merge(&mut profiles, document.profiles);
    }
    profiles
}

#[cfg(unix)]
fn persist(observed: BTreeMap<String, Limits>) -> anyhow::Result<()> {
    use anyhow::Context;
    let dir = super::store::checked_dir(&crate::paths::state_dir(), true)?
        .context("state directory unavailable")?;
    let _lock = super::store::lock(&dir, ".claude-usage-lock", true)?;
    let mut profiles = super::store::private_read(&dir, FILE, MAX_FILE)
        .ok()
        .flatten()
        .map(|bytes| decode(&bytes))
        .unwrap_or_default();
    merge(&mut profiles, observed);
    let bytes = serde_json::to_vec(&Document { profiles })?;
    super::store::private_replace(&dir, FILE, &bytes)
}

#[cfg(not(unix))]
fn persist(_: BTreeMap<String, Limits>) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn load() -> BTreeMap<String, Limits> {
    let mut profiles = super::store::checked_dir(&crate::paths::state_dir(), false)
        .ok()
        .flatten()
        .and_then(|dir| {
            super::store::private_read(&dir, FILE, MAX_FILE)
                .ok()
                .flatten()
        })
        .map(|bytes| decode(&bytes))
        .unwrap_or_default();
    if let Ok(observed) = OBSERVED.lock() {
        merge(&mut profiles, observed.clone());
    }
    profiles
}

#[cfg(not(unix))]
pub(crate) fn load() -> BTreeMap<String, Limits> {
    OBSERVED
        .lock()
        .map(|observed| observed.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    #[test]
    fn claude_limits_read_unified_headers_and_reject_hostile_values() {
        let mut observed = BTreeMap::new();
        let good = headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
            ("anthropic-ratelimit-unified-5h-reset", "1790000000"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.07"),
        ]);
        assert!(observe(&mut observed, Some("backup"), &good, 100));
        assert!(!observe(&mut observed, Some("backup"), &good, 110));
        assert!(observe(
            &mut observed,
            Some("backup"),
            &good,
            100 + REFRESH_SECONDS
        ));
        let same_second = headers(&[("anthropic-ratelimit-unified-5h-utilization", "0.8")]);
        assert!(observe(
            &mut observed,
            Some("backup"),
            &same_second,
            100 + REFRESH_SECONDS
        ));
        assert_eq!(observed["backup"].five_hour.unwrap().used, 0.8);
        assert!(observe(
            &mut observed,
            Some("backup"),
            &good,
            100 + REFRESH_SECONDS
        ));
        let backup = observed["backup"];
        assert_eq!(
            backup.five_hour,
            Some(Window {
                used: 0.42,
                resets_at: Some(1_790_000_000)
            })
        );
        assert_eq!(backup.seven_day.unwrap().resets_at, None);
        let partial = headers(&[("anthropic-ratelimit-unified-7d-utilization", "0.5")]);
        assert!(observe(&mut observed, None, &partial, 200));
        assert_eq!(observed["default"].five_hour, None);
        assert!(observe(&mut observed, Some("backup"), &partial, 300));
        assert_eq!(observed["backup"].five_hour, backup.five_hour);
        assert_eq!(observed["backup"].seven_day.unwrap().used, 0.5);
        for hostile in [
            headers(&[("anthropic-ratelimit-unified-5h-utilization", "NaN")]),
            headers(&[("anthropic-ratelimit-unified-5h-utilization", "-1")]),
            headers(&[("anthropic-ratelimit-unified-5h-utilization", "1e9")]),
            headers(&[
                ("anthropic-ratelimit-unified-5h-utilization", "0.1"),
                ("anthropic-ratelimit-unified-5h-utilization", "0.9"),
            ]),
            headers(&[("anthropic-ratelimit-unified-status", "allowed")]),
        ] {
            assert!(!observe(&mut observed, Some("other"), &hostile, 400));
        }
        assert!(!observe(&mut observed, Some("Bad/../name"), &good, 400));
        assert_eq!(observed.len(), 2);
    }

    #[test]
    fn claude_limits_keep_newest_bounded_valid_entries() {
        let entry = |at| Limits {
            five_hour: Some(Window {
                used: 0.5,
                resets_at: None,
            }),
            seven_day: None,
            observed_at: at,
        };
        let mut profiles = BTreeMap::from([("kept".to_string(), entry(1_000_000))]);
        let incoming = (0..MAX_PROFILES as u64 + 5)
            .map(|index| (format!("p{index}"), entry(index)))
            .chain([
                ("kept".to_string(), entry(5)),
                ("Bad!".to_string(), entry(9)),
            ])
            .collect();
        merge(&mut profiles, incoming);
        assert_eq!(profiles.len(), MAX_PROFILES);
        assert_eq!(profiles["kept"].observed_at, 1_000_000);
        assert!(!profiles.contains_key("p0") && !profiles.contains_key("Bad!"));
        let mut replacement = entry(1_000_000);
        replacement.five_hour.as_mut().unwrap().used = 0.8;
        merge(
            &mut profiles,
            BTreeMap::from([("kept".to_string(), replacement)]),
        );
        assert_eq!(profiles["kept"], replacement);
        let stored = serde_json::to_vec(&Document { profiles }).unwrap();
        assert_eq!(decode(&stored).len(), MAX_PROFILES);
        assert!(
            decode(br#"{"profiles":{"x":{"five_hour":{"used":1e300},"observed_at":1}}}"#)
                .is_empty()
        );
        assert!(decode(b"not json").is_empty());
    }
}
