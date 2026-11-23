#[cfg(test)]
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::provider::RequestContext;
use crate::retry::{compute_backoff_delay, sleep};

use super::auth::token_store::StoredAuth;
use super::client::*;
use super::client_body::CodexHttpBody;
use super::client_capture::*;
use super::search::{SearchRequest, SearchResponse};

impl CodexHttpClient {
    pub(crate) async fn post_transcription(
        &self,
        base_url: &str,
        input: &super::transcription::PreparedTranscription,
        ctx: &RequestContext,
    ) -> Result<reqwest::Response, CodexError> {
        let url = format!("{}/transcribe", base_url.trim_end_matches('/'));
        let mut auth = self
            .auth_manager
            .get_auth()
            .await
            .map_err(|error| CodexError {
                status: 401,
                message: "Auth error".to_string(),
                detail: Some(error.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Auth,
            })?;
        let mut refresh_attempted = false;

        loop {
            let headers = build_codex_transcription_headers(&auth, ctx)?;
            let response = self.attempt_transcription(&url, &headers, input).await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !refresh_attempted {
                refresh_attempted = true;
                ctx.recovery_budget.start();
                drop(response);
                auth = self
                    .auth_manager
                    .force_refresh(&auth.access)
                    .await
                    .map_err(auth_refresh_error)?;
                if !ctx
                    .recovery_budget
                    .try_retry(std::time::Duration::ZERO, false)
                {
                    return Err(CodexError::recovery_exhausted());
                }
                continue;
            }
            return Ok(response);
        }
    }

    async fn attempt_transcription(
        &self,
        url: &str,
        headers: &http::HeaderMap,
        input: &super::transcription::PreparedTranscription,
    ) -> Result<reqwest::Response, CodexError> {
        let part = reqwest::multipart::Part::bytes(input.audio.to_vec())
            .file_name(input.filename.clone())
            .mime_str(&input.content_type)
            .map_err(|error| CodexError {
                status: 400,
                message: "Invalid audio content type".to_string(),
                detail: Some(error.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            })?;
        let mut form = reqwest::multipart::Form::new().part("file", part);
        if let Some(language) = input.language.as_deref() {
            form = form.text("language", language.to_string());
        }
        let mut request = self.native_client.post(url).multipart(form);
        for (key, value) in headers {
            request = request.header(key.as_str(), value.as_bytes());
        }
        tokio::time::timeout(
            Duration::from_millis(IMAGE_HEADER_TIMEOUT_MS),
            request.send(),
        )
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: format!(
                "Timed out waiting {}ms for Codex transcription response headers",
                IMAGE_HEADER_TIMEOUT_MS
            ),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        })?
        .map_err(|error| CodexError {
            status: 0,
            message: format!("Codex transcription transport error: {error}"),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        })
    }

    pub(crate) async fn post_image_json(
        &self,
        base_url: &str,
        operation: super::images::ImageOperation,
        body: &serde_json::Value,
        ctx: &RequestContext,
    ) -> Result<reqwest::Response, CodexError> {
        let body_json = serde_json::to_vec(body).map_err(|error| CodexError {
            status: 500,
            message: "Failed to serialize image request".to_string(),
            detail: Some(error.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        })?;
        let url = format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            operation.upstream_path()
        );
        let mut auth = self
            .auth_manager
            .get_auth()
            .await
            .map_err(|error| CodexError {
                status: 401,
                message: "Auth error".to_string(),
                detail: Some(error.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Auth,
            })?;
        let mut refresh_attempted = false;

        loop {
            let headers = build_codex_image_headers(&auth, ctx)?;
            let response = self
                .attempt_image_json(&url, &headers, body_json.clone())
                .await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !refresh_attempted {
                refresh_attempted = true;
                ctx.recovery_budget.start();
                drop(response);
                auth = self
                    .auth_manager
                    .force_refresh(&auth.access)
                    .await
                    .map_err(auth_refresh_error)?;
                if !ctx.recovery_budget.try_retry(Duration::ZERO, false) {
                    return Err(CodexError::recovery_exhausted());
                }
                continue;
            }
            return Ok(response);
        }
    }

    async fn attempt_image_json(
        &self,
        url: &str,
        headers: &http::HeaderMap,
        body_json: Vec<u8>,
    ) -> Result<reqwest::Response, CodexError> {
        let mut request = self.native_client.post(url);
        for (key, value) in headers {
            request = request.header(key.as_str(), value.as_bytes());
        }
        tokio::time::timeout(
            Duration::from_millis(IMAGE_HEADER_TIMEOUT_MS),
            request.body(body_json).send(),
        )
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: format!(
                "Timed out waiting {}ms for Codex image response headers",
                IMAGE_HEADER_TIMEOUT_MS
            ),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        })?
        .map_err(|error| CodexError {
            status: 0,
            message: format!("Codex image transport error: {error}"),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        })
    }

    pub(crate) async fn post_native_responses(
        &self,
        body: &serde_json::Value,
        ctx: &RequestContext,
        use_responses_lite: bool,
        stream: bool,
    ) -> Result<reqwest::Response, CodexError> {
        let http_body = self
            .prepare_http_body(
                body,
                ctx.traffic.is_some(),
                "Failed to serialize native Responses request",
            )
            .await?;
        let mut auth = self
            .auth_manager
            .get_auth()
            .await
            .map_err(|err| CodexError {
                status: 401,
                message: "Auth error".to_string(),
                detail: Some(err.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Auth,
            })?;
        let mut refresh_attempted = false;

        loop {
            let started_at = Instant::now();
            let headers = build_native_codex_headers(&auth, ctx, use_responses_lite, stream)?;
            if let Some(traffic) = ctx.traffic.as_deref() {
                http_body.capture(traffic, &self.base_url, &headers);
            }

            let response = self.attempt_native_responses(&headers, &http_body).await?;
            let status = response.status().as_u16();
            if status == 401 && !refresh_attempted {
                refresh_attempted = true;
                ctx.recovery_budget.start();
                drop(response);
                auth = self.refreshed_auth(&auth).await.inspect_err(|error| {
                    if error.detail.as_deref() == Some(CODEX_ACCOUNT_CHANGED_DETAIL) {
                        ctx.recovery_budget.cancel_unspent_start();
                    }
                })?;
                if !ctx.recovery_budget.try_retry(Duration::ZERO, false) {
                    return Err(CodexError::recovery_exhausted());
                }
                continue;
            }

            if let Some(traffic) = ctx.traffic.as_deref() {
                write_live_upstream_response_headers(traffic, &response, started_at.elapsed());
            }
            return Ok(response);
        }
    }

    async fn attempt_native_responses(
        &self,
        headers: &http::HeaderMap,
        http_body: &CodexHttpBody,
    ) -> Result<reqwest::Response, CodexError> {
        let mut request = self.native_client.post(&self.base_url);
        for (key, value) in headers {
            request = request.header(key.as_str(), value.as_bytes());
        }

        tokio::time::timeout(
            Duration::from_millis(self.header_timeout_ms),
            http_body.apply(request).send(),
        )
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: format!(
                "Timed out waiting {}ms for Codex response headers",
                self.header_timeout_ms
            ),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        })?
        .map_err(|err| CodexError {
            status: 0,
            message: format!("Native Responses transport error: {err}"),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        })
    }

    pub(crate) async fn post_search(
        &self,
        body: &SearchRequest,
        ctx: &RequestContext,
    ) -> Result<SearchResponse, CodexError> {
        ctx.recovery_budget.begin_http_attempt();
        let mut auth = self.auth_manager.get_auth().await.map_err(|e| {
            trace_auth_failure(&ctx.recovery_budget, &e);
            CodexError {
                status: 401,
                message: "Auth error".to_string(),
                detail: Some(e.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Auth,
            }
        })?;
        ctx.recovery_budget
            .set_credential(super::sources::CredentialId::for_chatgpt_auth(&auth));
        let http_body = self
            .prepare_http_body(
                body,
                ctx.traffic.is_some(),
                "Failed to serialize search request",
            )
            .await?;
        let mut auth_refresh_attempted = false;
        let mut retries = 0_u32;

        loop {
            ctx.recovery_budget
                .set_kind(super::sources::FailureKind::Final);
            let response = self
                .attempt_post_search(&auth, &http_body, ctx, &body.id)
                .await?;
            if ctx.recovery_budget.is_tracked() {
                if !(200..300).contains(&response.status)
                    && serde_json::from_slice::<SearchResponse>(&response.body).is_ok()
                {
                    ctx.recovery_budget.mark_hosted();
                }
                if response.status >= 500 {
                    ctx.recovery_budget.mark_ambiguous_hosted();
                }
                let permanent = super::client::explicit_permanent_buffered_error(&response.body);
                if permanent.is_some()
                    || super::client::codex_status_error_message(&response.body)
                        .is_some_and(|message| super::events::is_context_overflow(None, &message))
                {
                    ctx.recovery_budget.mark_final_only();
                    let mut error = codex_status_error(response);
                    error.usage_limit = None;
                    if let Some(failure) = permanent.filter(|failure| failure.context_window) {
                        error.detail =
                            Some(format!("{} (context_length_exceeded)", failure.message));
                    }
                    return Err(error);
                }
                if super::events::usage_limit_from_response(&response.body, &response.headers)
                    .is_some()
                {
                    ctx.recovery_budget
                        .set_kind(super::sources::FailureKind::UsageLimit);
                    if super::recovery::RecoveryBudget::quota_denied_before_execution(
                        response.status,
                        &response.body,
                        false,
                    ) {
                        ctx.recovery_budget
                            .set_send(super::api::SendEvidence::DeniedBeforeExecution);
                    } else {
                        ctx.recovery_budget.mark_ambiguous_hosted();
                    }
                    return Err(codex_status_error(response));
                }
                if ctx.recovery_budget.hosted() {
                    return Err(codex_status_error(response));
                }
                if response.status == 429 {
                    ctx.recovery_budget.mark_ambiguous_hosted();
                }
            }
            if response.status == 401 && !auth_refresh_attempted {
                auth_refresh_attempted = true;
                ctx.recovery_budget.start();
                auth = self
                    .auth_manager
                    .force_refresh(&auth.access)
                    .await
                    .map_err(|error| {
                        trace_auth_failure(&ctx.recovery_budget, &error);
                        auth_refresh_error(error)
                    })?;
                ctx.recovery_budget
                    .set_credential(super::sources::CredentialId::for_chatgpt_auth(&auth));
                if !ctx.recovery_budget.try_retry(Duration::ZERO, false) {
                    return Err(CodexError::recovery_exhausted());
                }
                continue;
            }
            if should_retry_codex_status(response.status)
                && retries < MAX_BUFFERED_TRANSPORT_RETRIES
            {
                let retry_after = response
                    .headers
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
                    .map(|(_, value)| value.as_str());
                let delay = compute_backoff_delay(retries, retry_after);
                if delay.exceeds_budget {
                    return Err(codex_status_error(response));
                }
                if !ctx
                    .recovery_budget
                    .try_retry(std::time::Duration::from_millis(delay.wait_ms), false)
                {
                    return Err(CodexError::recovery_exhausted());
                }
                retries += 1;
                sleep(delay.wait_ms).await;
                if ctx.recovery_budget.expired() {
                    return Err(CodexError::recovery_exhausted());
                }
                continue;
            }
            if should_retry_codex_status(response.status)
                && retries >= MAX_BUFFERED_TRANSPORT_RETRIES
            {
                return Err(CodexError::recovery_exhausted());
            }
            if !(200..300).contains(&response.status) {
                return Err(codex_status_error(response));
            }
            let result = serde_json::from_slice(&response.body).map_err(|e| CodexError {
                status: 502,
                message: "Failed to decode Codex search response".to_string(),
                detail: Some(e.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            });
            if result.is_ok() {
                ctx.recovery_budget.mark_hosted();
            } else {
                ctx.recovery_budget.mark_ambiguous_hosted();
            }
            return result;
        }
    }

    async fn attempt_post_search(
        &self,
        auth: &StoredAuth,
        http_body: &CodexHttpBody,
        ctx: &RequestContext,
        owner: &str,
    ) -> Result<CodexResponse, CodexError> {
        let url = search_endpoint(&self.base_url);
        let headers = build_codex_search_headers(auth, ctx, owner)?;

        if let Some(traffic) = ctx.traffic.as_deref() {
            http_body.capture(traffic, &url, &headers);
        }

        let mut request = self.client.post(&url);
        for (key, value) in headers.iter() {
            request = request.header(key.as_str(), value.as_bytes());
        }
        let started_at = Instant::now();
        ctx.recovery_budget
            .set_send(super::api::SendEvidence::OutcomeUnknown);
        let mut response = tokio::time::timeout(
            Duration::from_millis(self.header_timeout_ms),
            http_body.apply(request).send(),
        )
        .await
        .map_err(|_| {
            ctx.recovery_budget.mark_ambiguous_hosted();
            CodexError {
                status: 0,
                message: format!(
                    "Timed out waiting {}ms for Codex search response headers",
                    self.header_timeout_ms
                ),
                detail: None,
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            }
        })?
        .map_err(|e| {
            if e.is_connect() {
                ctx.recovery_budget
                    .set_send(super::api::SendEvidence::NotSent);
            } else {
                ctx.recovery_budget.mark_ambiguous_hosted();
            }
            CodexError {
                status: 0,
                message: format!("Codex search network error: {e}"),
                detail: None,
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            }
        })?;

        let status = response.status().as_u16();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(key, value)| {
                (
                    key.to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let body = self
            .read_buffered_body(
                &mut response,
                ctx,
                "Codex search response body",
                &mut false,
                &mut false,
            )
            .await
            .inspect_err(|_| ctx.recovery_budget.mark_ambiguous_hosted())?;

        if let Some(traffic) = ctx.traffic.as_deref() {
            write_upstream_response_capture(traffic, status, started_at.elapsed(), &headers, &body);
        }

        Ok(CodexResponse {
            body,
            status,
            headers,
            transport: ActualTransport::Http,
        })
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::super::client_test_support::*;
    use super::*;

    #[tokio::test]
    async fn tracked_search_quota_and_ambiguous_retry_have_distinct_replay_evidence() {
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        use crate::config::codex_auth::AuthSource;
        for first_status in [None, Some(503), Some(429)] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                for index in 0..=usize::from(first_status.is_some()) {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let _ = read_http_request(&mut stream).await;
                    if index == 0
                        && let Some(status) = first_status
                    {
                        let response = format!(
                            "HTTP/1.1 {status} Error\r\ncontent-length: 0\r\nretry-after: 0\r\nconnection: close\r\n\r\n"
                        );
                        stream.write_all(response.as_bytes()).await.unwrap();
                    } else {
                        let body =
                            br#"{"error":{"type":"usage_limit_reached","resets_at":2000000000}}"#;
                        let headers = format!(
                            "HTTP/1.1 429 Too Many Requests\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                            body.len()
                        );
                        stream.write_all(headers.as_bytes()).await.unwrap();
                        stream.write_all(body).await.unwrap();
                    }
                }
            });
            let client = authenticated_http_test_client(format!("http://{addr}/responses"));
            let gate = ReplayGate::new(true);
            let attempt =
                SourceAttempt::new(AuthSource::ChatGpt, &http_test_context(), gate.clone());
            let request = super::super::search::SearchRequest {
                id: "session".into(),
                model: "model".into(),
                reasoning: None,
                input: None,
                commands: super::super::search::SearchCommands {
                    search_query: vec![super::super::search::SearchQuery { q: "query".into() }],
                },
                settings: super::super::search::SearchSettings {
                    filters: None,
                    allowed_callers: vec!["direct"],
                    external_web_access: true,
                },
                max_output_tokens: 2500,
            };
            let error = client
                .post_search(&request, &attempt.context)
                .await
                .unwrap_err();
            let failure = attempt.failure(error);
            assert!(failure.kind == FailureKind::UsageLimit);
            assert!(
                failure.credential
                    == Some(super::super::sources::CredentialId::for_chatgpt_auth(
                        &http_test_auth()
                    ))
            );
            assert_eq!(
                failure.error.usage_limit.as_ref().unwrap().resets_at,
                Some(2_000_000_000)
            );
            assert_eq!(
                gate.may_switch(failure.kind, failure.send),
                first_status.is_none()
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn search_refresh_and_failures_share_four_attempt_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let server_client = client.clone();
        let server = tokio::spawn(async move {
            for attempt in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _ = read_http_request(&mut stream).await;
                let response = if attempt == 0 {
                    let mut auth = http_test_auth();
                    auth.access = "rotated-test".into();
                    server_client.auth_manager().set_test_auth(auth);
                    b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .as_slice()
                } else {
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nretry-after: 0\r\nconnection: close\r\n\r\n".as_slice()
                };
                stream.write_all(response).await.unwrap();
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(200), listener.accept())
                    .await
                    .is_err()
            );
        });
        let request = super::super::search::SearchRequest {
            id: "session".into(),
            model: "gpt-5.6-luna".into(),
            reasoning: None,
            input: None,
            commands: super::super::search::SearchCommands {
                search_query: vec![super::super::search::SearchQuery { q: "query".into() }],
            },
            settings: super::super::search::SearchSettings {
                filters: None,
                allowed_callers: vec!["direct"],
                external_web_access: true,
            },
            max_output_tokens: 2500,
        };
        let error = client
            .post_search(&request, &http_test_context())
            .await
            .unwrap_err();
        assert!(error.is_recovery_exhausted());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn image_request_refreshes_once_after_unauthorized() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let server_client = client.clone();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut stream).await;
                let request = String::from_utf8_lossy(&request);
                if attempt == 0 {
                    assert!(request.contains("authorization: Bearer test"));
                    server_client.auth_manager().set_test_auth(StoredAuth {
                        access: "rotated".into(),
                        refresh: "rotated-refresh".into(),
                        account_id: Some("acct-rotated".into()),
                        expires: u64::MAX,
                    });
                    stream
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                } else {
                    assert!(request.contains("authorization: Bearer rotated"));
                    assert!(request.contains("chatgpt-account-id: acct-rotated"));
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                        )
                        .await
                        .unwrap();
                }
            }
        });

        let response = client
            .post_image_json(
                &format!("http://{addr}"),
                super::super::images::ImageOperation::Generation,
                &serde_json::json!({"model":"gpt-image-2","prompt":"draw"}),
                &http_test_context(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn image_request_does_not_retry_server_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_http_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        });

        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let response = client
            .post_image_json(
                &format!("http://{addr}"),
                super::super::images::ImageOperation::Generation,
                &serde_json::json!({"model":"gpt-image-2","prompt":"draw"}),
                &http_test_context(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn image_request_uses_fixed_path_oauth_and_json_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            let header_end = request
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap();
            let headers = String::from_utf8_lossy(&request[..header_end]);
            assert!(headers.starts_with("POST /root/images/generations HTTP/1.1"));
            assert!(headers.contains("authorization: Bearer test"));
            assert!(headers.contains("chatgpt-account-id: acct"));
            let body: serde_json::Value =
                serde_json::from_slice(&request[header_end + 4..]).unwrap();
            assert_eq!(body["model"], "gpt-image-2");
            assert_eq!(body["prompt"], "draw a fox");
            let response = br#"{"created":1,"data":[{"b64_json":"aW1n"}]}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(response).await.unwrap();
        });

        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let response = client
            .post_image_json(
                &format!("http://{addr}/root"),
                super::super::images::ImageOperation::Generation,
                &serde_json::json!({"model":"gpt-image-2","prompt":"draw a fox"}),
                &http_test_context(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_responses_replaces_auth_and_preserves_json_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let payload = serde_json::json!({
            "model": "gpt-5.4",
            "input": "hello",
            "stream": false,
            "extra": {"kept": true}
        });
        let expected = serde_json::to_vec(&payload).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert_wire_json(&request, &expected, false);
            let request = String::from_utf8_lossy(&request);
            assert!(request.contains("authorization: Bearer test"));
            assert!(request.contains("chatgpt-account-id: acct"));
            assert!(request.contains("accept: application/json"));
            let body = br#"{"id":"resp_native","object":"response"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });

        let client = authenticated_http_test_client(format!("http://{addr}/v1/responses"));
        let response = client
            .post_native_responses(&payload, &http_test_context(), false, false)
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.bytes().await.unwrap(),
            br#"{"id":"resp_native","object":"response"}"#.as_slice()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_responses_refreshes_once_before_returning_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = authenticated_http_test_client(format!("http://{addr}/v1/responses"));
        client.request_compression = crate::config::CodexRequestCompression::Gzip;
        let client = Arc::new(client);
        let payload = serde_json::json!({"model":"gpt-5.4","input":"hello"});
        let expected = serde_json::to_vec(&payload).unwrap();
        let server_client = client.clone();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut stream).await;
                assert_wire_json(&request, &expected, true);
                let request = String::from_utf8_lossy(&request);
                if attempt == 0 {
                    assert!(request.contains("authorization: Bearer test"));
                    server_client.auth_manager().set_test_auth(StoredAuth {
                        access: "rotated".into(),
                        refresh: "rotated-refresh".into(),
                        account_id: Some("acct".into()),
                        expires: u64::MAX,
                    });
                    stream
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                } else {
                    assert!(request.contains("authorization: Bearer rotated"));
                    assert!(request.contains("chatgpt-account-id: acct"));
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                        )
                        .await
                        .unwrap();
                }
            }
        });

        let response = client
            .post_native_responses(&payload, &http_test_context(), false, false)
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap(), b"{}".as_slice());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_responses_does_not_resend_after_account_change() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/v1/responses"
        )));
        let server_client = client.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_request(&mut stream).await;
            server_client.auth_manager().set_test_auth(StoredAuth {
                access: "rotated".into(),
                refresh: "rotated-refresh".into(),
                account_id: Some("other-account".into()),
                expires: u64::MAX,
            });
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            drop(stream);
            tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_ok()
        });

        let payload =
            serde_json::json!({"model":"gpt-5.4","previous_response_id":"resp_first_account"});
        let result = client
            .post_native_responses(&payload, &http_test_context(), false, false)
            .await;
        assert!(
            !server.await.unwrap(),
            "native Responses tried another account"
        );
        let error = result.unwrap_err();
        assert_eq!(error.status, 409);
        assert_eq!(error.detail.as_deref(), Some(CODEX_ACCOUNT_CHANGED_DETAIL));
    }

    #[tokio::test]
    async fn native_responses_does_not_follow_redirects() {
        let source = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = source.local_addr().unwrap();
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let source_server = tokio::spawn(async move {
            let (mut stream, _) = source.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let response = format!(
                "HTTP/1.1 302 Found\r\nlocation: http://{target_addr}/stolen\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let client = authenticated_http_test_client(format!("http://{source_addr}/v1/responses"));
        let response = client
            .post_native_responses(
                &serde_json::json!({"model":"gpt-5.4","input":"hello"}),
                &http_test_context(),
                false,
                false,
            )
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        source_server.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), target.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn standalone_search_posts_json_to_alpha_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            let header_end = request
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap();
            let headers = String::from_utf8_lossy(&request[..header_end]);
            assert!(headers.starts_with("POST /alpha/search HTTP/1.1"));
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("accept: application/json")
            );
            assert!(headers.contains("authorization: Bearer test"));
            assert!(headers.contains("session-id: session"));
            assert!(headers.contains("x-client-request-id: session"));
            assert!(headers.contains("x-codex-window-id: session:0"));
            let body: serde_json::Value =
                serde_json::from_slice(&request[header_end + 4..]).unwrap();
            assert!(body["id"] == "session");
            assert_eq!(body["model"], "gpt-5.6-luna");
            assert!(body.get("reasoning").is_none());
            assert_eq!(body["commands"]["search_query"][0]["q"], "find Codex");

            let response = serde_json::to_vec(&serde_json::json!({
                "encrypted_output": "opaque",
                "output": "search output",
                "results": [{
                    "type": "text_result",
                    "ref_id": "turn0search0",
                    "url": "https://example.com",
                    "title": "Example"
                }]
            }))
            .unwrap();
            let response_headers = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response.len()
            );
            stream.write_all(response_headers.as_bytes()).await.unwrap();
            stream.write_all(&response).await.unwrap();
        });

        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let request = super::super::search::SearchRequest {
            id: "session".to_string(),
            model: "gpt-5.6-luna".to_string(),
            reasoning: None,
            input: None,
            commands: super::super::search::SearchCommands {
                search_query: vec![super::super::search::SearchQuery {
                    q: "find Codex".to_string(),
                }],
            },
            settings: super::super::search::SearchSettings {
                filters: None,
                allowed_callers: vec!["direct"],
                external_web_access: true,
            },
            max_output_tokens: 2_500,
        };
        let response = client
            .post_search(&request, &http_test_context())
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(response.output, "search output");
        assert_eq!(response.results.unwrap().len(), 1);
    }
}
