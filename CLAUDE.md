# Kestrel — X/Twitter CLI + MCP Server

A Rust CLI app for posting to X (Twitter) via the X API v2. Single-user: each person brings their own API keys. Also runs as an MCP server so Hermes and other AI agents can post through it, with a hard daily post limit to prevent runaway agent posting.

## Architecture

Single binary with two modes:
1. **CLI mode**: `kestrel post "tweet text"`, `kestrel status`, `kestrel configure`
2. **MCP server mode**: `kestrel serve` — JSON-RPC over stdio

## Tech Stack
- Rust 2024 edition
- `clap` for CLI (derive mode)
- `reqwest` for HTTP (with `json` and `rustls-tls` features)
- OAuth 1.0a signing: use `hmac`, `sha2`, `rand`, `base64` crates (no oauth1 crate needed; implement the signing manually per X's docs)
- `rmcp` crate for MCP server (stdio transport)
- `tokio` for async runtime (full features)
- `serde` + `serde_json` for JSON
- `toml` + `serde` for config file format
- `chrono` for date/time (rate limiter reset)
- `dirs` for config directory (~/.config/kestrel/)
- `anyhow` for error handling
- `tracing` + `tracing-subscriber` for logging

## Config File
Location: `~/.config/kestrel/config.toml`
```toml
# X API v2 credentials (from developer.x.com)
api_key = "YOUR_API_KEY"
api_secret = "YOUR_API_SECRET"
access_token = "YOUR_ACCESS_TOKEN"
access_token_secret = "YOUR_ACCESS_TOKEN_SECRET"

# Daily post limit (default: 50)
daily_limit = 50
```

## Rate Limiter
- File: `~/.config/kestrel/state.json`
- Tracks: `{ "date": "2026-10-07", "count": 3, "daily_limit": 50 }`
- Check before every post: if `date != today`, reset count to 0. If `count >= daily_limit`, refuse.
- CLI status shows: "Posts today: 3/50 (6%)"

## X API Client
- Endpoint: `POST https://api.x.com/2/tweets`
- Auth: OAuth 1.0a User Context
- Headers: `oauth_consumer_key`, `oauth_token`, `oauth_signature_method` (HMAC-SHA1), `oauth_timestamp`, `oauth_nonce`, `oauth_version` (1.0), `oauth_signature`
- Body: JSON `{"text": "tweet content"}`
- Content-Type: application/json
- Error handling: parse X API error responses, surface meaningful messages

## CLI Commands
```
kestrel post "Hello world"          # Post a tweet
kestrel status                       # Show rate limit usage
kestrel configure                    # Interactive config setup (prompts for keys)
kestrel serve                        # Start MCP server on stdio
```

## MCP Server
- Transport: stdio (standard MCP)
- Server name: "kestrel"
- Tools exposed:
  1. `kestrel_post` — args: `text` (string, required). Posts to X. Returns success/error.
  2. `kestrel_status` — no args. Returns current rate limit state.
- Use the `rmcp` crate. Register tools with proper JSON schemas for inputs.

## Project Structure
```
src/
  main.rs          — entry point, dispatch to CLI or serve
  cli.rs           — clap derive structs
  config.rs        — load/save config.toml
  client.rs        — X API v2 posting (OAuth 1.0a signing + HTTP)
  rate_limiter.rs  — daily counter, check/reset/increment
  mcp_server.rs    — rmcp-based MCP server on stdio
```

## Quality Standards
- All error paths handled (no unwrap() in production code paths)
- Clear error messages for auth failures, network errors, rate limits
- `--help` output that reads well
- README.md with install instructions (cargo install --path .) and config setup
- Git repo initialized at ~/Projects/kestrel
- Cargo.toml with all deps, build and run without errors