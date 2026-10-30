use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use chrono::Utc;
use colored::Colorize;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::timeout;

use crate::ui;

const CHECK_INTERVAL_SECS: i64 = 86400; // 24 hours
const NETWORK_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug, Serialize, Deserialize)]
struct UpdateCache {
    last_check_timestamp: i64,
    latest_version: String,
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

fn cache_file_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".synapse").join("update_cache.json"))
}

/// Parses a semver string into (major, minor, patch).
pub fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let clean = v.trim().trim_start_matches('v');
    let parts: Vec<&str> = clean.split('.').collect();
    if parts.len() < 3 {
        return None;
    }
    let major = parts[0].parse().ok()?;
    let minor = parts[1].parse().ok()?;
    let patch_str = parts[2].split(|c: char| !c.is_ascii_digit()).next()?;
    let patch = patch_str.parse().ok()?;
    Some((major, minor, patch))
}

/// Returns true if `latest` is strictly newer than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_semver(latest), parse_semver(current)) {
        (Some((l_maj, l_min, l_pat)), Some((c_maj, c_min, c_pat))) => {
            (l_maj, l_min, l_pat) > (c_maj, c_min, c_pat)
        }
        _ => false,
    }
}

/// Fetches the latest published version of synapse-mux from NPM registry.
pub async fn fetch_latest_npm_version() -> Option<String> {
    // Try curl first (fast and standard across Windows, macOS, Linux)
    let curl_future = async {
        let output = Command::new("curl")
            .args([
                "-s",
                "-m",
                "2",
                "https://registry.npmjs.org/synapse-mux/latest",
            ])
            .output()
            .await
            .ok()?;
        if output.status.success() {
            let body = String::from_utf8_lossy(&output.stdout);
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body)
                && let Some(v) = json.get("version").and_then(|v| v.as_str())
            {
                return Some(v.to_string());
            }
        }
        None
    };

    if let Ok(Some(v)) = timeout(NETWORK_TIMEOUT, curl_future).await {
        return Some(v);
    }

    // Fallback to npm view if curl fails
    let npm_future = async {
        let output = Command::new("npm")
            .args(["view", "synapse-mux", "version"])
            .output()
            .await
            .ok()?;
        if output.status.success() {
            let v = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !v.is_empty() && v.contains('.') {
                return Some(v);
            }
        }
        None
    };

    if let Ok(Some(v)) = timeout(NETWORK_TIMEOUT, npm_future).await {
        return Some(v);
    }

    None
}

/// Non-blocking check for available updates respecting 24-hour cache.
pub async fn check_for_update_cached(current_version: &str) -> Option<String> {
    let now = Utc::now().timestamp();
    let cache_path = cache_file_path()?;

    // Check if cache is fresh
    if let Ok(data) = fs::read_to_string(&cache_path)
        && let Ok(cache) = serde_json::from_str::<UpdateCache>(&data)
        && now - cache.last_check_timestamp < CHECK_INTERVAL_SECS
    {
        if is_newer(&cache.latest_version, current_version) {
            return Some(cache.latest_version);
        }
        return None;
    }

    // Fetch latest online
    if let Some(latest) = fetch_latest_npm_version().await {
        if let Some(parent) = cache_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let cache = UpdateCache {
            last_check_timestamp: now,
            latest_version: latest.clone(),
        };
        let _ = fs::write(
            &cache_path,
            serde_json::to_string(&cache).unwrap_or_default(),
        );

        if is_newer(&latest, current_version) {
            return Some(latest);
        }
    }

    None
}

/// Prints a modern, non-intrusive update notification banner.
pub fn print_update_banner(current: &str, latest: &str) {
    println!(
        "\n{}",
        "╭──────────────────────────────────────────────────────────╮".cyan()
    );
    println!(
        "│  {} {:<38} │",
        "Update available:".yellow().bold(),
        format!("{} → {}", current.dimmed(), latest.green().bold())
    );
    println!(
        "│  Run {} or {} to update   │",
        "synapse update".cyan().bold(),
        "npm i -g synapse-mux@latest".dimmed()
    );
    println!(
        "{}\n",
        "╰──────────────────────────────────────────────────────────╯".cyan()
    );
}

/// Executes the `synapse update` self-update command.
pub async fn run_update(check_only: bool) -> ExitCode {
    let current = env!("CARGO_PKG_VERSION");
    println!(
        "\n{} {}",
        "⚡".yellow(),
        "Checking for Synapse-Mux updates...".bold()
    );

    let mut steps = ui::StepProgress::new(if check_only { 1 } else { 2 });
    steps.start_step("Checking NPM registry for latest release");

    let latest_opt = fetch_latest_npm_version().await;
    let latest = match latest_opt {
        Some(v) => v,
        None => {
            steps.fail_step("Unable to query NPM registry (check your internet connection)");
            return ExitCode::FAILURE;
        }
    };

    if !is_newer(&latest, current) {
        steps.complete_step(&format!(
            "Synapse is already up to date (current: v{}, latest: v{})",
            current, latest
        ));
        return ExitCode::SUCCESS;
    }

    steps.complete_step(&format!("Update found: v{} → v{}", current, latest));

    if check_only {
        println!("\nRun {} to update now.\n", "synapse update".cyan().bold());
        return ExitCode::SUCCESS;
    }

    steps.start_step(&format!("Running: npm install -g synapse-mux@{}", latest));

    let is_win = cfg!(windows);
    let mut cmd = if is_win {
        let mut c = Command::new("cmd");
        c.args(["/C", "npm", "install", "-g", "synapse-mux@latest"]);
        c
    } else {
        let mut c = Command::new("npm");
        c.args(["install", "-g", "synapse-mux@latest"]);
        c
    };

    let status = cmd.status().await;

    match status {
        Ok(s) if s.success() => {
            steps.complete_step(&format!("Successfully upgraded Synapse to v{}!", latest));
            println!(
                "\n{} Synapse is now running on {}\n",
                "✔".green().bold(),
                format!("v{}", latest).cyan().bold()
            );
            ExitCode::SUCCESS
        }
        Ok(_) | Err(_) => {
            steps.fail_step("Failed to install global update via npm");
            ui::print_error_box(
                "Update Command Failed",
                "The npm installation did not succeed. This typically happens if global npm installs require elevated privileges.",
                "Try running manually: npm install -g synapse-mux@latest (or with sudo on Linux/macOS)",
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_semver_parsing() {
        assert_eq!(parse_semver("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_semver("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("2.0.1-alpha"), Some((2, 0, 1)));
    }

    #[test]
    fn test_is_newer() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }
}
