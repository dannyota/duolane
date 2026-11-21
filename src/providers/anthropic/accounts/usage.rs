use jiff::{Timestamp, Zoned};
use reqwest::header::ACCEPT;
use serde::Deserialize;
use serde_json::Value;
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod login;

#[cfg(any(not(target_os = "macos"), test))]
pub(super) use login::FileReader;
#[cfg(any(target_os = "macos", test))]
pub(super) use login::KeychainReader;

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const BETA: &str = "oauth-2025-04-20";
const BODY_LIMIT: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reason {
    LoginNotFound,
    LoginUnreadable,
    LoginExpired,
    #[cfg(any(target_os = "macos", test))]
    KeychainDenied,
    #[cfg(any(target_os = "macos", test))]
    KeychainTimedOut,
    RequestFailed,
    HttpStatus(u16),
    ResponseInvalid,
}

impl fmt::Display for Reason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoginNotFound => formatter.write_str("login not found"),
            Self::LoginUnreadable => formatter.write_str("login unreadable"),
            Self::LoginExpired => {
                formatter.write_str("login expired; open Claude Code with this account")
            }
            #[cfg(any(target_os = "macos", test))]
            Self::KeychainDenied => formatter.write_str("Keychain access denied"),
            #[cfg(any(target_os = "macos", test))]
            Self::KeychainTimedOut => formatter.write_str("Keychain timed out"),
            Self::RequestFailed => formatter.write_str("usage request failed"),
            Self::HttpStatus(status) => write!(formatter, "usage request failed: HTTP {status}"),
            Self::ResponseInvalid => formatter.write_str("usage response invalid"),
        }
    }
}

pub(super) struct Token(String);

impl Token {
    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Debug)]
pub(super) struct Login {
    pub(super) token: Token,
    pub(super) expires_at: u64,
}

pub(super) trait LoginReader: Send + Sync {
    fn read(&self, configured: &Path, dir: &File) -> Result<Login, Reason>;
}

#[derive(Deserialize)]
struct Stored {
    #[serde(rename = "claudeAiOauth")]
    oauth: StoredOauth,
}

#[derive(Deserialize)]
struct StoredOauth {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "expiresAt")]
    expires_at: u64,
}

pub(super) fn parse_login(bytes: &[u8]) -> Result<Login, Reason> {
    let stored: Stored = serde_json::from_slice(bytes).map_err(|_| Reason::LoginUnreadable)?;
    Ok(Login {
        token: Token(stored.oauth.access_token),
        expires_at: stored.oauth.expires_at,
    })
}

#[derive(Debug, Default, PartialEq)]
pub(super) struct Window {
    utilization: Option<f64>,
    resets_at: Option<Timestamp>,
}

#[derive(Debug, Default, PartialEq)]
pub(super) struct Usage {
    five_hour: Option<Window>,
    seven_day: Option<Window>,
}

fn window(value: Option<Value>) -> Option<Window> {
    let Value::Object(fields) = value? else {
        return None;
    };
    Some(Window {
        utilization: fields
            .get("utilization")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite()),
        resets_at: fields
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(|value| value.parse().ok()),
    })
}

fn parse_usage(bytes: &[u8]) -> Result<Usage, Reason> {
    let Ok(Value::Object(mut fields)) = serde_json::from_slice(bytes) else {
        return Err(Reason::ResponseInvalid);
    };
    Ok(Usage {
        five_hour: window(fields.remove("five_hour")),
        seven_day: window(fields.remove("seven_day")),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Https,
    All,
}

fn normalize_proxy(raw: &str) -> Option<url::Url> {
    let candidate = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let parsed = url::Url::parse(&candidate).ok()?;
    if !matches!(
        parsed.scheme(),
        "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
    ) || parsed.host_str().is_none()
        || matches!(parsed.scheme(), "socks4" | "socks4a")
            && (!parsed.username().is_empty() || parsed.password().is_some())
    {
        return None;
    }
    Some(parsed)
}

fn reaches_intercept_listener(url: &url::Url, intercept_port: u16) -> bool {
    let local = crate::upstream_url::is_loopback_host(url)
        || match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_unspecified(),
            Some(url::Host::Ipv6(ip)) => ip.to_canonical().is_unspecified(),
            _ => false,
        };
    local && url.port_or_known_default() == Some(intercept_port)
}

pub(super) fn usage_proxies(
    env: &impl Fn(&str) -> Option<String>,
    intercept_port: u16,
) -> Result<Vec<(Kind, String)>, Reason> {
    let mut proxies = Vec::new();
    for (kind, upper, lower) in [
        (Kind::Https, "HTTPS_PROXY", "https_proxy"),
        (Kind::All, "ALL_PROXY", "all_proxy"),
    ] {
        let Some(raw) = env(upper).or_else(|| env(lower)) else {
            continue;
        };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let url = normalize_proxy(raw).ok_or(Reason::RequestFailed)?;
        if !reaches_intercept_listener(&url, intercept_port) {
            proxies.push((kind, url.to_string()));
        }
    }
    Ok(proxies)
}

pub(super) fn build_client(proxies: Vec<(Kind, String)>) -> Result<reqwest::Client, Reason> {
    let mut builder = crate::tls::client_builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    let no_proxy = reqwest::NoProxy::from_env();
    for (kind, url) in proxies {
        let proxy = match kind {
            Kind::Https => reqwest::Proxy::https(&url),
            Kind::All => reqwest::Proxy::all(&url),
        }
        .map_err(|_| Reason::RequestFailed)?;
        builder = builder.proxy(proxy.no_proxy(no_proxy.clone()));
    }
    builder.build().map_err(|_| Reason::RequestFailed)
}

pub(super) fn production_usage_endpoint(_env: &impl Fn(&str) -> Option<String>) -> &'static str {
    ENDPOINT
}

#[cfg(target_os = "macos")]
fn system_reader() -> Arc<dyn LoginReader> {
    Arc::new(KeychainReader::system())
}

#[cfg(not(target_os = "macos"))]
fn system_reader() -> Arc<dyn LoginReader> {
    Arc::new(FileReader)
}

async fn fetch(client: &reqwest::Client, endpoint: &str, token: &Token) -> Result<Usage, Reason> {
    let mut response = client
        .get(endpoint)
        .bearer_auth(token.expose())
        .header("anthropic-beta", BETA)
        .header(ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| Reason::RequestFailed)?;
    let status = response.status();
    if !status.is_success() {
        return Err(Reason::HttpStatus(status.as_u16()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Reason::RequestFailed)? {
        if body.len() + chunk.len() > BODY_LIMIT {
            return Err(Reason::ResponseInvalid);
        }
        body.extend_from_slice(&chunk);
    }
    parse_usage(&body)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

pub(super) struct Target {
    pub(super) name: String,
    pub(super) location: Result<(PathBuf, File), Reason>,
}

pub(super) struct Row {
    pub(super) name: String,
    pub(super) outcome: Result<Usage, Reason>,
}

pub(super) struct Query {
    pub(super) reader: Arc<dyn LoginReader>,
    pub(super) client: Result<reqwest::Client, Reason>,
    pub(super) endpoint: String,
}

impl Query {
    pub(super) fn production() -> Self {
        let env = |name: &str| std::env::var(name).ok();
        Self {
            reader: system_reader(),
            client: usage_proxies(&env, crate::intercept::port()).and_then(build_client),
            endpoint: production_usage_endpoint(&env).to_string(),
        }
    }

    pub(super) async fn collect(&self, targets: Vec<Target>) -> Vec<Row> {
        futures_util::future::join_all(targets.into_iter().map(|target| async move {
            Row {
                outcome: self.account(target.location).await,
                name: target.name,
            }
        }))
        .await
    }

    async fn account(&self, location: Result<(PathBuf, File), Reason>) -> Result<Usage, Reason> {
        let (configured, dir) = location?;
        let reader = Arc::clone(&self.reader);
        let login = tokio::task::spawn_blocking(move || reader.read(&configured, &dir))
            .await
            .map_err(|_| Reason::LoginUnreadable)??;
        if login.expires_at <= now_millis() {
            return Err(Reason::LoginExpired);
        }
        let client = self.client.as_ref().map_err(|reason| *reason)?;
        fetch(client, &self.endpoint, &login.token).await
    }
}

fn format_reset(reset: Timestamp, now: &Zoned) -> String {
    let local = reset.to_zoned(now.time_zone().clone());
    let pattern = if local.date() == now.date() {
        "%H:%M"
    } else {
        "%b %d %H:%M"
    };
    local.strftime(pattern).to_string()
}

fn window_text(label: &str, window: Option<&Window>, now: &Zoned) -> String {
    let Some(window) = window else {
        return format!("{label} {:>4}", "?");
    };
    let used = window.utilization.map_or_else(
        || "?".to_string(),
        |value| format!("{}%", value.round() as i64),
    );
    let reset = window
        .resets_at
        .map_or_else(|| "?".to_string(), |reset| format_reset(reset, now));
    format!("{label} {used:>4} (resets {reset})")
}

pub(super) fn render(rows: &[Row], selected: Option<&str>, now: &Zoned) -> Vec<String> {
    let width = rows.iter().map(|row| row.name.len()).max().unwrap_or(0);
    rows.iter()
        .map(|row| {
            let marker = if selected == Some(row.name.as_str()) {
                '*'
            } else {
                ' '
            };
            let detail = match &row.outcome {
                Ok(usage) => format!(
                    "{}   {}",
                    window_text("5h", usage.five_hour.as_ref(), now),
                    window_text("week", usage.seven_day.as_ref(), now)
                ),
                Err(reason) => reason.to_string(),
            };
            format!("{marker} {:<width$}   {detail}", row.name)
        })
        .collect()
}

#[cfg(test)]
mod tests;
