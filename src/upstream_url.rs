pub fn check_upstream_urls() -> anyhow::Result<()> {
    check_upstream_url_settings(crate::config::upstream_url_settings())
}

fn check_upstream_url_settings(settings: Vec<(&'static str, String)>) -> anyhow::Result<()> {
    for (setting, value) in settings {
        if let Some(problem) = upstream_url_problem(&value) {
            anyhow::bail!("{setting} {problem}");
        }
    }
    Ok(())
}

pub(crate) fn upstream_url_problem(raw: &str) -> Option<&'static str> {
    let Ok(url) = url::Url::parse(raw) else {
        return Some("is not a valid absolute URL");
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Some("must not contain user information");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Some("must not contain a query or fragment");
    }
    match url.scheme() {
        "https" => None,
        "http" if is_loopback_host(&url) => None,
        _ => Some("must use https, or http only for a loopback host"),
    }
}

pub fn is_loopback_host(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.to_canonical().is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_urls_need_https_or_loopback_http_without_userinfo_query_or_fragment() {
        let cases = [
            ("https://chatgpt.com/backend-api/codex/responses", true),
            ("https://api.anthropic.com", true),
            ("http://127.0.0.1:8080/responses", true),
            ("http://127.1.2.3/", true),
            ("http://localhost:3000", true),
            ("http://LOCALHOST:3000/v1", true),
            ("http://[::1]:9000/", true),
            ("http://[::ffff:127.0.0.1]/", true),
            ("http://example.com/responses", false),
            ("http://10.0.0.1/", false),
            ("http://[::2]/", false),
            ("http://localhost.example.com/", false),
            ("http://127.0.0.1.nip.io/", false),
            ("ws://127.0.0.1/", false),
            ("wss://chatgpt.com/", false),
            ("ftp://127.0.0.1/", false),
            ("https://user:secret@chatgpt.com/", false),
            ("https://user@chatgpt.com/", false),
            ("https://chatgpt.com/responses?debug=1", false),
            ("https://chatgpt.com/responses?", false),
            ("https://chatgpt.com/responses#part", false),
            ("chatgpt.com/responses", false),
            ("", false),
        ];
        for (url, allowed) in cases {
            assert_eq!(upstream_url_problem(url).is_none(), allowed, "{url}");
        }

        let error = check_upstream_url_settings(vec![
            (
                "DUOLANE_ANTHROPIC_BASE_URL",
                "http://127.0.0.1:1".to_string(),
            ),
            (
                "codex.baseUrl",
                "https://user:secret@chatgpt.com/".to_string(),
            ),
        ])
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("codex.baseUrl "), "{error}");
        assert!(!error.contains("secret"), "{error}");
    }
}
