# Kestrel

Post to X (Twitter) from the command line, or let AI agents post through it as an
MCP server. Each user brings their own X API keys. A hard daily post limit, shared by
the CLI and the MCP server, keeps an agent from posting without bound.

## Install

Requires a Rust toolchain with edition 2024 support (Rust 1.85+).

```sh
git clone <this repo> ~/Projects/kestrel
cd ~/Projects/kestrel
cargo install --path .
```

This puts `kestrel` in `~/.cargo/bin` (make sure it is on your `PATH`).

## Get X API credentials

1. Create a project and app at <https://developer.x.com>.
2. In the app's **User authentication settings**, set app permissions to **Read and Write**.
3. Under **Keys and tokens**, copy the **API Key** and **API Key Secret**, then generate
   an **Access Token** and **Access Token Secret**. If you changed permissions after
   generating the access token, regenerate it; otherwise posts fail with 403.

## Configure

```sh
kestrel configure
```

This prompts for the four credentials and the daily limit, then writes
`~/.config/kestrel/config.toml` with `0600` permissions. Run it again to change a
value; press Enter to keep the current one.

You can also write the file by hand:

```toml
# X API v2 credentials (from developer.x.com)
api_key = "YOUR_API_KEY"
api_secret = "YOUR_API_SECRET"
access_token = "YOUR_ACCESS_TOKEN"
access_token_secret = "YOUR_ACCESS_TOKEN_SECRET"

# Daily post limit (default: 50)
daily_limit = 50
```

## Usage

```sh
kestrel post "Hello world"   # Post a tweet, prints its URL and today's usage
kestrel post "check this out" --image ./photo.png   # Post with an image (-i for short)
kestrel status               # Posts today: 3/50 (6%)
kestrel configure            # Interactive credential setup
kestrel serve                # MCP server on stdio
kestrel --help
```

Images (PNG, JPEG, GIF, WebP) are uploaded to X's media endpoint first, then attached
to the tweet. Only the posted tweet counts toward Kestrel's daily limit, and no upload
is attempted once the limit is reached.

Set `KESTREL_LOG=info` (or `debug`) for logs on stderr.

## Daily limit

Usage is tracked in `~/.config/kestrel/state.json`:

```json
{ "date": "2026-10-07", "count": 3, "daily_limit": 50 }
```

- The count resets when the local date changes.
- Every post checks the limit first. Once `count >= daily_limit`, Kestrel refuses to post.
- Only successful posts count. A post rejected by X does not use up the allowance.
- `daily_limit` in `config.toml` is the source of truth. If you lower it, the new
  limit applies right away.

## MCP server

`kestrel serve` speaks MCP (JSON-RPC over stdio). It identifies itself as `kestrel`
and exposes two tools:

| Tool             | Arguments         | Description                                    |
|------------------|-------------------|------------------------------------------------|
| `kestrel_post`   | `text` (string), `image_path` (string, optional) | Post to X, optionally attaching a local image. Returns an error result at the limit. |
| `kestrel_status` | none              | Today's count, limit, and remaining posts.     |

Example client configuration (Claude Desktop / Claude Code / Hermes style):

```json
{
  "mcpServers": {
    "kestrel": {
      "command": "kestrel",
      "args": ["serve"]
    }
  }
}
```

If the client does not inherit your `PATH`, use the full path, e.g.
`/home/you/.cargo/bin/kestrel`.

## Troubleshooting

| Error | Likely cause |
|-------|--------------|
| `401 ... authentication failed` | Wrong key or secret, a revoked token, or a skewed system clock. |
| `403 ... forbidden` | The app lacks Read and Write permission, the token was generated before the permission change, or the tweet is a duplicate. |
| `429 ... rate limit hit` | X's own API rate limit, not Kestrel's daily limit. The message shows the reset time. |
| `daily post limit reached` | Kestrel's daily limit. It resets tomorrow, or you can raise `daily_limit`. |

## Development

```sh
cargo build
cargo test     # unit tests plus end-to-end CLI/MCP tests (no network)
cargo clippy --all-targets
```
