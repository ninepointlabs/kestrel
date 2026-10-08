mod cli;
mod client;
mod config;
mod mcp_server;
mod rate_limiter;

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result, bail};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};
use crate::client::XClient;
use crate::config::{Config, DEFAULT_DAILY_LIMIT};
use crate::rate_limiter::RateLimiter;

#[tokio::main]
async fn main() {
    // Logs go to stderr: stdout carries the MCP protocol in `serve` mode.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("KESTREL_LOG").unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(io::stderr)
        .init();

    if let Err(e) = run(Cli::parse()).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Post { text } => {
            let config = Config::load()?;
            let limiter = RateLimiter::new(config.daily_limit)?;
            let client = XClient::new(config)?;
            let (tweet, state) = client::post_with_limit(&client, &limiter, &text).await?;
            println!("Posted: {}", tweet.url());
            println!("{state}");
        }
        Command::Status => {
            let config = Config::load()?;
            let state = RateLimiter::new(config.daily_limit)?.status()?;
            println!("{state}");
        }
        Command::Configure => configure()?,
        Command::Serve => mcp_server::serve(Config::load()?).await?,
    }
    Ok(())
}

fn configure() -> Result<()> {
    let existing = Config::load().ok();
    println!(
        "Kestrel setup. Get credentials at https://developer.x.com (app needs Read and Write)."
    );
    println!("Press Enter to keep the current value shown in [brackets].\n");

    let stdin = io::stdin();
    let mut input = stdin.lock();
    let ex = existing.as_ref();

    let config = Config {
        api_key: prompt(&mut input, "API key", ex.map(|c| c.api_key.as_str()))?,
        api_secret: prompt(&mut input, "API secret", ex.map(|c| c.api_secret.as_str()))?,
        access_token: prompt(
            &mut input,
            "Access token",
            ex.map(|c| c.access_token.as_str()),
        )?,
        access_token_secret: prompt(
            &mut input,
            "Access token secret",
            ex.map(|c| c.access_token_secret.as_str()),
        )?,
        daily_limit: {
            let current = ex
                .map_or(DEFAULT_DAILY_LIMIT, |c| c.daily_limit)
                .to_string();
            let raw = prompt_plain(&mut input, "Daily post limit", &current)?;
            raw.parse()
                .with_context(|| format!("daily limit must be a whole number, got {raw:?}"))?
        },
    };

    let path = config.save()?;
    println!("\nSaved {}", path.display());
    Ok(())
}

/// Prompt for a secret; the current value is shown masked.
fn prompt(input: &mut impl BufRead, label: &str, current: Option<&str>) -> Result<String> {
    let shown = current.map(mask);
    let value = read_line(input, label, shown.as_deref())?;
    match (value.is_empty(), current) {
        (false, _) => Ok(value),
        (true, Some(cur)) => Ok(cur.to_owned()),
        (true, None) => bail!("{label} is required"),
    }
}

fn prompt_plain(input: &mut impl BufRead, label: &str, current: &str) -> Result<String> {
    let value = read_line(input, label, Some(current))?;
    Ok(if value.is_empty() {
        current.to_owned()
    } else {
        value
    })
}

fn read_line(input: &mut impl BufRead, label: &str, shown: Option<&str>) -> Result<String> {
    match shown {
        Some(s) => print!("{label} [{s}]: "),
        None => print!("{label}: "),
    }
    io::stdout().flush().context("failed to write prompt")?;
    let mut line = String::new();
    if input.read_line(&mut line).context("failed to read input")? == 0 {
        bail!("input closed before configuration was complete");
    }
    Ok(line.trim().to_owned())
}

fn mask(secret: &str) -> String {
    let tail: String = secret
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("****{tail}")
}
