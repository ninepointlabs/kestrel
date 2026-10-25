//! X API v2 client: OAuth 1.0a (HMAC-SHA1) request signing + `POST /2/tweets`,
//! plus image upload via the v1.1 media endpoint.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use chrono::{Local, NaiveDate};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::config::{self, Config, PREMIUM_POST_LENGTH, STANDARD_POST_LENGTH};
use crate::rate_limiter::{RateLimiter, State};

pub const TWEETS_URL: &str = "https://api.x.com/2/tweets";
pub const MEDIA_UPLOAD_URL: &str = "https://upload.x.com/1.1/media/upload.json";
pub const USERS_ME_URL: &str = "https://api.x.com/2/users/me";

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
    users_me_url: String,
    /// Where the auto-detected subscription type is cached (`account.json`).
    account_cache: PathBuf,
}

impl XClient {
    pub fn new(config: Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("kestrel/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            http,
            config,
            users_me_url: USERS_ME_URL.to_owned(),
            account_cache: config::config_dir()?.join("account.json"),
        })
    }

    /// Refuse text that is empty or longer than [`Self::max_post_length`].
    pub async fn check_text(&self, text: &str) -> Result<()> {
        validate_text(text, None)?;
        validate_text(text, self.max_post_length().await)
    }

    /// The post length limit to enforce locally. An explicit `max_post_length` wins;
    /// `auto` uses the account's X subscription, looked up at most once a day.
    /// `None` when the lookup fails: the length is then left for X to judge.
    pub async fn max_post_length(&self) -> Option<usize> {
        if let Some(n) = self.config.max_post_length.fixed() {
            return Some(n);
        }
        let today = Local::now().date_naive();
        let fingerprint = token_fingerprint(&self.config.access_token);
        if let Some(n) = read_account_cache(&self.account_cache, &fingerprint, today)
            .as_deref()
            .and_then(length_for_subscription)
        {
            return Some(n);
        }

        let subscription_type = match self.fetch_subscription_type().await {
            Ok(kind) => kind,
            Err(e) => {
                tracing::warn!(
                    "could not check this account's X subscription, so post length is left \
                     to X ({e:#}). Set max_post_length in config.toml to skip the check."
                );
                return None;
            }
        };
        let Some(n) = length_for_subscription(&subscription_type) else {
            tracing::warn!(
                "unknown X subscription type {subscription_type:?}, so post length is left \
                 to X. Set max_post_length in config.toml to skip the check."
            );
            return None;
        };
        let cache = AccountCache {
            token_fingerprint: fingerprint,
            subscription_type,
            checked_on: today,
        };
        if let Err(e) = write_account_cache(&self.account_cache, &cache) {
            tracing::warn!("failed to cache the X subscription type: {e:#}");
        }
        Some(n)
    }

    /// `GET /2/users/me?user.fields=subscription_type` for the authenticated account.
    async fn fetch_subscription_type(&self) -> Result<String> {
        let query = [("user.fields", "subscription_type")];
        let auth = authorization_header_with_params(
            "GET",
            &self.users_me_url,
            &query,
            &self.config,
            &generate_nonce(),
            &unix_timestamp()?,
        )?;
        let response = self
            .http
            .get(format!("{}?{}", self.users_me_url, query_string(&query)))
            .header(header::AUTHORIZATION, auth)
            .send()
            .await
            .map_err(|e| anyhow!("network error contacting X API: {e}"))?;
        let body = read_body(response).await?;
        let parsed: UsersMeResponse = serde_json::from_str(&body)
            .with_context(|| format!("unexpected X API response: {body}"))?;
        parsed
            .data
            .subscription_type
            .context("X did not report a subscription_type for this account")
    }

    /// Post a tweet, optionally attaching media previously returned by [`Self::upload_media`]
    /// and optionally as a reply to the tweet `reply_to_id`.
    pub async fn post_tweet(
        &self,
        text: &str,
        media_ids: Option<&[String]>,
        reply_to_id: Option<&str>,
    ) -> Result<Tweet> {
        validate_text(text, self.config.max_post_length.fixed())?;
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

/// Refuse text X would reject (empty, or over `max_len` when a limit is known)
/// before any API call.
pub fn validate_text(text: &str, max_len: Option<usize>) -> Result<()> {
    if text.trim().is_empty() {
        bail!("tweet text is empty");
    }
    let Some(max_len) = max_len else {
        return Ok(());
    };
    let len = tweet_length(text);
    if len > max_len {
        bail!(
            "tweet is too long: {len}/{max_len} characters as X counts them \
             (each link counts as {URL_LENGTH}; emoji and CJK characters count as 2). \
             Set by `max_post_length` in config.toml."
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
struct UsersMeResponse {
    data: UserFields,
}

#[derive(Deserialize)]
struct UserFields {
    subscription_type: Option<String>,
}

/// X's `subscription_type` values: Premium tiers may post long text.
fn length_for_subscription(subscription_type: &str) -> Option<usize> {
    match subscription_type {
        "Premium" | "PremiumPlus" => Some(PREMIUM_POST_LENGTH),
        "Basic" | "None" => Some(STANDARD_POST_LENGTH),
        _ => None,
    }
}

/// Cached result of the subscription lookup, valid for the day and the token it was made with.
#[derive(Debug, Serialize, Deserialize)]
struct AccountCache {
    token_fingerprint: String,
    subscription_type: String,
    checked_on: NaiveDate,
}

/// Identifies the account without storing the token itself.
fn token_fingerprint(access_token: &str) -> String {
    Sha1::digest(access_token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The cached subscription type, if the cache is from `today` and for this token.
/// A missing or unreadable cache just means looking it up again.
fn read_account_cache(path: &Path, fingerprint: &str, today: NaiveDate) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let cache: AccountCache = serde_json::from_str(&raw).ok()?;
    (cache.token_fingerprint == fingerprint && cache.checked_on == today)
        .then_some(cache.subscription_type)
}

fn write_account_cache(path: &Path, cache: &AccountCache) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }
    let body = serde_json::to_string_pretty(cache).context("failed to serialize cache")?;
    fs::write(path, body).with_context(|| format!("failed to write {}", path.display()))
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
    authorization_header_with_params(method, url, &[], config, nonce, timestamp)
}

/// [`authorization_header`] for a request with query parameters: they are signed,
/// but sent in the URL rather than the header. `url` excludes the query string.
pub fn authorization_header_with_params(
    method: &str,
    url: &str,
    query: &[(&str, &str)],
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

    let signed_params: Vec<(&str, &str)> = oauth_params.iter().chain(query).copied().collect();
    let signature = sign(
        method,
        url,
        &signed_params,
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

/// `k=v&k=v`, percent-encoded the same way the signature encodes them.
fn query_string(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
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
            max_post_length: config::MaxPostLength::Auto,
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
        let standard = Some(STANDARD_POST_LENGTH);
        assert!(validate_text(&"a".repeat(280), standard).is_ok());
        assert!(validate_text(&"日".repeat(140), standard).is_ok());
        let err = validate_text(&"a".repeat(281), standard)
            .unwrap_err()
            .to_string();
        assert!(err.contains("too long: 281/280"), "{err}");
        assert!(validate_text(&"日".repeat(141), standard).is_err());
        assert!(
            validate_text(
                &format!(
                    "{} https://example.com/{}",
                    "a".repeat(256),
                    "x".repeat(100)
                ),
                standard
            )
            .is_ok()
        );

        let premium = Some(PREMIUM_POST_LENGTH);
        assert!(validate_text(&"a".repeat(281), premium).is_ok());
        assert!(validate_text(&"a".repeat(25_000), premium).is_ok());
        let err = validate_text(&"a".repeat(25_001), premium)
            .unwrap_err()
            .to_string();
        assert!(err.contains("too long: 25001/25000"), "{err}");

        // No known limit: only emptiness is checked.
        assert!(validate_text(&"a".repeat(100_000), None).is_ok());
        assert!(validate_text("  ", None).is_err());
        assert!(validate_text("  ", premium).is_err());
    }

    #[test]
    fn query_params_are_signed_but_not_in_header() {
        let config = test_config(config::MaxPostLength::Auto);
        let query = [("user.fields", "subscription_type")];
        let with = authorization_header_with_params(
            "GET",
            USERS_ME_URL,
            &query,
            &config,
            "nonce123",
            "1700000000",
        )
        .unwrap();
        let without =
            authorization_header("GET", USERS_ME_URL, &config, "nonce123", "1700000000").unwrap();
        assert!(!with.contains("user.fields"), "{with}");
        let signature = |h: &str| h.split("oauth_signature=").nth(1).unwrap().to_owned();
        assert_ne!(signature(&with), signature(&without));

        let mut params = vec![
            ("oauth_consumer_key", "ck"),
            ("oauth_nonce", "nonce123"),
            ("oauth_signature_method", "HMAC-SHA1"),
            ("oauth_timestamp", "1700000000"),
            ("oauth_token", "tk"),
            ("oauth_version", "1.0"),
        ];
        params.extend(query);
        let expected = sign("GET", USERS_ME_URL, &params, "cs", "ts").unwrap();
        assert!(
            with.contains(&format!(
                "oauth_signature=\"{}\"",
                percent_encode(&expected)
            )),
            "{with}"
        );
    }

    #[test]
    fn subscription_types_map_to_limits() {
        assert_eq!(length_for_subscription("Premium"), Some(25_000));
        assert_eq!(length_for_subscription("PremiumPlus"), Some(25_000));
        assert_eq!(length_for_subscription("Basic"), Some(280));
        assert_eq!(length_for_subscription("None"), Some(280));
        assert_eq!(length_for_subscription("SomethingNew"), None);
    }

    #[test]
    fn account_cache_is_per_token_and_per_day() {
        let dir = temp_dir("cache");
        let path = dir.join("account.json");
        let today = NaiveDate::from_ymd_opt(2026, 10, 25).unwrap();
        assert_eq!(read_account_cache(&path, "abc", today), None);
        write_account_cache(
            &path,
            &AccountCache {
                token_fingerprint: "abc".into(),
                subscription_type: "Premium".into(),
                checked_on: today,
            },
        )
        .unwrap();
        assert_eq!(
            read_account_cache(&path, "abc", today).as_deref(),
            Some("Premium")
        );
        assert_eq!(read_account_cache(&path, "other-token", today), None);
        assert_eq!(
            read_account_cache(&path, "abc", today.succ_opt().unwrap()),
            None
        );
        fs::write(&path, "not json").unwrap();
        assert_eq!(read_account_cache(&path, "abc", today), None);
        assert_ne!(token_fingerprint("tk"), token_fingerprint("tk2"));
        assert!(!token_fingerprint("tk").contains("tk"));
        let _ = fs::remove_dir_all(dir);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kestrel-client-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_config(max_post_length: config::MaxPostLength) -> Config {
        Config {
            api_key: "ck".into(),
            api_secret: "cs".into(),
            access_token: "tk".into(),
            access_token_secret: "ts".into(),
            daily_limit: 50,
            max_post_length,
        }
    }

    /// A client whose `/2/users/me` is `users_me_url` and whose cache lives in `dir`.
    fn mock_client(
        max_post_length: config::MaxPostLength,
        users_me_url: &str,
        dir: &Path,
    ) -> XClient {
        let mut client = XClient::new(test_config(max_post_length)).unwrap();
        client.users_me_url = users_me_url.to_owned();
        client.account_cache = dir.join("account.json");
        client
    }

    /// A stand-in for X that answers `responses.len()` requests with the given
    /// status and JSON body, in order. Resolves to the request heads it received.
    async fn mock_x(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/2/users/me", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let mut heads = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                heads.push(String::from_utf8_lossy(&buf).into_owned());
                let response = format!(
                    "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            heads
        });
        (url, handle)
    }

    /// A URL nothing listens on, so any request to it fails.
    async fn dead_url() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/2/users/me", listener.local_addr().unwrap());
        drop(listener);
        url
    }

    #[tokio::test]
    async fn auto_detects_premium_and_caches_it() {
        let dir = temp_dir("premium");
        let (url, server) = mock_x(vec![(
            200,
            r#"{"data":{"id":"1","name":"N","username":"n","subscription_type":"Premium"}}"#,
        )])
        .await;
        let client = mock_client(config::MaxPostLength::Auto, &url, &dir);
        assert_eq!(client.max_post_length().await, Some(25_000));
        client.check_text(&"a".repeat(281)).await.unwrap();

        let heads = server.await.unwrap();
        assert_eq!(heads.len(), 1, "second check should use the cache");
        let head = &heads[0];
        assert!(
            head.starts_with("GET /2/users/me?user.fields=subscription_type HTTP/1.1"),
            "{head}"
        );
        assert!(
            head.to_ascii_lowercase()
                .contains("authorization: oauth oauth_consumer_key=\"ck\""),
            "{head}"
        );

        // With the server gone, the cached answer still holds for today.
        let client = mock_client(config::MaxPostLength::Auto, &dead_url().await, &dir);
        assert_eq!(client.max_post_length().await, Some(25_000));
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn auto_detects_standard_and_refuses_long_text() {
        for kind in ["Basic", "None"] {
            let dir = temp_dir(&format!("standard-{kind}"));
            let body: &'static str = match kind {
                "Basic" => r#"{"data":{"id":"1","subscription_type":"Basic"}}"#,
                _ => r#"{"data":{"id":"1","subscription_type":"None"}}"#,
            };
            let (url, server) = mock_x(vec![(200, body)]).await;
            let client = mock_client(config::MaxPostLength::Auto, &url, &dir);
            let err = client
                .check_text(&"a".repeat(281))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("too long: 281/280"), "{kind}: {err}");
            client.check_text(&"a".repeat(280)).await.unwrap();
            server.await.unwrap();
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[tokio::test]
    async fn failed_detection_does_not_refuse() {
        let dir = temp_dir("failed");
        let (url, server) = mock_x(vec![
            (401, r#"{"title":"Unauthorized","detail":"Unauthorized"}"#),
            (200, r#"{"data":{"id":"1"}}"#),
            (200, r#"{"data":{"id":"1","subscription_type":"Mystery"}}"#),
            (200, "not json"),
        ])
        .await;
        let client = mock_client(config::MaxPostLength::Auto, &url, &dir);
        for _ in 0..4 {
            assert_eq!(client.max_post_length().await, None);
        }
        server.await.unwrap();

        let client = mock_client(config::MaxPostLength::Auto, &dead_url().await, &dir);
        client.check_text(&"a".repeat(30_000)).await.unwrap();
        assert!(client.check_text(" ").await.is_err());
        assert!(
            !dir.join("account.json").exists(),
            "failures must not be cached"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn explicit_setting_skips_detection() {
        let dir = temp_dir("explicit");
        // The cache says Premium, but an explicit setting wins without any lookup.
        write_account_cache(
            &dir.join("account.json"),
            &AccountCache {
                token_fingerprint: token_fingerprint("tk"),
                subscription_type: "Premium".into(),
                checked_on: Local::now().date_naive(),
            },
        )
        .unwrap();
        let url = dead_url().await;
        for (setting, expected) in [
            (config::MaxPostLength::Standard, 280),
            (config::MaxPostLength::Premium, 25_000),
            (config::MaxPostLength::Chars(4000), 4000),
        ] {
            let client = mock_client(setting, &url, &dir);
            assert_eq!(client.max_post_length().await, Some(expected));
        }
        let err = mock_client(config::MaxPostLength::Standard, &url, &dir)
            .check_text(&"a".repeat(281))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("too long: 281/280"), "{err}");
        let _ = fs::remove_dir_all(dir);
    }

    fn test_client() -> XClient {
        XClient::new(test_config(config::MaxPostLength::Auto)).unwrap()
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
