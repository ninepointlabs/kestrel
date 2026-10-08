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

    /// Post a tweet, optionally attaching media previously returned by [`Self::upload_media`].
    pub async fn post_tweet(&self, text: &str, media_ids: Option<&[String]>) -> Result<Tweet> {
        if text.trim().is_empty() {
            bail!("tweet text is empty");
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
            .json(&tweet_body(text, media_ids))
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

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("image")
            .to_owned();
        let mut part = reqwest::multipart::Part::bytes(bytes).file_name(file_name);
        if let Some(mime) = image_mime_type(path) {
            part = part
                .mime_str(mime)
                .context("failed to set image content type")?;
        }
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
        let body = read_body(response)
            .await
            .context("media upload failed")?;
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

fn tweet_body(text: &str, media_ids: Option<&[String]>) -> serde_json::Value {
    match media_ids {
        Some(ids) if !ids.is_empty() => {
            serde_json::json!({ "text": text, "media": { "media_ids": ids } })
        }
        _ => serde_json::json!({ "text": text }),
    }
}

fn parse_media_id(body: &str) -> Result<String> {
    let parsed: MediaUploadResponse = serde_json::from_str(body)
        .with_context(|| format!("unexpected X media upload response: {body}"))?;
    Ok(parsed.media_id_string)
}

fn image_mime_type(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Post while enforcing the daily limit: check, post, then count the post.
/// A failed post is not counted.
pub async fn post_with_limit(
    client: &XClient,
    limiter: &RateLimiter,
    text: &str,
    media_ids: Option<&[String]>,
) -> Result<(Tweet, State)> {
    limiter.check()?;
    let tweet = client.post_tweet(text, media_ids).await?;
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

        let err = parse_media_id(r#"{"media_id": 1}"#).unwrap_err().to_string();
        assert!(err.contains("unexpected X media upload response"), "{err}");
    }

    #[test]
    fn tweet_body_includes_media_only_when_present() {
        assert_eq!(tweet_body("hi", None), serde_json::json!({ "text": "hi" }));
        assert_eq!(tweet_body("hi", Some(&[])), serde_json::json!({ "text": "hi" }));
        let ids = vec!["123".to_owned()];
        assert_eq!(
            tweet_body("hi", Some(&ids)),
            serde_json::json!({ "text": "hi", "media": { "media_ids": ["123"] } })
        );
    }

    #[test]
    fn image_mime_type_from_extension() {
        assert_eq!(image_mime_type(Path::new("a/photo.PNG")), Some("image/png"));
        assert_eq!(image_mime_type(Path::new("x.jpeg")), Some("image/jpeg"));
        assert_eq!(image_mime_type(Path::new("noext")), None);
    }

    #[tokio::test]
    async fn upload_media_missing_file_is_clear_error() {
        let config = Config {
            api_key: "ck".into(),
            api_secret: "cs".into(),
            access_token: "tk".into(),
            access_token_secret: "ts".into(),
            daily_limit: 50,
        };
        let client = XClient::new(config).unwrap();
        let err = client
            .upload_media("/nonexistent/kestrel-test.png")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("image file not found"), "{err}");
    }
}
