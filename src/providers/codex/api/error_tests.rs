use super::{ApiClient, ApiFailureKind, SendEvidence};
use crate::providers::codex::auth::api_key::OpenAiKey;
use axum::{Json, Router, http::StatusCode};
use serde_json::json;
use tokio::net::TcpListener;
use url::Url;

#[tokio::test]
async fn codex_api_unread_denial_has_unknown_outcome() {
    use axum::{body::Body, http::Response, routing::post};
    use bytes::Bytes;
    use futures_util::{StreamExt, stream};
    use std::{io, time::Duration};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route(
        "/v1/responses",
        post(|Json(body): Json<serde_json::Value>| async move {
            let response = Response::builder().status(StatusCode::UNAUTHORIZED);
            match body["case"].as_str().unwrap() {
                "complete" => response.body(Body::from(r#"{"error":{}}"#)).unwrap(),
                _ => {
                    let first =
                        stream::once(async { Ok::<_, io::Error>(Bytes::from_static(b"{")) });
                    let second = stream::once(async {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Err::<Bytes, _>(io::Error::other("truncated"))
                    });
                    response
                        .body(Body::from_stream(first.chain(second)))
                        .unwrap()
                }
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    for (case, evidence) in [
        ("truncated", SendEvidence::OutcomeUnknown),
        ("complete", SendEvidence::DeniedBeforeExecution),
    ] {
        let failure = match client.responses(&json!({"case":case})).await {
            Err(failure) => failure,
            Ok(_) => panic!("expected failure"),
        };
        assert_eq!(failure.kind, ApiFailureKind::Rejected);
        assert_eq!(failure.send, evidence);
    }
    server.abort();
}

#[tokio::test]
async fn background_catalog_refresh_does_not_queue_tasks() {
    use axum::routing::get;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let app = Router::new().route(
        "/v1/models",
        get({
            let (calls, entered, release) = (calls.clone(), entered.clone(), release.clone());
            move || {
                let (calls, entered, release) = (calls.clone(), entered.clone(), release.clone());
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    entered.notify_one();
                    release.notified().await;
                    Json(json!({"data":[]}))
                }
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Arc::new(ApiClient::new(base, OpenAiKey::parse(b"toy-key").unwrap()).unwrap());
    assert!(client.refresh_models_if_due_in_background());
    entered.notified().await;
    for _ in 0..100 {
        assert!(!client.refresh_models_if_due_in_background());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.notify_one();
    server.abort();
}
