#[path = "support/hermetic.rs"]
mod hermetic;

use duolane::anthropic::{
    encode_sse_event, parse_sse_events, parse_sse_events_with_stats, schema::MessagesRequest,
};
use duolane::auth::{AuthStorage, InMemoryAuthStore};
use duolane::config::{AliasProvider, load_config_for_env};
use duolane::logging::{create_logger, redact_value};
use duolane::paths::{self, DirResolverEnv};
use duolane::retry::{RETRY_INITIAL_DELAY_MS, RETRY_MAX_DELAY_MS, compute_backoff_delay};
use duolane::traffic::{
    TrafficCaptureOptions, create_traffic_capture, redact_traffic, sanitize_path_part,
    traffic_capture_enabled_for_env,
};
use serde_json::Map;
use serde_json::json;
use std::collections::HashMap;
use std::env;
use tempfile::TempDir;

#[test]
fn messages_fixture_and_sse_parsing() {
    let raw = std::fs::read_to_string("tests/fixtures/anthropic-message.json").unwrap();
    let req: MessagesRequest = serde_json::from_str(&raw).unwrap();
    assert_eq!(req.model.as_deref(), Some("gpt-5.4[1m]"));
    assert!(req.extra.contains_key("extra_field"));

    let raw_sse = std::fs::read_to_string("tests/fixtures/sse-basic.txt").unwrap();
    let events = parse_sse_events(raw_sse.as_bytes());
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].event, None);
    assert_eq!(events[0].data, "{\"chunk\":1}");
    assert_eq!(events[1].event.as_deref(), Some("update"));
    assert_eq!(events[1].data, "{\"text\":\"line1\"}\n{\"text\":\"line2\"}");
    assert_eq!(events[2].event.as_deref(), Some("final"));
    assert_eq!(events[2].data, "{\"done\":true}");

    let (events_with_stats, stats) = parse_sse_events_with_stats(raw_sse.as_bytes());
    assert_eq!(events_with_stats.len(), events.len());
    assert!(stats.event_count >= 2);

    let encoded = encode_sse_event(Some("message"), "{\"a\":1}\n{\"b\":2}");
    assert_eq!(
        encoded,
        b"event: message\ndata: {\"a\":1}\ndata: {\"b\":2}\n\n".to_vec()
    );
    let reparsed = parse_sse_events(&encoded);
    assert_eq!(reparsed[0].event.as_deref(), Some("message"));
    assert_eq!(reparsed[0].data, "{\"a\":1}\n{\"b\":2}");
    let first_line: serde_json::Value =
        serde_json::from_str(reparsed[0].data.lines().next().unwrap()).unwrap();
    assert_eq!(first_line["a"], json!(1));
}

#[test]
fn path_resolvers_cover_platform_rules() {
    let mut env = HashMap::new();
    env.insert(
        "DUOLANE_CONFIG_DIR".to_string(),
        "/tmp/ccp-config".to_string(),
    );
    let deps = DirResolverEnv {
        platform: "macos".to_string(),
        home: "/home/u".into(),
        env: env.clone(),
    };
    assert_eq!(
        paths::resolve_config_dir(&deps).to_string_lossy(),
        "/tmp/ccp-config"
    );

    let deps = DirResolverEnv {
        platform: "macos".to_string(),
        home: "/home/u".into(),
        env: HashMap::from([("XDG_CONFIG_HOME".into(), "/x".into())]),
    };
    assert_eq!(
        paths::resolve_config_dir(&deps).to_string_lossy(),
        "/x/duolane"
    );

    let deps = DirResolverEnv {
        platform: "linux".to_string(),
        home: "/home/u".into(),
        env: HashMap::from([("XDG_CONFIG_HOME".into(), "/x".into())]),
    };
    assert_eq!(
        paths::resolve_config_dir(&deps).to_string_lossy(),
        "/x/duolane"
    );

    let deps = DirResolverEnv {
        platform: "windows".to_string(),
        home: "C:/Users/u".into(),
        env: HashMap::from([("APPDATA".into(), "C:/Users/u/AppData/Roaming".into())]),
    };
    assert_eq!(
        paths::resolve_config_dir(&deps).to_string_lossy(),
        "C:/Users/u/AppData/Roaming/duolane"
    );
}

#[test]
fn state_dir_override_wins_over_every_platform_default() {
    for platform in ["linux", "macos", "windows"] {
        let deps = DirResolverEnv {
            platform: platform.to_string(),
            home: "/home/u".into(),
            env: HashMap::from([
                ("DUOLANE_STATE_DIR".into(), "/tmp/state".into()),
                ("XDG_STATE_HOME".into(), "/x".into()),
                ("LOCALAPPDATA".into(), "/l".into()),
            ]),
        };
        assert_eq!(
            paths::resolve_state_dir(&deps).to_string_lossy(),
            "/tmp/state",
            "{platform}"
        );
    }
}

#[test]
fn config_dir_override_honors_only_the_duolane_variable() {
    let with_var = |name: String| DirResolverEnv {
        platform: "linux".to_string(),
        home: "/home/u".into(),
        env: HashMap::from([(name, "/tmp/override".to_string())]),
    };

    assert_eq!(
        paths::resolve_config_dir(&with_var("DUOLANE_CONFIG_DIR".to_string())).to_string_lossy(),
        "/tmp/override"
    );
    assert_eq!(
        paths::resolve_config_dir(&with_var(["CCP", "CONFIG_DIR"].join("_"))).to_string_lossy(),
        "/home/u/.config/duolane"
    );
}

#[test]
fn default_config_and_state_dirs_end_in_duolane() {
    for platform in ["linux", "macos", "windows"] {
        let deps = DirResolverEnv {
            platform: platform.to_string(),
            home: "/home/u".into(),
            env: HashMap::new(),
        };
        let config = paths::resolve_config_dir(&deps);
        let state = paths::resolve_state_dir(&deps);
        assert!(config.ends_with("duolane"), "{platform}: {config:?}");
        assert!(state.ends_with("duolane"), "{platform}: {state:?}");
    }
}

fn deps(platform: &str, home: &str, env: &[(&str, &str)]) -> DirResolverEnv {
    DirResolverEnv {
        platform: platform.to_string(),
        home: home.into(),
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

#[test]
fn state_dir_resolves_from_xdg_fallback_and_windows_local_app_data() {
    assert_eq!(
        paths::resolve_state_dir(&deps("linux", "/home/u", &[("XDG_STATE_HOME", "/s")])),
        std::path::Path::new("/s/duolane")
    );
    assert_eq!(
        paths::resolve_state_dir(&deps("macos", "/home/u", &[("XDG_STATE_HOME", "/s")])),
        std::path::Path::new("/s/duolane")
    );
    assert_eq!(
        paths::resolve_state_dir(&deps("linux", "/home/u", &[])),
        std::path::Path::new("/home/u/.local/state/duolane")
    );
    assert_eq!(
        paths::resolve_state_dir(&deps("windows", "C:/Users/u", &[("LOCALAPPDATA", "C:/L")])),
        std::path::Path::new("C:/L/duolane")
    );
}

#[test]
fn config_dir_resolves_from_windows_app_data() {
    assert_eq!(
        paths::resolve_config_dir(&deps("windows", "C:/Users/u", &[("APPDATA", "C:/A")])),
        std::path::Path::new("C:/A/duolane")
    );
}

#[test]
fn empty_override_variables_are_treated_as_unset() {
    assert_eq!(
        paths::resolve_config_dir(&deps(
            "linux",
            "/home/u",
            &[("DUOLANE_CONFIG_DIR", ""), ("XDG_CONFIG_HOME", "/x")]
        )),
        std::path::Path::new("/x/duolane")
    );
    assert_eq!(
        paths::resolve_state_dir(&deps(
            "linux",
            "/home/u",
            &[("DUOLANE_STATE_DIR", ""), ("XDG_STATE_HOME", "/s")]
        )),
        std::path::Path::new("/s/duolane")
    );
}

#[cfg(unix)]
#[test]
fn hermetic_root_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let root = home.parent().unwrap();
    assert!(
        root.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("duolane-hermetic-")
    );
    let mode = std::fs::metadata(root).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[test]
fn every_test_binary_includes_the_hermetic_environment() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let include = "#[path = \"support/hermetic.rs\"]";
    let mut checked = 0;
    for entry in std::fs::read_dir(root.join("tests")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\r\n", "\n");
        assert!(
            source.contains(include),
            "{} lacks {include}",
            path.display()
        );
        checked += 1;
    }
    assert!(checked > 0);
    let unit_include = "#[cfg(test)]\n#[path = \"../tests/support/hermetic.rs\"]";
    for file in ["src/lib.rs", "src/main.rs"] {
        let source = std::fs::read_to_string(root.join(file))
            .unwrap()
            .replace("\r\n", "\n");
        assert!(
            source.contains(unit_include),
            "{file} lacks the unit-test include"
        );
    }
}

#[test]
fn log_file_is_named_after_the_product() {
    assert_eq!(
        paths::log_file().file_name().and_then(|name| name.to_str()),
        Some("duolane.log")
    );
}

#[test]
fn inbound_token_header_is_redacted_in_logs() {
    let redacted = redact_value(json!({"x-duolane-key": "s3cret"}));
    assert!(
        redacted["x-duolane-key"]
            .as_str()
            .unwrap()
            .contains("redacted")
    );
}

#[test]
fn logging_redaction_and_truncation() {
    let short =
        json!({"authorization": "abc", "visible": "x", "nested": {"set-cookie": "c", "v": "y"}});
    let redacted = redact_value(short);
    assert_eq!(redacted["visible"], json!("x"));
    assert!(
        redacted["authorization"]
            .as_str()
            .unwrap()
            .contains("redacted")
    );

    let long = "a".repeat(5000);
    let long_payload = redact_value(json!({"text": long}));
    let text = long_payload["text"].as_str().unwrap();
    assert!(text.contains("[1000 more]"));
}

#[test]
fn traffic_capture_helpers() {
    let mut env = HashMap::new();
    env.insert("DUOLANE_TRAFFIC_LOG".into(), "1".into());
    assert!(traffic_capture_enabled_for_env(&env));
    assert!(!traffic_capture_enabled_for_env(&HashMap::new()));

    assert_eq!(sanitize_path_part("a*b@c/def"), "a_b_c_def");
    let path = sanitize_path_part(
        "/a\
/path",
    );
    assert!(path.chars().all(|ch| ch != '/'));

    let sample = json!({"access_token":"abc","nested":{"chatgpt-account-id":"x","value":1}});
    let redacted = redact_traffic(&sample);
    assert!(
        redacted["access_token"]
            .as_str()
            .unwrap()
            .contains("redacted")
    );
    assert_eq!(
        redact_traffic(&json!({"user_id":"person","note":"Bearer secret"}))["user_id"],
        "[redacted len=6]"
    );
    let secrets = json!({
        "access_token":"access-secret",
        "nested":[{"refresh_token":"refresh-secret","oauth_token":"oauth-secret"}],
        "identity":{"email":"person@example.test"},
        "user":{"message":"Bearer text is ordinary message content"}
    });
    let redacted = redact_traffic(&secrets).to_string();
    for secret in [
        "access-secret",
        "refresh-secret",
        "oauth-secret",
        "person@example.test",
    ] {
        assert!(!redacted.contains(secret));
    }
    assert!(redacted.contains("Bearer text is ordinary message content"));

    unsafe {
        env::set_var("DUOLANE_TRAFFIC_LOG", "1");
    }
    let temp = TempDir::new().unwrap();
    let capture = create_traffic_capture(TrafficCaptureOptions {
        req_id: "req-1".into(),
        session_id: None,
        session_seq: Some(12),
        provider: Some("codex".into()),
        state_dir_override: Some(temp.path().to_path_buf()),
    })
    .expect("capture");

    capture.write_text("020-note", "hello");
    capture.write_json("030-req", &json!({"token":"abc"}));
    let captured = std::fs::read_to_string(
        std::fs::read_dir(capture.root())
            .unwrap()
            .find_map(|entry| {
                let path = entry.ok()?.path();
                path.file_name()?
                    .to_string_lossy()
                    .ends_with("030-req.json")
                    .then_some(path)
            })
            .unwrap(),
    )
    .unwrap();
    assert!(!captured.contains("abc"));
    unsafe {
        env::remove_var("DUOLANE_TRAFFIC_LOG");
    }
}

#[test]
fn retry_backoff_decisions() {
    let outcome = compute_backoff_delay(0, None);
    assert!(outcome.wait_ms >= RETRY_INITIAL_DELAY_MS / 2);
    assert!(outcome.wait_ms <= RETRY_MAX_DELAY_MS);

    let outcome_num = compute_backoff_delay(0, Some("5"));
    assert_eq!(outcome_num.wait_ms, 5000);

    let too_long = compute_backoff_delay(0, Some("120"));
    assert!(too_long.exceeds_budget);
}

#[tokio::test]
async fn auth_store_read_write_and_logout() {
    let store: InMemoryAuthStore<serde_json::Value> = InMemoryAuthStore::new();
    assert!(store.load().unwrap().is_none());
    store.save(json!({"token": "abc"})).unwrap();
    let loaded = store.load().unwrap().expect("value");
    assert_eq!(loaded["token"], json!("abc"));
    store.clear().unwrap();
    assert!(store.load().unwrap().is_none());

    let path = tempfile::tempdir().unwrap().path().join("auth.json");
    assert!(!path.exists());
}

#[test]
fn config_env_precedence_and_defaults() {
    let config = TempDir::new().unwrap();
    let mut env = HashMap::from([(
        "DUOLANE_CONFIG_DIR".to_string(),
        config.path().to_string_lossy().into_owned(),
    )]);
    assert_eq!(load_config_for_env(&env).port, 18765);

    env.insert("PORT".to_string(), "19999".to_string());
    assert_eq!(load_config_for_env(&env).port, 19999);

    env.insert("DUOLANE_ALIAS_PROVIDER".to_string(), "codex".to_string());
    assert!(matches!(
        load_config_for_env(&env).alias_provider,
        AliasProvider::Codex
    ));
}

#[test]
fn alias_provider_has_expected_default() {
    let config = TempDir::new().unwrap();
    let env = HashMap::from([(
        "DUOLANE_CONFIG_DIR".to_string(),
        config.path().to_string_lossy().into_owned(),
    )]);
    assert!(matches!(
        load_config_for_env(&env).alias_provider,
        AliasProvider::Anthropic
    ));
}

#[test]
fn logger_factory_builds() {
    let logger = create_logger("server");
    let fields = Map::new();
    logger.debug("ready", Some(fields));
}
