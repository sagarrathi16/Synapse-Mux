use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;

use crate::config::VerificationCommand;
use crate::security::{scrub_secrets, truncate_output};

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024; // 256 KB limit per check stream

#[derive(Error, Debug)]
pub enum CheckError {
    #[error("Command execution error for '{name}': {source}")]
    Execution {
        name: String,
        source: std::io::Error,
    },
    #[error("Verification command '{0}' timed out")]
    Timeout(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckStatus {
    Passed,
    Failed,
    NotRun,
    Inconclusive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub command: Vec<String>,
    pub status: CheckStatus,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub required: bool,
    pub message: Option<String>,
}

pub struct CheckRunner;

impl CheckRunner {
    pub async fn run_command(
        cmd_spec: &VerificationCommand,
        worktree_dir: &Path,
    ) -> Result<CheckResult, CheckError> {
        let start = Instant::now();

        if cmd_spec.argv.is_empty() {
            return Ok(CheckResult {
                name: cmd_spec.name.clone(),
                command: vec![],
                status: CheckStatus::NotRun,
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: 0,
                required: cmd_spec.required,
                message: Some("Empty argv vector".into()),
            });
        }

        let program = &cmd_spec.argv[0];
        let args = &cmd_spec.argv[1..];

        let working_dir = if let Some(ref rel_cwd) = cmd_spec.cwd {
            worktree_dir.join(rel_cwd)
        } else {
            worktree_dir.to_path_buf()
        };

        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(&working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let timeout = Duration::from_secs(cmd_spec.timeout_seconds);

        let output_res = tokio::time::timeout(timeout, cmd.output()).await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match output_res {
            Ok(Ok(output)) => {
                let raw_stdout = String::from_utf8_lossy(&output.stdout);
                let raw_stderr = String::from_utf8_lossy(&output.stderr);
                let scrubbed_stdout = scrub_secrets(&raw_stdout);
                let scrubbed_stderr = scrub_secrets(&raw_stderr);
                let stdout = truncate_output(&scrubbed_stdout, DEFAULT_MAX_OUTPUT_BYTES, false);
                let stderr = truncate_output(&scrubbed_stderr, DEFAULT_MAX_OUTPUT_BYTES, true);
                let exit_code = output.status.code();
                let status = if output.status.success() {
                    CheckStatus::Passed
                } else {
                    CheckStatus::Failed
                };

                Ok(CheckResult {
                    name: cmd_spec.name.clone(),
                    command: cmd_spec.argv.clone(),
                    status,
                    exit_code,
                    stdout,
                    stderr,
                    duration_ms,
                    required: cmd_spec.required,
                    message: None,
                })
            }
            Ok(Err(e)) => Err(CheckError::Execution {
                name: cmd_spec.name.clone(),
                source: e,
            }),
            Err(_) => Ok(CheckResult {
                name: cmd_spec.name.clone(),
                command: cmd_spec.argv.clone(),
                status: CheckStatus::Inconclusive,
                exit_code: None,
                stdout: String::new(),
                stderr: format!("Timed out after {} seconds", cmd_spec.timeout_seconds),
                duration_ms,
                required: cmd_spec.required,
                message: Some("Command timed out".into()),
            }),
        }
    }

    pub async fn run_all(
        commands: &[VerificationCommand],
        worktree_dir: &Path,
    ) -> Vec<CheckResult> {
        let mut results = Vec::new();
        for cmd_spec in commands {
            match Self::run_command(cmd_spec, worktree_dir).await {
                Ok(res) => results.push(res),
                Err(e) => {
                    results.push(CheckResult {
                        name: cmd_spec.name.clone(),
                        command: cmd_spec.argv.clone(),
                        status: CheckStatus::Failed,
                        exit_code: None,
                        stdout: String::new(),
                        stderr: e.to_string(),
                        duration_ms: 0,
                        required: cmd_spec.required,
                        message: Some(format!("Execution failed: {e}")),
                    });
                }
            }
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_check_runner_success_and_failure() {
        let temp = TempDir::new().unwrap();

        // Successful check using git or echo / python
        let pass_cmd = VerificationCommand {
            name: "git_check".into(),
            argv: vec!["git".into(), "--version".into()],
            cwd: None,
            timeout_seconds: 10,
            required: true,
        };

        let res = CheckRunner::run_command(&pass_cmd, temp.path())
            .await
            .unwrap();
        assert_eq!(res.status, CheckStatus::Passed);
        assert!(res.stdout.contains("git version"));

        // Failing check
        let fail_cmd = VerificationCommand {
            name: "git_invalid".into(),
            argv: vec!["git".into(), "invalid-subcommand-xyz".into()],
            cwd: None,
            timeout_seconds: 10,
            required: true,
        };

        let fail_res = CheckRunner::run_command(&fail_cmd, temp.path())
            .await
            .unwrap();
        assert_eq!(fail_res.status, CheckStatus::Failed);

        // Check secret scrubbing in command output
        let echo_secret_cmd = VerificationCommand {
            name: "echo_secret".into(),
            argv: vec![
                "git".into(),
                "config".into(),
                "--default".into(),
                "sk-ant-secret123456789".into(),
                "user.token".into(),
            ],
            cwd: None,
            timeout_seconds: 10,
            required: true,
        };
        let secret_res = CheckRunner::run_command(&echo_secret_cmd, temp.path())
            .await
            .unwrap();
        assert!(!secret_res.stdout.contains("sk-ant-secret123456789"));
        assert!(secret_res.stdout.contains("sk-ant-***REDACTED***"));
    }
}
