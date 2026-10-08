use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{Local, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::config;

/// Persisted daily counter, stored in `~/.config/kestrel/state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub date: NaiveDate,
    pub count: u32,
    pub daily_limit: u32,
}

impl State {
    pub fn remaining(&self) -> u32 {
        self.daily_limit.saturating_sub(self.count)
    }

    pub fn is_exhausted(&self) -> bool {
        self.count >= self.daily_limit
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pct = if self.daily_limit == 0 {
            100
        } else {
            (u64::from(self.count) * 100 / u64::from(self.daily_limit)) as u32
        };
        write!(
            f,
            "Posts today: {}/{} ({pct}%)",
            self.count, self.daily_limit
        )
    }
}

pub struct RateLimiter {
    path: PathBuf,
    daily_limit: u32,
}

impl RateLimiter {
    /// Limiter backed by the default state file. `daily_limit` comes from config.
    pub fn new(daily_limit: u32) -> Result<Self> {
        Ok(Self::with_path(
            config::config_dir()?.join("state.json"),
            daily_limit,
        ))
    }

    pub fn with_path(path: PathBuf, daily_limit: u32) -> Self {
        Self { path, daily_limit }
    }

    /// Current state for today (count reset to 0 if the stored date is not today).
    pub fn status(&self) -> Result<State> {
        self.status_on(today())
    }

    /// Fail if today's limit has been reached.
    pub fn check(&self) -> Result<State> {
        self.check_on(today())
    }

    /// Record one successful post and persist it.
    pub fn record(&self) -> Result<State> {
        self.record_on(today())
    }

    fn status_on(&self, date: NaiveDate) -> Result<State> {
        let mut state = self.read()?.unwrap_or(State {
            date,
            count: 0,
            daily_limit: self.daily_limit,
        });
        if state.date != date {
            state.date = date;
            state.count = 0;
        }
        // Config is the source of truth for the limit.
        state.daily_limit = self.daily_limit;
        Ok(state)
    }

    fn check_on(&self, date: NaiveDate) -> Result<State> {
        let state = self.status_on(date)?;
        if state.is_exhausted() {
            bail!(
                "daily post limit reached ({}/{}). Kestrel will not post again until tomorrow (local time).",
                state.count,
                state.daily_limit
            );
        }
        Ok(state)
    }

    fn record_on(&self, date: NaiveDate) -> Result<State> {
        let mut state = self.status_on(date)?;
        state.count = state.count.saturating_add(1);
        self.write(&state)?;
        Ok(state)
    }

    fn read(&self) -> Result<Option<State>> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => serde_json::from_str(&raw).map(Some).with_context(|| {
                format!(
                    "corrupt rate limit state in {}. Fix or delete the file.",
                    self.path.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("failed to read {}", self.path.display())),
        }
    }

    /// Write via temp file + rename so a crash never leaves a half-written state file.
    fn write(&self, state: &State) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        let tmp = tmp_path(&self.path);
        let body = serde_json::to_string_pretty(state).context("failed to serialize state")?;
        fs::write(&tmp, body).with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("failed to replace {}", self.path.display()))
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

fn today() -> NaiveDate {
    Local::now().date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(name: &str, limit: u32) -> (RateLimiter, PathBuf) {
        let dir = std::env::temp_dir().join(format!("kestrel-rl-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (RateLimiter::with_path(dir.join("state.json"), limit), dir)
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    #[test]
    fn fresh_state_is_zero() {
        let (rl, dir) = limiter("fresh", 50);
        let s = rl.status_on(day(7)).unwrap();
        assert_eq!(s.count, 0);
        assert_eq!(s.to_string(), "Posts today: 0/50 (0%)");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn records_and_refuses_at_limit() {
        let (rl, dir) = limiter("limit", 2);
        rl.check_on(day(7)).unwrap();
        rl.record_on(day(7)).unwrap();
        rl.check_on(day(7)).unwrap();
        let s = rl.record_on(day(7)).unwrap();
        assert_eq!(s.count, 2);
        assert!(rl.check_on(day(7)).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resets_on_new_day() {
        let (rl, dir) = limiter("reset", 1);
        rl.record_on(day(7)).unwrap();
        assert!(rl.check_on(day(7)).is_err());
        let s = rl.check_on(day(8)).unwrap();
        assert_eq!(s.count, 0);
        assert_eq!(s.date, day(8));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn persists_expected_json_shape() {
        let (rl, dir) = limiter("shape", 50);
        for _ in 0..3 {
            rl.record_on(day(7)).unwrap();
        }
        let raw = fs::read_to_string(dir.join("state.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "date": "2026-10-07", "count": 3, "daily_limit": 50 })
        );
        assert_eq!(
            rl.status_on(day(7)).unwrap().to_string(),
            "Posts today: 3/50 (6%)"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn lowered_limit_in_config_takes_effect() {
        let (rl, dir) = limiter("lowered", 50);
        for _ in 0..5 {
            rl.record_on(day(7)).unwrap();
        }
        let rl = RateLimiter::with_path(dir.join("state.json"), 5);
        assert!(rl.check_on(day(7)).is_err());
        let _ = fs::remove_dir_all(dir);
    }
}
