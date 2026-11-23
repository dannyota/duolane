use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{Json, Router, http::StatusCode, routing::get};
use serde_json::json;
use tokio::net::TcpListener;
use url::Url;

use super::{ApiClient, ApiFailureKind, SendEvidence};
use crate::providers::codex::auth::api_key::OpenAiKey;

#[test]
fn codex_api_retains_key_scoped_identity() {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::sources::CredentialId;
    use crate::providers::codex::translate::reasoning_signature::ReasoningAccount;

    let base = Url::parse("http://127.0.0.1/v1").unwrap();
    let api = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    assert!(api.credential_id() == CredentialId::new(AuthSource::ApiKey, b"toy-key"));
    assert!(api.reasoning_account() == &ReasoningAccount::for_api_key(b"toy-key"));
    assert!(api.credential_id() != CredentialId::new(AuthSource::ApiKey, b"other-key"));
}

#[tokio::test]
async fn codex_api_redacts_rejected_key_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/models", get(|| async {
        (StatusCode::UNAUTHORIZED, Json(json!({"error":{"message":"private upstream text","type":"invalid_request_error","param":null,"code":"invalid_api_key"}})))
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let failure = match client.verify().await {
        Ok(()) => panic!("expected failure"),
        Err(failure) => failure,
    };
    assert_eq!(failure.kind, ApiFailureKind::Rejected);
    assert_eq!(failure.send, SendEvidence::DeniedBeforeExecution);
    assert!(!failure.error.message.contains("private upstream text"));
    assert!(!failure.error.message.contains("invalid_api_key"));
    server.abort();
}

#[tokio::test]
async fn codex_api_catalog_retains_only_successful_list() {
    let count = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let counter = count.clone();
    let app = Router::new().route(
        "/v1/models",
        get(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Json(json!({"data":[{"id":"gpt-5.5"},{"id":"gpt-99.1-sol"},{"id":"unrelated"}]}))
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    assert!(client.catalog().is_none());
    tokio::join!(client.refresh_models(), client.refresh_models());
    let catalog = client.catalog().unwrap();
    assert!(catalog.iter().any(|id| id == "gpt-5.5"));
    assert!(catalog.iter().any(|id| id == "gpt-99.1-sol"));
    assert!(!catalog.iter().any(|id| id == "unrelated"));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn codex_api_redacts_http_and_stream_failures() {
    use super::ApiFailureKind;
    use axum::{
        body::Body,
        http::{HeaderMap, Response},
        routing::post,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!(
        "http://{}/prefix/v1",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let app = Router::new().route("/prefix/v1/responses", post(move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
        let seen = seen.clone();
        async move {
            assert!(headers.get("authorization").is_some_and(|v| v.as_bytes() == b"Bearer toy-key"));
            for name in ["chatgpt-account-id", "originator", "session_id", "window_id"] {
                assert!(!headers.contains_key(name));
            }
            seen.fetch_add(1, Ordering::SeqCst);
            let (status, data) = match body["case"].as_str().unwrap() {
                "redirect" => (302, ""),
                "typed_quota" => (400, r#"{"error":{"type":"insufficient_quota"}}"#),
                "quota" => (429, r#"{"error":{"code":"credit_balance_exhausted","message":"secret upstream message"}}"#),
                "bad_error" => (429, "malformed private upstream text"),
                "rate" => (429, r#"{"error":{"code":"rate_limit_exceeded","message":"secret upstream message"}}"#),
                "server" => (503, r#"{"error":{"type":"insufficient_quota","message":"secret upstream message"}}"#),
                "context" => (400, r#"{"error":{"code":"context_length_exceeded"}}"#),
                "stream" => (200, "data: {\"type\":\"response.failed\",\"response\":{\"output\":[{\"type\":\"web_search_call\"}],\"error\":{\"type\":\"insufficient_quota\",\"message\":\"secret upstream message\"}}}\n\n"),
                "failed_status" => (200, "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"failed\"}}\n\n"),
                "incomplete" => (200, "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"secret upstream message\"}}}\n\n"),
                "executed_quota" => (200, "data: {\"type\":\"response.failed\",\"response\":{\"output\":[{\"type\":\"message\"}],\"error\":{\"type\":\"insufficient_quota\"}}}\n\n"),
                "stream_server" => (200, "data: {\"type\":\"error\",\"status\":503,\"error\":{\"type\":\"insufficient_quota\"}}\n\n"),
                "stream_credit" => (200, "data: {\"type\":\"error\",\"status\":429,\"error\":{\"code\":\"credit_balance_exhausted\"}}\n\n"),
                "empty_error" => (200, "event: error\ndata: [DONE]\n\ndata: {\"type\":\"response.completed\"}\n\n"),
                "malformed" => (200, "data: not-json\n\n"),
                "missing_type" => (200, "data: {\"message\":\"secret upstream message\"}\n\n"),
                _ => unreachable!(),
            };
            Response::builder().status(status).header("location", "/leak").header("x-private", "secret upstream header")
                .header("retry-after", "3").body(Body::from(data)).unwrap()
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    for (case, kind) in [
        ("redirect", ApiFailureKind::Other),
        ("quota", ApiFailureKind::Quota),
        ("typed_quota", ApiFailureKind::Invalid),
        ("rate", ApiFailureKind::Transient),
        ("bad_error", ApiFailureKind::Transient),
        ("server", ApiFailureKind::Transient),
        ("context", ApiFailureKind::ContextOverflow),
    ] {
        let failure = match client.responses(&json!({"case":case})).await {
            Err(failure) => failure,
            Ok(_) => panic!("expected failure"),
        };
        assert_eq!(failure.kind, kind);
        assert!(!format!("{:?}", failure).contains("secret upstream"));
        if case == "rate" {
            assert_eq!(failure.error.status, 429);
            assert_eq!(failure.error.retry_after.as_deref(), Some("3"));
        }
        if case == "server" || case == "rate" {
            assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
        }
    }
    for case in [
        "stream",
        "malformed",
        "missing_type",
        "failed_status",
        "incomplete",
        "empty_error",
        "stream_server",
        "executed_quota",
        "stream_credit",
    ] {
        let mut stream = match client.responses(&json!({"case":case})).await {
            Ok(stream) => stream,
            Err(_) => panic!("expected stream"),
        };
        let failure = match stream.next().await {
            Some(Err(failure)) => failure,
            _ => panic!("expected stream failure"),
        };
        assert!(!format!("{:?}", failure).contains("secret upstream"));
        if case == "executed_quota" {
            assert_eq!(failure.kind, ApiFailureKind::Quota);
            assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
        }
        if case == "stream_server" {
            assert_eq!(failure.kind, ApiFailureKind::Transient);
            assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
        }
        if case == "stream_credit" {
            assert_eq!(failure.kind, ApiFailureKind::Quota);
        }
        if case == "stream" {
            assert_eq!(failure.kind, ApiFailureKind::Quota);
            assert!(failure.hosted_executed);
            assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 16);
    server.abort();
}

#[tokio::test]
async fn codex_api_catalog_retains_failed_refresh_then_accepts_empty() {
    use std::time::Duration;
    let count = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let counter = count.clone();
    let app = Router::new().route(
        "/v1/models",
        get(move || {
            let counter = counter.clone();
            async move {
                match counter.fetch_add(1, Ordering::SeqCst) {
                    0 => (StatusCode::OK, Json(json!({"data":[{"id":"gpt-5.5"}]}))),
                    1 => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error":{"message":"hidden"}})),
                    ),
                    _ => (StatusCode::OK, Json(json!({"data":[]}))),
                }
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    client.refresh_models().await;
    *client.catalog.attempt.lock().await =
        Some(tokio::time::Instant::now() - Duration::from_secs(3600));
    client.refresh_models().await;
    assert_eq!(client.catalog().unwrap().len(), 1);
    client.refresh_models().await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    *client.catalog.attempt.lock().await =
        Some(tokio::time::Instant::now() - Duration::from_secs(3600));
    client.refresh_models().await;
    assert!(client.catalog().unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn codex_api_redacts_verification_bounds_redirect_and_timeout() {
    use axum::{
        body::Body,
        http::{HeaderMap, Response},
    };
    use std::time::Duration;
    let count = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!(
        "http://{}/nested/v1",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let counter = count.clone();
    let app = Router::new().route(
        "/nested/v1/models",
        get(move |headers: HeaderMap| {
            let counter = counter.clone();
            async move {
                assert!(
                    headers
                        .get("authorization")
                        .is_some_and(|value| value.as_bytes() == b"Bearer toy-key")
                );
                assert!(!headers.contains_key("chatgpt-account-id"));
                assert!(!headers.contains_key("originator"));
                match counter.fetch_add(1, Ordering::SeqCst) {
                    0 => Response::builder()
                        .status(200)
                        .body(Body::from("{}"))
                        .unwrap(),
                    1 => Response::builder()
                        .status(302)
                        .header("location", "/leak")
                        .body(Body::empty())
                        .unwrap(),
                    2 => Response::builder().status(201).body(Body::empty()).unwrap(),
                    3 => Response::builder()
                        .status(200)
                        .body(Body::from(vec![b'x'; 1024 * 1024 + 1]))
                        .unwrap(),
                    _ => {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        Response::builder().status(200).body(Body::empty()).unwrap()
                    }
                }
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut client = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    assert!(client.verify().await.is_ok());
    for _ in 0..3 {
        assert!(client.verify().await.is_err());
    }
    client.header_timeout = Duration::from_millis(10);
    let failure = match client.verify().await {
        Err(failure) => failure,
        Ok(()) => panic!("expected timeout"),
    };
    assert_eq!(failure.kind, ApiFailureKind::HeaderTimeout);
    assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
    assert_eq!(count.load(Ordering::SeqCst), 5);
    server.abort();
}

#[tokio::test]
async fn codex_api_redacts_stream_truncation_and_keeps_hosted_evidence() {
    use axum::{body::Body, http::Response, routing::post};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(|Json(body): Json<serde_json::Value>| async move {
        let data = match body["case"].as_str().unwrap() {
            "success" => "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
            "truncated" => "data: {\"type\":\"response.created\",\"response\":{}}\n\n",
            _ => "data: {\"type\":\"response.web_search_call.completed\"}\n\ndata: broken\n\n",
        };
        Response::builder().status(200).body(Body::from(data)).unwrap()
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    for case in ["success", "truncated", "hosted"] {
        let mut stream = match client.responses(&json!({"case":case})).await {
            Ok(stream) => stream,
            Err(_) => panic!("expected stream"),
        };
        assert!(matches!(stream.next().await, Some(Ok(_))));
        if case == "success" {
            assert!(stream.next().await.is_none());
        } else {
            let failure = match stream.next().await {
                Some(Err(failure)) => failure,
                _ => panic!("expected failure"),
            };
            assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
            assert_eq!(failure.hosted_executed, case == "hosted");
        }
    }
    server.abort();
}

#[tokio::test]
async fn codex_api_redacts_oversized_error_and_body_read_failure() {
    use axum::{body::Body, http::Response, routing::post};
    use bytes::Bytes;
    use futures_util::{StreamExt, stream};
    use std::{io, time::Duration};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route(
        "/v1/responses",
        post(|Json(body): Json<serde_json::Value>| async move {
            if body["case"] == "large" {
                Response::builder()
                    .status(429)
                    .body(Body::from(vec![b'x'; 1024 * 1024 + 1]))
                    .unwrap()
            } else {
                let first = stream::once(async {
                    Ok::<_, io::Error>(Bytes::from_static(
                        b"data: {\"type\":\"response.web_search_call.completed\"}\n\n",
                    ))
                });
                let second = stream::once(async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Err::<Bytes, _>(io::Error::other("private upstream error"))
                });
                let chunks = first.chain(second);
                Response::builder()
                    .status(200)
                    .body(Body::from_stream(chunks))
                    .unwrap()
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    assert!(client.authorization.is_sensitive());
    let failure = match client.responses(&json!({"case":"large"})).await {
        Err(failure) => failure,
        Ok(_) => panic!("expected failure"),
    };
    assert_eq!(failure.kind, ApiFailureKind::Transient);
    let mut stream = match client.responses(&json!({"case":"read"})).await {
        Ok(stream) => stream,
        Err(_) => panic!("expected stream"),
    };
    assert!(matches!(stream.next().await, Some(Ok(_))));
    let failure = match stream.next().await {
        Some(Err(failure)) => failure,
        _ => panic!("expected body failure"),
    };
    assert!(failure.hosted_executed);
    assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
    assert!(!format!("{:?}", failure).contains("private upstream error"));
    server.abort();
}

#[tokio::test]
async fn codex_api_redacts_oversized_sse_frame() {
    use axum::{body::Body, http::Response, routing::post};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route(
        "/v1/responses",
        post(|| async {
            Response::builder()
                .status(200)
                .body(Body::from(vec![
                    b'x';
                    crate::providers::codex::client::MAX_UPSTREAM_FRAME_BYTES
                        + 1
                ]))
                .unwrap()
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let mut events = match client.responses(&json!({})).await {
        Ok(stream) => stream,
        Err(_) => panic!("expected stream"),
    };
    let failure = match events.next().await {
        Some(Err(failure)) => failure,
        _ => panic!("expected failure"),
    };
    assert_eq!(failure.kind, ApiFailureKind::Other);
    assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
    server.abort();
}

#[test]
fn codex_api_redacts_quota_requires_http_denial() {
    use super::ApiFailure;
    let value = json!({"error":{"type":"insufficient_quota"}});
    for status in [302, 408, 500] {
        let failure = ApiFailure::upstream(status, Some(&value), false, false);
        assert_ne!(failure.kind, ApiFailureKind::Quota);
        assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
    }
    for status in [401, 429] {
        let failure = ApiFailure::upstream(status, Some(&value), false, false);
        assert_eq!(failure.send, SendEvidence::DeniedBeforeExecution);
    }
    let stream = ApiFailure::upstream(429, Some(&value), false, true);
    assert_eq!(stream.kind, ApiFailureKind::Quota);
    assert_eq!(stream.send, SendEvidence::OutcomeUnknown);
}

#[tokio::test]
async fn codex_api_redacts_first_stream_quota_is_ambiguous() {
    use axum::{body::Body, routing::post};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(|| async {
        Body::from("data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"insufficient_quota\"}}}\n\n")
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let mut events = match client.responses(&json!({})).await {
        Ok(events) => events,
        Err(_) => panic!("expected stream"),
    };
    let failure = match events.next().await {
        Some(Err(failure)) => failure,
        _ => panic!("expected failure"),
    };
    assert_eq!(failure.kind, ApiFailureKind::Quota);
    assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
    server.abort();
}

#[tokio::test]
async fn codex_api_redacts_unknown_events_after_output() {
    use axum::{body::Body, routing::post};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(|| async {
        Body::from("data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata: {\"type\":\"invalid_type\",\"message\":\"toy upstream sentinel\"}\n\n")
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let mut events = match client.responses(&json!({})).await {
        Ok(events) => events,
        Err(_) => panic!("expected stream"),
    };
    assert!(matches!(events.next().await, Some(Ok(_))));
    let failure = match events.next().await {
        Some(Err(failure)) => failure,
        _ => panic!("expected safe failure"),
    };
    assert!(!format!("{failure:?}").contains("toy upstream sentinel"));
    server.abort();
}

#[tokio::test]
async fn codex_api_redacts_incomplete_stream_as_transient() {
    use axum::{body::Body, routing::post};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route(
        "/v1/responses",
        post(|Json(body): Json<serde_json::Value>| async move {
            match body["partial"].as_bool().unwrap() {
                true => Body::from("data: {\"type\":\"response.output_text.delta\""),
                false => Body::from("data: {\"type\":\"response.created\",\"response\":{}}\n\n"),
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    for partial in [false, true] {
        let mut events = match client.responses(&json!({"partial":partial})).await {
            Ok(events) => events,
            Err(_) => panic!("expected stream"),
        };
        if !partial {
            assert!(matches!(events.next().await, Some(Ok(_))));
        }
        let failure = match events.next().await {
            Some(Err(failure)) => failure,
            _ => panic!("expected failure"),
        };
        assert_eq!(failure.kind, ApiFailureKind::Transient);
        assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
    }
    server.abort();
}

#[tokio::test]
async fn codex_api_catalog_rejects_malformed_entries_without_losing_snapshot() {
    use std::time::Duration;
    let count = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let counter = count.clone();
    let app = Router::new().route(
        "/v1/models",
        get(move || {
            let counter = counter.clone();
            async move {
                match counter.fetch_add(1, Ordering::SeqCst) {
                    0 => Json(json!({"data":[{"id":"gpt-5.5"}]})),
                    1 => Json(json!({"data":[{"id":"gpt-99.1-sol"},{"id":123}]})),
                    _ => Json(json!({"data":[]})),
                }
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    client.refresh_models().await;
    for attempt in 1..=2 {
        *client.catalog.attempt.lock().await =
            Some(tokio::time::Instant::now() - Duration::from_secs(3600));
        client.refresh_models().await;
        let snapshot = client.catalog().unwrap();
        if attempt == 1 {
            assert!(snapshot.iter().any(|id| id == "gpt-5.5"));
        } else {
            assert!(snapshot.is_empty());
        }
    }
    server.abort();
}

#[tokio::test]
async fn codex_api_redacts_unknown_extensions_and_preserves_known_events() {
    use axum::{body::Body, routing::post};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(|Json(body): Json<serde_json::Value>| async move {
        if body["malformed"] == true {
            Body::from("data: {\"type\":\"response.output_text.delta\",\"delta\":{\"message\":\"toy secret\"}}\n\n")
        } else {
            Body::from(concat!(
                "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"reasoning\"}}\n\n",
                "data: {\"type\":\"response.reasoning_summary_part.added\",\"part\":{\"type\":\"summary_text\"}}\n\n",
                "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"thinking\"}\n\n",
                "data: {\"type\":\"response.web_search_call.completed\"}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
                "data: {\"type\":\"future.extension\",\"message\":\"toy secret\"}\n\n",
                "data: {\"type\":\"response.output_text.annotation.added\",\"annotation\":{\"type\":\"url_citation\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
            ))
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let mut events = match client.responses(&json!({"malformed":false})).await {
        Ok(events) => events,
        Err(_) => panic!("expected stream"),
    };
    for kind in [
        "response.output_item.added",
        "response.reasoning_summary_part.added",
        "response.reasoning_summary_text.delta",
        "response.web_search_call.completed",
        "response.output_text.delta",
        "response.output_text.annotation.added",
        "response.completed",
    ] {
        let payload = match events.next().await {
            Some(Ok(payload)) => payload,
            _ => panic!("expected event"),
        };
        assert!(payload.get("type").and_then(serde_json::Value::as_str) == Some(kind));
    }
    assert!(events.next().await.is_none());
    let mut events = match client.responses(&json!({"malformed":true})).await {
        Ok(events) => events,
        Err(_) => panic!("expected stream"),
    };
    let failure = match events.next().await {
        Some(Err(failure)) => failure,
        _ => panic!("expected failure"),
    };
    assert!(!format!("{failure:?}").contains("toy secret"));
    server.abort();
}

#[tokio::test]
async fn codex_api_redacts_known_events_and_response_metadata() {
    use axum::{body::Body, routing::post};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(|Json(body): Json<serde_json::Value>| async move {
        if body["malformed"] == true {
            Body::from(concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"valid\"}\n\n",
                "data: {\"type\":\"response.created\",\"message\":\"toy sentinel\"}\n\n",
            ))
        } else {
            Body::from(concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"valid\",\"message\":\"toy sentinel\"}\n\n",
                "data: {\"type\":\"response.completed\",\"message\":\"toy sentinel\",\"response\":{\"id\":\"resp_test\",\"model\":\"gpt-5.5\",\"created_at\":123,\"status\":\"completed\",\"message\":\"toy sentinel\",\"metadata\":{\"private\":\"toy sentinel\"},\"usage\":{\"input_tokens\":12,\"output_tokens\":3,\"message\":\"toy sentinel\",\"input_tokens_details\":{\"cached_tokens\":2,\"message\":\"toy sentinel\"}},\"output\":[{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"encrypted\",\"detail\":\"toy sentinel\"},{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"action\":{\"type\":\"search\",\"query\":\"query\",\"sources\":[{\"type\":\"url\",\"url\":\"https://example.test\",\"message\":\"toy sentinel\"}],\"message\":\"toy sentinel\"}},{\"type\":\"invented\",\"message\":\"toy sentinel\"}]}}\n\n"
            ))
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    for malformed in [true, false] {
        let mut stream = match client.responses(&json!({"malformed":malformed})).await {
            Ok(events) => events,
            Err(_) => panic!("expected stream"),
        };
        let first = match stream.next().await {
            Some(Ok(value)) => value,
            _ => panic!("expected delta"),
        };
        assert!(first.get("message").is_none());
        assert!(first.get("delta").and_then(serde_json::Value::as_str) == Some("valid"));
        if malformed {
            assert!(matches!(stream.next().await, Some(Err(_)) | None));
        } else {
            let value = match stream.next().await {
                Some(Ok(value)) => value,
                _ => panic!("expected completion"),
            };
            assert!(!value.to_string().contains("toy sentinel"));
            assert!(
                value
                    .pointer("/response/id")
                    .and_then(serde_json::Value::as_str)
                    == Some("resp_test")
            );
            assert!(
                value
                    .pointer("/response/usage/input_tokens_details/cached_tokens")
                    .and_then(serde_json::Value::as_u64)
                    == Some(2)
            );
            assert!(
                value
                    .pointer("/response/output/0/encrypted_content")
                    .and_then(serde_json::Value::as_str)
                    == Some("encrypted")
            );
            assert!(
                value
                    .pointer("/response/output/1/action/sources/0/url")
                    .and_then(serde_json::Value::as_str)
                    == Some("https://example.test")
            );
            assert!(value.pointer("/response/output/2").is_none());
        }
    }
    server.abort();
}
