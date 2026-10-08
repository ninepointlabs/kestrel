use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const DEFAULT_DAILY_LIMIT: u32 = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub api_key: String,
    pub api_secret: String,
    pub access_token: String,
    pub access_token_secret: String,
    #[serde(default = "default_daily_limit")]
    pub daily_limit: u32,
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
    fn daily_limit_defaults_to_50() {
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
        };
        config.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.daily_limit, 7);
        assert_eq!(loaded.access_token_secret, "d");
        fs::remove_dir_all(&dir).unwrap();
    }
}
