//! MCP server over stdio exposing `kestrel_post` and `kestrel_status`.

use std::io;
use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    tool, tool_handler, tool_router,
    transport::Transport,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdin, Stdout};
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
        // Don't spend a subscription lookup or an upload on a post the daily limit
        // would refuse.
        self.limiter.check()?;
        self.client.check_text(&args.text).await?;
        let media_ids = match &args.image_path {
            Some(path) => vec![self.client.upload_media(path).await?],
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
        .serve(StdioTransport::new())
        .await
        .context("failed to start MCP server")?;
    running.waiting().await.context("MCP server error")?;
    Ok(())
}

/// Newline-delimited JSON-RPC over stdin/stdout.
///
/// rmcp's own stdio transport silently drops input that isn't valid JSON; this one
/// answers it with a JSON-RPC parse error (-32700) as the spec requires.
struct StdioTransport {
    stdin: BufReader<Stdin>,
    /// Kept across `receive` calls: a cancelled read leaves its partial line here.
    line_buf: Vec<u8>,
    stdout: Arc<Mutex<Stdout>>,
}

impl StdioTransport {
    fn new() -> Self {
        Self {
            stdin: BufReader::new(tokio::io::stdin()),
            line_buf: Vec::new(),
            stdout: Arc::new(Mutex::new(tokio::io::stdout())),
        }
    }
}

async fn write_line(stdout: &Mutex<Stdout>, mut line: Vec<u8>) -> io::Result<()> {
    line.push(b'\n');
    let mut out = stdout.lock().await;
    out.write_all(&line).await?;
    out.flush().await
}

impl Transport<RoleServer> for StdioTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let stdout = self.stdout.clone();
        let line = serde_json::to_vec(&item).map_err(io::Error::other);
        async move { write_line(&stdout, line?).await }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        loop {
            match self.stdin.read_until(b'\n', &mut self.line_buf).await {
                Ok(0) => return None,
                Ok(_) => {}
                Err(e) => {
                    tracing::error!("error reading stdin: {e}");
                    return None;
                }
            }
            let line = std::mem::take(&mut self.line_buf);
            match parse_line(&line) {
                Incoming::Message(msg) => return Some(*msg),
                Incoming::Skip => {}
                Incoming::Reply(reply) => {
                    let bytes = serde_json::to_vec(&reply).ok()?;
                    if let Err(e) = write_line(&self.stdout, bytes).await {
                        tracing::error!("error writing stdout: {e}");
                        return None;
                    }
                }
            }
        }
    }

    async fn close(&mut self) -> io::Result<()> {
        Ok(())
    }
}

enum Incoming {
    Message(Box<RxJsonRpcMessage<RoleServer>>),
    /// An error response to send back instead of handling the line.
    Reply(Value),
    Skip,
}

fn parse_line(line: &[u8]) -> Incoming {
    let line = line
        .strip_prefix(b"\xEF\xBB\xBF")
        .unwrap_or(line)
        .trim_ascii();
    if line.is_empty() {
        return Incoming::Skip;
    }
    let err = match serde_json::from_slice(line) {
        Ok(msg) => return Incoming::Message(Box::new(msg)),
        Err(e) => e,
    };
    let Ok(value) = serde_json::from_slice::<Value>(line) else {
        tracing::debug!("parse error on incoming message: {err}");
        return Incoming::Reply(error_reply(Value::Null, -32700, "Parse error"));
    };
    // Notifications never get a reply, even unknown or malformed ones.
    if value.get("method").is_some() && value.get("id").is_none() {
        tracing::debug!("ignoring notification: {err}");
        return Incoming::Skip;
    }
    tracing::debug!("invalid request: {err}");
    let id = match value.get("id") {
        Some(id @ (Value::String(_) | Value::Number(_))) => id.clone(),
        _ => Value::Null,
    };
    Incoming::Reply(error_reply(id, -32600, "Invalid Request"))
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(line: &str) -> Option<Value> {
        match parse_line(line.as_bytes()) {
            Incoming::Reply(v) => Some(v),
            _ => None,
        }
    }

    #[test]
    fn invalid_json_gets_parse_error() {
        let r = reply("this is not json").unwrap();
        assert_eq!(r, error_reply(Value::Null, -32700, "Parse error"));
        assert_eq!(reply("{\"jsonrpc\":").unwrap()["error"]["code"], -32700);
    }

    #[test]
    fn valid_messages_and_notifications_pass_or_skip() {
        assert!(matches!(
            parse_line(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
            Incoming::Message(_)
        ));
        assert!(matches!(parse_line(b"\r\n"), Incoming::Skip));
        assert!(matches!(
            parse_line(br#"{"jsonrpc":"2.0","method":"notifications/custom","params":5}"#),
            Incoming::Skip | Incoming::Message(_)
        ));
    }

    #[test]
    fn wrong_shape_gets_invalid_request() {
        let r = reply(r#"{"foo":"bar"}"#).unwrap();
        assert_eq!(r, error_reply(Value::Null, -32600, "Invalid Request"));
        let r = reply(r#"{"jsonrpc":"2.0","id":7}"#).unwrap();
        assert_eq!(r["id"], 7);
    }
}
