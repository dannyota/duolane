use crate::anthropic::schema::MessagesRequest;
use crate::monitor::MonitorHandle;
use crate::request_identity::ConversationIdentity;
use crate::traffic::TrafficCapture;
use anyhow::Result;
use async_trait::async_trait;
use axum::response::Response;
use clap::Subcommand;
use std::sync::Arc;

pub const MAX_BUFFERED_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Subcommand)]
pub enum AuthCommand {
    #[command(about = "Sign in using browser-based authentication")]
    Login,
    #[command(about = "Sign in using a device code")]
    Device,
    #[command(about = "Show the current authentication status")]
    Status,
    #[command(about = "Delete stored authentication credentials")]
    Logout,
}

#[allow(clippy::double_must_use)]
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn supported_models(&self) -> Vec<String>;
    fn cli(&self) -> &'static dyn CliHandlers;

    fn missing_login_hint(&self) -> Option<&'static str> {
        None
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response;

    async fn handle_messages_with_conversation_identity(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
    ) -> Response {
        let _ = conversation_identity;
        self.handle_messages(body, ctx).await
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response;
}

pub trait CliHandlers: Send + Sync {
    fn login(&self) -> Result<()>;
    fn device(&self) -> Result<()>;
    fn status(&self) -> Result<()>;
    fn logout(&self) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub req_id: String,
    pub session_id: Option<String>,
    pub traffic: Option<Arc<TrafficCapture>>,
    pub monitor: Option<MonitorHandle>,
    pub passthrough: Option<Passthrough>,
}

#[derive(Debug, Clone)]
pub struct Passthrough {
    pub raw_body: axum::body::Bytes,
    pub headers: axum::http::HeaderMap,
    pub path_and_query: String,
}
