//! X API v2 client: OAuth 1.0a (HMAC-SHA1) request signing + `POST /2/tweets`,
//! plus image upload via the v1.1 media endpoint.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::{StatusCode, header};
use serde::Deserialize;
use sha1::Sha1;

use crate::config::Config;
use crate::rate_limiter::{RateLimiter, State};

pub const TWEETS_URL: &str = "https://api.x.com/2/tweets";
pub const MEDIA_UPLOAD_URL: &str = "https://upload.x.com/1.1/media/upload.json";

/// X's limit for a standard (non-Premium) post, in weighted characters.
pub const MAX_TWEET_LENGTH: usize = 280;
/// X wraps every link in t.co, so each URL counts as this many characters.
const URL_LENGTH: usize = 23;

#[derive(Debug, Clone)]
pub struct Tweet {
    pub id: String,
    pub text: String,
}

impl Tweet {
    pub fn url(&self) -> String {
        format!("https://x.com/i/status/{}", self.id)
    }
}

pub struct XClient {
    http: reqwest::Client,
    config: Config,
}

impl XClient {
    pub fn new(config: Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("kestrel/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self { http, config })
    }

    /// Post a tweet, optionally attaching media previously returned by [`Self::upload_media`]
    /// and optionally as a reply to the tweet `reply_to_id`.
    pub async fn post_tweet(
        &self,
        text: &str,
        media_ids: Option<&[String]>,
        reply_to_id: Option<&str>,
    ) -> Result<Tweet> {
        validate_text(text)?;
        if let Some(id) = reply_to_id {
            validate_tweet_id(id)?;
        }

        let auth = authorization_header(
            "POST",
            TWEETS_URL,
            &self.config,
            &generate_nonce(),
            &unix_timestamp()?,
        )?;

        let response = self
            .http
            .post(TWEETS_URL)
            .header(header::AUTHORIZATION, auth)
            .json(&tweet_body(text, media_ids, reply_to_id))
            .send()
            .await
            .map_err(|e| anyhow!("network error contacting X API: {e}"))?;
        let body = read_body(response).await?;

        let parsed: CreateTweetResponse = serde_json::from_str(&body)
            .with_context(|| format!("unexpected X API response: {body}"))?;
        Ok(Tweet {
            id: parsed.data.id,
            text: parsed.data.text,
        })
    }

    /// Post `text` as a reply to the tweet `reply_to_id` (e.g. a "summoned reply"
    /// carrying the link for a plain parent tweet).
    pub async fn post_reply(
        &self,
        text: &str,
        reply_to_id: &str,
        media_ids: Option<&[String]>,
    ) -> Result<Tweet> {
        self.post_tweet(text, media_ids, Some(reply_to_id)).await
    }

    /// Upload an image file and return its `media_id_string` for use in [`Self::post_tweet`].
    ///
    /// The body is multipart, so (as with JSON) it is not part of the OAuth signature.
    pub async fn upload_media(&self, image_path: &str) -> Result<String> {
        let path = Path::new(image_path);
        let bytes = tokio::fs::read(path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow!("image file not found: {image_path}")
            } else {
                anyhow!("failed to read image file {image_path}: {e}")
            }
        })?;
        if bytes.is_empty() {
            bail!("image file is empty: {image_path}");
        }
        let Some(mime) = sniff_image_mime(&bytes) else {
            bail!("not a supported image: {image_path} (expected PNG, JPEG, GIF, or WebP)");
        };

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("image")
            .to_owned();
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str(mime)
            .context("failed to set image content type")?;
        let form = reqwest::multipart::Form::new().part("media", part);

        let auth = authorization_header(
            "POST",
            MEDIA_UPLOAD_URL,
            &self.config,
            &generate_nonce(),
            &unix_timestamp()?,
        )?;

        let response = self
            .http
            .post(MEDIA_UPLOAD_URL)
            .header(header::AUTHORIZATION, auth)
            .multipart(form)
            .send()
            .await
            .map_err(|e| anyhow!("network error uploading media to X: {e}"))?;
        let body = read_body(response).await.context("media upload failed")?;
        parse_media_id(&body)
    }
}

/// Read a response body, turning non-2xx statuses into a descriptive X API error.
async fn read_body(response: reqwest::Response) -> Result<String> {
    let status = response.status();
    let reset = response
        .headers()
        .get("x-rate-limit-reset")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = response
        .text()
        .await
        .map_err(|e| anyhow!("network error reading X API response: {e}"))?;

    if !status.is_success() {
        return Err(api_error(status, &body, reset.as_deref()));
    }
    Ok(body)
}

fn tweet_body(
    text: &str,
    media_ids: Option<&[String]>,
    reply_to_id: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::json!({ "text": text });
    if let Some(ids) = media_ids.filter(|ids| !ids.is_empty()) {
        body["media"] = serde_json::json!({ "media_ids": ids });
    }
    if let Some(id) = reply_to_id {
        body["reply"] = serde_json::json!({ "in_reply_to_tweet_id": id });
    }
    body
}

/// Tweet IDs are numeric strings; catch typos (or pasted URLs) before calling the API.
fn validate_tweet_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        bail!(
            "invalid tweet ID to reply to: {id:?} (expected digits only, e.g. 1846987139428634858)"
        );
    }
    Ok(())
}

fn parse_media_id(body: &str) -> Result<String> {
    let parsed: MediaUploadResponse = serde_json::from_str(body)
        .with_context(|| format!("unexpected X media upload response: {body}"))?;
    Ok(parsed.media_id_string)
}

/// Identify an image by its magic bytes; these are the formats X accepts.
fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Refuse text X would reject (empty or over the length limit) before any API call.
pub fn validate_text(text: &str) -> Result<()> {
    if text.trim().is_empty() {
        bail!("tweet text is empty");
    }
    let len = tweet_length(text);
    if len > MAX_TWEET_LENGTH {
        bail!(
            "tweet is too long: {len}/{MAX_TWEET_LENGTH} characters as X counts them \
             (each link counts as {URL_LENGTH}; emoji and CJK characters count as 2)"
        );
    }
    Ok(())
}

/// Weighted length following X's counting rules (twitter-text v3), simplified:
/// links count as [`URL_LENGTH`], Latin and most punctuation as 1, everything else
/// (CJK, emoji, ...) as 2, and an emoji sequence (ZWJ, skin tone, flag) as one emoji.
/// Only `http://` and `https://` links are recognised, and text is not NFC-normalised.
pub fn tweet_length(text: &str) -> usize {
    text.split_inclusive(char::is_whitespace)
        .map(|word| {
            let url = word.trim_end_matches(|c: char| {
                c.is_whitespace()
                    || matches!(c, '.' | ',' | ':' | ';' | '!' | '?' | ')' | '\'' | '"')
            });
            match url
                .strip_prefix("https://")
                .or_else(|| url.strip_prefix("http://"))
            {
                Some(rest) if !rest.is_empty() => URL_LENGTH + weighted_chars(&word[url.len()..]),
                _ => weighted_chars(word),
            }
        })
        .sum()
}

fn weighted_chars(s: &str) -> usize {
    let mut len = 0;
    let mut prev = None;
    let mut half_flag = false;
    for c in s.chars() {
        len += match c {
            // Modifiers and joined code points belong to the preceding emoji.
            '\u{200D}' | '\u{FE0E}' | '\u{FE0F}' | '\u{1F3FB}'..='\u{1F3FF}' => 0,
            _ if prev == Some('\u{200D}') => 0,
            // A flag is a pair of regional indicators.
            '\u{1F1E6}'..='\u{1F1FF}' if half_flag => 0,
            '\u{0000}'..='\u{10FF}'
            | '\u{2000}'..='\u{200D}'
            | '\u{2010}'..='\u{201F}'
            | '\u{2032}'..='\u{2037}' => 1,
            _ => 2,
        };
        half_flag = matches!(c, '\u{1F1E6}'..='\u{1F1FF}') && !half_flag;
        prev = Some(c);
    }
    len
}

/// Post while enforcing the daily limit: check, post, then count the post.
/// A failed post is not counted.
pub async fn post_with_limit(
    client: &XClient,
    limiter: &RateLimiter,
    text: &str,
    media_ids: Option<&[String]>,
    reply_to_id: Option<&str>,
) -> Result<(Tweet, State)> {
    limiter.check()?;
    let tweet = match reply_to_id {
        Some(id) => client.post_reply(text, id, media_ids).await?,
        None => client.post_tweet(text, media_ids, None).await?,
    };
    let state = limiter
        .record()
        .context("tweet was posted, but updating the daily counter failed")?;
    Ok((tweet, state))
}

#[derive(Deserialize)]
struct CreateTweetResponse {
    data: CreatedTweet,
}

#[derive(Deserialize)]
struct CreatedTweet {
    id: String,
    text: String,
}

#[derive(Deserialize)]
struct MediaUploadResponse {
    media_id_string: String,
}

/// X API v2 returns either problem-details (`title`/`detail`) or an `errors` array;
/// the v1.1 media endpoint may instead return a bare `error` string.
#[derive(Deserialize, Default)]
struct ApiErrorBody {
    title: Option<String>,
    detail: Option<String>,
    error: Option<String>,
    #[serde(default)]
    errors: Vec<ApiErrorItem>,
}

#[derive(Deserialize)]
struct ApiErrorItem {
    message: Option<String>,
    detail: Option<String>,
}

fn api_error(status: StatusCode, body: &str, rate_limit_reset: Option<&str>) -> anyhow::Error {
    let parsed: ApiErrorBody = serde_json::from_str(body).unwrap_or_default();
    let mut messages: Vec<String> = parsed
        .errors
        .iter()
        .filter_map(|e| e.message.clone().or_else(|| e.detail.clone()))
        .collect();
    if let Some(detail) = parsed.detail.or(parsed.title).or(parsed.error) {
        messages.insert(0, detail);
    }
    let detail = if messages.is_empty() {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            status.canonical_reason().unwrap_or("no details").to_owned()
        } else {
            trimmed.to_owned()
        }
    } else {
        messages.join("; ")
    };

    let hint = match status {
        StatusCode::UNAUTHORIZED => {
            " — authentication failed. Check your API key/secret and access token/secret \
             (`kestrel configure`), and that your system clock is correct."
                .to_owned()
        }
        StatusCode::FORBIDDEN => {
            " — forbidden. Make sure your X app has Read and Write permissions and that \
             the access token was regenerated after changing them. X also returns 403 for \
             duplicate tweets."
                .to_owned()
        }
        StatusCode::TOO_MANY_REQUESTS => {
            let when = rate_limit_reset
                .and_then(|s| s.parse::<i64>().ok())
                .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
                .map(|t| {
                    format!(
                        " Resets at {}.",
                        t.with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M:%S %Z")
                    )
                })
                .unwrap_or_default();
            format!(" — X API rate limit hit.{when}")
        }
        _ => String::new(),
    };

    anyhow!("X API error {}: {detail}{hint}", status.as_u16())
}

/// Build the `Authorization: OAuth ...` header for an OAuth 1.0a user-context request.
///
/// The request body is JSON, so it is not part of the signature base string
/// (only form-encoded bodies are signed).
pub fn authorization_header(
    method: &str,
    url: &str,
    config: &Config,
    nonce: &str,
    timestamp: &str,
) -> Result<String> {
    let mut oauth_params = vec![
        ("oauth_consumer_key", config.api_key.as_str()),
        ("oauth_nonce", nonce),
        ("oauth_signature_method", "HMAC-SHA1"),
        ("oauth_timestamp", timestamp),
        ("oauth_token", config.access_token.as_str()),
        ("oauth_version", "1.0"),
    ];

    let signature = sign(
        method,
        url,
        &oauth_params,
        &config.api_secret,
        &config.access_token_secret,
    )?;
    oauth_params.push(("oauth_signature", &signature));
    oauth_params.sort();

    let fields: Vec<String> = oauth_params
        .iter()
        .map(|(k, v)| format!("{}=\"{}\"", percent_encode(k), percent_encode(v)))
        .collect();
    Ok(format!("OAuth {}", fields.join(", ")))
}

/// HMAC-SHA1 signature over the OAuth 1.0a signature base string.
/// `params` are all request parameters (oauth_* plus any query/form params), unencoded.
pub fn sign(
    method: &str,
    url: &str,
    params: &[(&str, &str)],
    consumer_secret: &str,
    token_secret: &str,
) -> Result<String> {
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (percent_encode(k), percent_encode(v)))
        .collect();
    encoded.sort();
    let param_string = encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    let base_string = format!(
        "{}&{}&{}",
        method.to_ascii_uppercase(),
        percent_encode(url),
        percent_encode(&param_string)
    );
    let signing_key = format!(
        "{}&{}",
        percent_encode(consumer_secret),
        percent_encode(token_secret)
    );

    let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(signing_key.as_bytes())
        .map_err(|e| anyhow!("failed to initialize HMAC-SHA1: {e}"))?;
    mac.update(base_string.as_bytes());
    Ok(BASE64.encode(mac.finalize().into_bytes()))
}

/// RFC 3986 percent-encoding as required by OAuth 1.0a: everything except
/// `A-Z a-z 0-9 - . _ ~` is encoded, with uppercase hex.
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn generate_nonce() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unix_timestamp() -> Result<String> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before 1970")?
        .as_secs();
    Ok(secs.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding_matches_rfc3986() {
        assert_eq!(
            percent_encode("Ladies + Gentlemen"),
            "Ladies%20%2B%20Gentlemen"
        );
        assert_eq!(
            percent_encode("An encoded string!"),
            "An%20encoded%20string%21"
        );
        assert_eq!(
            percent_encode("Dogs, Cats & Mice"),
            "Dogs%2C%20Cats%20%26%20Mice"
        );
        assert_eq!(percent_encode("☃"), "%E2%98%83");
        assert_eq!(percent_encode("abc-._~XYZ019"), "abc-._~XYZ019");
    }

    /// Worked example from X's "Creating a signature" documentation.
    #[test]
    fn signature_matches_x_documentation_example() {
        let params = [
            (
                "status",
                "Hello Ladies + Gentlemen, a signed OAuth request!",
            ),
            ("include_entities", "true"),
            ("oauth_consumer_key", "xvz1evFS4wEEPTGEFPHBog"),
            ("oauth_nonce", "kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg"),
            ("oauth_signature_method", "HMAC-SHA1"),
            ("oauth_timestamp", "1318622958"),
            (
                "oauth_token",
                "370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb",
            ),
            ("oauth_version", "1.0"),
        ];
        let sig = sign(
            "POST",
            "https://api.twitter.com/1.1/statuses/update.json",
            &params,
            "kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw",
            "LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE",
        )
        .unwrap();
        assert_eq!(sig, "hCtSmYh+iHYCEqBWrE7C7hYmtUk=");
    }

    #[test]
    fn authorization_header_has_all_oauth_fields() {
        let config = Config {
            api_key: "ck".into(),
            api_secret: "cs".into(),
            access_token: "tk".into(),
            access_token_secret: "ts".into(),
            daily_limit: 50,
        };
        let h =
            authorization_header("POST", TWEETS_URL, &config, "nonce123", "1700000000").unwrap();
        assert!(h.starts_with("OAuth "));
        for field in [
            "oauth_consumer_key=\"ck\"",
            "oauth_nonce=\"nonce123\"",
            "oauth_signature_method=\"HMAC-SHA1\"",
            "oauth_timestamp=\"1700000000\"",
            "oauth_token=\"tk\"",
            "oauth_version=\"1.0\"",
            "oauth_signature=\"",
        ] {
            assert!(h.contains(field), "missing {field} in {h}");
        }
    }

    #[test]
    fn api_error_surfaces_detail_and_hint() {
        let body =
            r#"{"title":"Unauthorized","type":"about:blank","status":401,"detail":"Unauthorized"}"#;
        let msg = api_error(StatusCode::UNAUTHORIZED, body, None).to_string();
        assert!(msg.contains("401"), "{msg}");
        assert!(msg.contains("authentication failed"), "{msg}");

        let body = r#"{"detail":"You are not allowed to create a Tweet with duplicate content.","type":"about:blank","title":"Forbidden","status":403}"#;
        let msg = api_error(StatusCode::FORBIDDEN, body, None).to_string();
        assert!(msg.contains("duplicate content"), "{msg}");

        let msg = api_error(StatusCode::TOO_MANY_REQUESTS, "", Some("1700000000")).to_string();
        assert!(msg.contains("Resets at"), "{msg}");

        let body = r#"{"errors":[{"code":324,"message":"Invalid media"}]}"#;
        let msg = api_error(StatusCode::BAD_REQUEST, body, None).to_string();
        assert!(msg.contains("Invalid media"), "{msg}");

        let body = r#"{"request":"/1.1/media/upload.json","error":"media type unrecognized."}"#;
        let msg = api_error(StatusCode::BAD_REQUEST, body, None).to_string();
        assert!(msg.contains("media type unrecognized"), "{msg}");
    }

    #[test]
    fn media_upload_response_parsing() {
        let body = r#"{
            "media_id": 710511363345354753,
            "media_id_string": "710511363345354753",
            "size": 11065,
            "expires_after_secs": 86400,
            "image": {"image_type": "image/png", "w": 800, "h": 320}
        }"#;
        assert_eq!(parse_media_id(body).unwrap(), "710511363345354753");

        let err = parse_media_id(r#"{"media_id": 1}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unexpected X media upload response"), "{err}");
    }

    #[test]
    fn tweet_body_includes_media_only_when_present() {
        assert_eq!(
            tweet_body("hi", None, None),
            serde_json::json!({ "text": "hi" })
        );
        assert_eq!(
            tweet_body("hi", Some(&[]), None),
            serde_json::json!({ "text": "hi" })
        );
        let ids = vec!["123".to_owned()];
        assert_eq!(
            tweet_body("hi", Some(&ids), None),
            serde_json::json!({ "text": "hi", "media": { "media_ids": ["123"] } })
        );
    }

    #[test]
    fn tweet_body_includes_reply_when_present() {
        assert_eq!(
            tweet_body("link", None, Some("1846987139428634858")),
            serde_json::json!({
                "text": "link",
                "reply": { "in_reply_to_tweet_id": "1846987139428634858" }
            })
        );
        let ids = vec!["123".to_owned()];
        assert_eq!(
            tweet_body("hi", Some(&ids), Some("42")),
            serde_json::json!({
                "text": "hi",
                "media": { "media_ids": ["123"] },
                "reply": { "in_reply_to_tweet_id": "42" }
            })
        );
    }

    #[test]
    fn tweet_id_validation() {
        assert!(validate_tweet_id("1846987139428634858").is_ok());
        assert!(validate_tweet_id("").is_err());
        assert!(validate_tweet_id("https://x.com/i/status/123").is_err());
        assert!(validate_tweet_id("12a").is_err());
    }

    #[test]
    fn image_type_from_magic_bytes() {
        assert_eq!(
            sniff_image_mime(b"\x89PNG\r\n\x1a\n...."),
            Some("image/png")
        );
        assert_eq!(
            sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_image_mime(b"GIF89a..."), Some("image/gif"));
        assert_eq!(
            sniff_image_mime(b"RIFF\0\0\0\0WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(sniff_image_mime(b"%PDF-1.7"), None);
        assert_eq!(sniff_image_mime(b"hello"), None);
    }

    #[test]
    fn tweet_length_follows_x_counting() {
        assert_eq!(tweet_length("hello world"), 11);
        assert_eq!(tweet_length("café — “quoted”"), 15);
        assert_eq!(tweet_length("日本語"), 6);
        assert_eq!(
            tweet_length("see https://example.com/a/very/long/path?q=1."),
            4 + 23 + 1
        );
        assert_eq!(tweet_length("http://"), 7);
        assert_eq!(tweet_length("👍"), 2);
        assert_eq!(tweet_length("👍🏽"), 2);
        assert_eq!(tweet_length("👨‍👩‍👧"), 2);
        assert_eq!(tweet_length("🇬🇧🇫🇷"), 4);
        assert_eq!(tweet_length("❤️"), 2);
    }

    #[test]
    fn text_validation() {
        assert!(validate_text(&"a".repeat(280)).is_ok());
        assert!(validate_text(&"日".repeat(140)).is_ok());
        let err = validate_text(&"a".repeat(281)).unwrap_err().to_string();
        assert!(err.contains("too long: 281/280"), "{err}");
        assert!(validate_text(&"日".repeat(141)).is_err());
        assert!(
            validate_text(&format!(
                "{} https://example.com/{}",
                "a".repeat(256),
                "x".repeat(100)
            ))
            .is_ok()
        );
        assert!(validate_text("  ").is_err());
    }

    fn test_client() -> XClient {
        XClient::new(Config {
            api_key: "ck".into(),
            api_secret: "cs".into(),
            access_token: "tk".into(),
            access_token_secret: "ts".into(),
            daily_limit: 50,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn upload_media_missing_file_is_clear_error() {
        let err = test_client()
            .upload_media("/nonexistent/kestrel-test.png")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("image file not found"), "{err}");
    }

    #[tokio::test]
    async fn upload_media_rejects_non_image_before_network() {
        let path =
            std::env::temp_dir().join(format!("kestrel-notimage-{}.png", std::process::id()));
        std::fs::write(&path, "just some text").unwrap();
        let err = test_client()
            .upload_media(path.to_str().unwrap())
            .await
            .unwrap_err()
            .to_string();
        let _ = std::fs::remove_file(&path);
        assert!(err.contains("not a supported image"), "{err}");
    }
}
