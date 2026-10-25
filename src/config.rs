use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub const DEFAULT_DAILY_LIMIT: u32 = 50;
/// X's post length limits, in weighted characters.
pub const STANDARD_POST_LENGTH: usize = 280;
pub const PREMIUM_POST_LENGTH: usize = 25_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub api_key: String,
    pub api_secret: String,
    pub access_token: String,
    pub access_token_secret: String,
    #[serde(default = "default_daily_limit")]
    pub daily_limit: u32,
    #[serde(default)]
    pub max_post_length: MaxPostLength,
}

/// The `max_post_length` setting: `"auto"`, `"premium"`, `"standard"`, or a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MaxPostLength {
    /// Ask X for the account's subscription type (see `XClient::max_post_length`).
    #[default]
    Auto,
    Premium,
    Standard,
    Chars(usize),
}

impl MaxPostLength {
    /// The limit when set explicitly; `None` for `Auto`.
    pub fn fixed(self) -> Option<usize> {
        match self {
            Self::Auto => None,
            Self::Premium => Some(PREMIUM_POST_LENGTH),
            Self::Standard => Some(STANDARD_POST_LENGTH),
            Self::Chars(n) => Some(n),
        }
    }
}

impl Serialize for MaxPostLength {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => s.serialize_str("auto"),
            Self::Premium => s.serialize_str("premium"),
            Self::Standard => s.serialize_str("standard"),
            Self::Chars(n) => s.serialize_u64(*n as u64),
        }
    }
}

impl<'de> Deserialize<'de> for MaxPostLength {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Chars(i64),
            Name(String),
        }
        let invalid = |got: &dyn std::fmt::Debug| {
            de::Error::custom(format!(
                "max_post_length must be \"auto\", \"premium\", \"standard\", \
                 or a positive number, got {got:?}"
            ))
        };
        match Raw::deserialize(d)? {
            Raw::Chars(n) => match usize::try_from(n) {
                Ok(n) if n > 0 => Ok(Self::Chars(n)),
                _ => Err(invalid(&n)),
            },
            Raw::Name(name) => match name.to_ascii_lowercase().as_str() {
                "auto" => Ok(Self::Auto),
                "premium" => Ok(Self::Premium),
                "standard" => Ok(Self::Standard),
                _ => Err(invalid(&name)),
            },
        }
    }
}

fn default_daily_limit() -> u32 {
    DEFAULT_DAILY_LIMIT
}

/// `~/.config/kestrel`
pub fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("could not determine the user config directory")?;
    Ok(base.join("kestrel"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
                "no config found at {}. Run `kestrel configure` to set your X API credentials.",
                path.display()
            ),
            Err(e) => {
                return Err(e).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let config: Config = toml::from_str(&raw)
            .with_context(|| format!("invalid config in {}", path.display()))?;
        config.validate(path)?;
        Ok(config)
    }

    fn validate(&self, path: &Path) -> Result<()> {
        let fields = [
            ("api_key", &self.api_key),
            ("api_secret", &self.api_secret),
            ("access_token", &self.access_token),
            ("access_token_secret", &self.access_token_secret),
        ];
        let missing: Vec<&str> = fields
            .iter()
            .filter(|(_, v)| v.trim().is_empty() || v.starts_with("YOUR_"))
            .map(|(k, _)| *k)
            .collect();
        if !missing.is_empty() {
            bail!(
                "missing credentials in {}: {}. Run `kestrel configure`.",
                path.display(),
                missing.join(", ")
            );
        }
        Ok(())
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = config_path()?;
        self.save_to(&path)?;
        Ok(path)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        let body = toml::to_string_pretty(self).context("failed to serialize config")?;
        let contents =
            format!("# Kestrel config — X API v2 credentials (from developer.x.com)\n{body}");
        fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))?;
        restrict_permissions(path)?;
        Ok(())
    }
}

/// Credentials are secrets: make the file readable by the owner only.
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_for_optional_settings() {
        let config: Config = toml::from_str(
            r#"
            api_key = "a"
            api_secret = "b"
            access_token = "c"
            access_token_secret = "d"
            "#,
        )
        .unwrap();
        assert_eq!(config.daily_limit, 50);
        assert_eq!(config.max_post_length, MaxPostLength::Auto);
    }

    #[test]
    fn parses_max_post_length() {
        let parse = |value: &str| {
            toml::from_str::<Config>(&format!(
                "api_key = \"a\"\napi_secret = \"b\"\naccess_token = \"c\"\n\
                 access_token_secret = \"d\"\nmax_post_length = {value}\n"
            ))
            .map(|c| c.max_post_length)
            .map_err(|e| e.to_string())
        };
        assert_eq!(parse("\"auto\""), Ok(MaxPostLength::Auto));
        assert_eq!(parse("\"Premium\""), Ok(MaxPostLength::Premium));
        assert_eq!(parse("\"standard\""), Ok(MaxPostLength::Standard));
        assert_eq!(parse("4000"), Ok(MaxPostLength::Chars(4000)));
        assert_eq!(MaxPostLength::Premium.fixed(), Some(25_000));
        assert_eq!(MaxPostLength::Standard.fixed(), Some(280));
        assert_eq!(MaxPostLength::Auto.fixed(), None);
        for bad in ["\"huge\"", "0", "-5"] {
            let err = parse(bad).unwrap_err();
            assert!(err.contains("max_post_length must be"), "{bad}: {err}");
        }
    }

    #[test]
    fn rejects_placeholder_credentials() {
        let dir = std::env::temp_dir().join(format!("kestrel-cfg-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "api_key = \"YOUR_API_KEY\"\napi_secret = \"b\"\naccess_token = \"c\"\naccess_token_secret = \"\"\n",
        )
        .unwrap();
        let err = Config::load_from(&path).unwrap_err().to_string();
        assert!(err.contains("api_key"), "{err}");
        assert!(err.contains("access_token_secret"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("kestrel-cfg-rt-{}", std::process::id()));
        let path = dir.join("config.toml");
        let config = Config {
            api_key: "a".into(),
            api_secret: "b".into(),
            access_token: "c".into(),
            access_token_secret: "d".into(),
            daily_limit: 7,
            max_post_length: MaxPostLength::Chars(280),
        };
        config.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.daily_limit, 7);
        assert_eq!(loaded.max_post_length, MaxPostLength::Chars(280));
        for setting in [MaxPostLength::Auto, MaxPostLength::Premium] {
            Config {
                max_post_length: setting,
                ..config.clone()
            }
            .save_to(&path)
            .unwrap();
            assert_eq!(Config::load_from(&path).unwrap().max_post_length, setting);
        }
        assert_eq!(loaded.access_token_secret, "d");
        fs::remove_dir_all(&dir).unwrap();
    }
}
