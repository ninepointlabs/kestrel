//! MCP server over stdio exposing `kestrel_post` and `kestrel_status`.

use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::client::{self, XClient};
use crate::config::Config;
use crate::rate_limiter::RateLimiter;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PostArgs {
    /// The text of the tweet to post.
    pub text: String,
    /// Optional local path to an image (PNG, JPEG, GIF, or WebP) to attach.
    #[serde(default)]
    pub image_path: Option<String>,
    /// Optional tweet ID to reply to (e.g. to put a link in a reply under a plain tweet).
    #[serde(default)]
    pub reply_to_id: Option<String>,
}

#[derive(Clone)]
pub struct KestrelServer {
    client: Arc<XClient>,
    limiter: Arc<RateLimiter>,
    /// Serializes check → post → record so concurrent calls can't exceed the daily limit.
    post_lock: Arc<Mutex<()>>,
}

impl KestrelServer {
    /// Upload the optional image, then post under the daily limit.
    /// Caller must hold `post_lock`.
    async fn post(&self, args: &PostArgs) -> Result<(client::Tweet, crate::rate_limiter::State)> {
        let media_ids = match &args.image_path {
            Some(path) => {
                // Don't spend an upload on a post the daily limit would refuse.
                self.limiter.check()?;
                vec![self.client.upload_media(path).await?]
            }
            None => Vec::new(),
        };
        client::post_with_limit(
            &self.client,
            &self.limiter,
            &args.text,
            Some(&media_ids),
            args.reply_to_id.as_deref(),
        )
        .await
    }
}

#[tool_router]
impl KestrelServer {
    #[tool(
        name = "kestrel_post",
        description = "Post a tweet to X, optionally with an image (image_path: local file \
                       path) and/or as a reply to another tweet (reply_to_id). Subject to a hard daily post limit; call kestrel_status to \
                       see remaining posts."
    )]
    async fn kestrel_post(
        &self,
        Parameters(args): Parameters<PostArgs>,
    ) -> Result<CallToolResult, McpError> {
        let _guard = self.post_lock.lock().await;
        match self.post(&args).await {
            Ok((tweet, state)) => {
                tracing::info!(id = %tweet.id, "posted tweet");
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Posted: {}\nID: {}\nText: {}\n{state}",
                    tweet.url(),
                    tweet.id,
                    tweet.text
                ))]))
            }
            Err(e) => {
                tracing::warn!("post failed: {e:#}");
                Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Post failed: {e:#}"
                ))]))
            }
        }
    }

    #[tool(
        name = "kestrel_status",
        description = "Show how many posts have been made today against the daily limit."
    )]
    async fn kestrel_status(&self) -> Result<CallToolResult, McpError> {
        match self.limiter.status() {
            Ok(state) => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "{state}\nRemaining today: {}\nDate: {}",
                state.remaining(),
                state.date
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Failed to read rate limit state: {e:#}"
            ))])),
        }
    }
}

#[tool_handler(
    name = "kestrel",
    instructions = "Post to X (Twitter). Use kestrel_status to check the remaining daily \
                    post allowance before posting; kestrel_post refuses once the daily limit is hit."
)]
impl ServerHandler for KestrelServer {}

pub async fn serve(config: Config) -> Result<()> {
    let limiter = RateLimiter::new(config.daily_limit)?;
    let server = KestrelServer {
        client: Arc::new(XClient::new(config)?),
        limiter: Arc::new(limiter),
        post_lock: Arc::new(Mutex::new(())),
    };
    tracing::info!("kestrel MCP server listening on stdio");
    let running = server
        .serve(rmcp::transport::stdio())
        .await
        .context("failed to start MCP server")?;
    running.waiting().await.context("MCP server error")?;
    Ok(())
}
