use super::{client::CodexHttpClient, profiles};
use crate::provider::RequestContext;
use axum::response::Response;
use std::sync::Arc;

#[allow(clippy::result_large_err)]
pub(super) async fn bind_codex_profile(
    client: &CodexHttpClient,
    mut ctx: RequestContext,
) -> Result<RequestContext, Response> {
    let selection = tokio::task::spawn_blocking(profiles::resolve_selected).await;
    let (name, source) = match selection {
        Ok(Ok(selection)) => selection,
        _ => return Err(selection_error()),
    };
    #[cfg(unix)]
    let fingerprint = {
        use std::os::unix::ffi::OsStrExt;
        ring::digest::digest(&ring::digest::SHA256, source.path().as_os_str().as_bytes())
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    #[cfg(not(unix))]
    let fingerprint = String::new();
    match client.manager_for_source(source) {
        Ok(Some(manager)) => {
            ctx.codex_profile = Some(Arc::new(CodexProfileBinding {
                name,
                fingerprint,
                manager,
            }))
        }
        Ok(None) => ctx.codex_profile = None,
        Err(_) => return Err(selection_error()),
    }
    Ok(ctx)
}

fn selection_error() -> Response {
    let mut response = crate::anthropic::error::json_error(
        http::StatusCode::INTERNAL_SERVER_ERROR,
        "api_error",
        "Codex profile selection is invalid; run `duolane codex auth use <name>`",
    );
    response
        .headers_mut()
        .insert("x-should-retry", http::HeaderValue::from_static("false"));
    response
}

pub struct CodexProfileBinding {
    pub name: String,
    pub fingerprint: String,
    pub manager:
        Arc<super::auth::manager::CodexAuthManager<super::auth::token_store::CodexAuthStore>>,
}

impl std::fmt::Debug for CodexProfileBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexProfileBinding")
            .field("name", &self.name)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}
