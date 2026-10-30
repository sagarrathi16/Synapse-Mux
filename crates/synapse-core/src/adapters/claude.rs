use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, warn};

use super::{AdapterError, AgentEvent, ObservedUsage, WorkerAdapter, WorkerRequest, WorkerResult};

pub struct ClaudeCodeAdapter {
    binary_path: PathBuf,
}

impl Default for ClaudeCodeAdapter {
    fn default() -> Self {
        Self::new("claude")
    }
}

impl ClaudeCodeAdapter {
    pub fn new(binary_name_or_path: impl Into<PathBuf>) -> Self {
        Self {
            binary_path: binary_name_or_path.into(),
        }
    }

    pub fn parse_line(line: &str) -> AgentEvent {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return AgentEvent::MalformedEvent {
                raw_line: line.to_string(),
                error: "Empty line in stream".into(),
            };
        }

        let parsed: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                return AgentEvent::MalformedEvent {
                    raw_line: trimmed.to_string(),
                    error: format!("JSON parse error: {e}"),
                };
            }
        };

        let event_type = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match event_type {
            "system" | "init" => {
                let session_id = parsed
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .map(ToString::to_string);
                let model = parsed
                    .get("model")
                    .and_then(|m| m.as_str())
                    .map(ToString::to_string);
                AgentEvent::SystemInit {
                    session_id,
                    model,
                    raw: parsed,
                }
            }
            "assistant" | "message" => {
                let content = if let Some(content_str) =
                    parsed.get("content").and_then(|c| c.as_str())
                {
                    content_str.to_string()
                } else if let Some(msg_obj) = parsed.get("message") {
                    if let Some(c) = msg_obj.get("content").and_then(|c| c.as_str()) {
                        c.to_string()
                    } else if let Some(arr) = msg_obj.get("content").and_then(|c| c.as_array()) {
                        arr.iter()
                            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        msg_obj.to_string()
                    }
                } else {
                    parsed.to_string()
                };

                AgentEvent::AssistantMessage {
                    content,
                    raw: parsed,
                }
            }
            "tool_use" => {
                let tool = parsed
                    .get("tool")
                    .or_else(|| parsed.get("name"))
                    .and_then(|n| {
                        n.as_str()
                            .or_else(|| n.get("name").and_then(|x| x.as_str()))
                    })
                    .unwrap_or("unknown")
                    .to_string();

                let input = parsed
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));

                AgentEvent::ToolUse {
                    tool,
                    input,
                    raw: parsed,
                }
            }
            "tool_result" => {
                let tool = parsed
                    .get("tool")
                    .or_else(|| parsed.get("name"))
                    .and_then(|n| {
                        n.as_str()
                            .or_else(|| n.get("name").and_then(|x| x.as_str()))
                    })
                    .unwrap_or("unknown")
                    .to_string();

                let output = parsed
                    .get("output")
                    .or_else(|| parsed.get("result"))
                    .and_then(|o| o.as_str())
                    .unwrap_or("")
                    .to_string();

                AgentEvent::ToolResult {
                    tool,
                    output,
                    raw: parsed,
                }
            }
            "usage" => {
                let usage_obj = parsed.get("usage").unwrap_or(&parsed);
                let input_tokens = usage_obj
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let output_tokens = usage_obj
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let cached_tokens = usage_obj
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_u64());

                AgentEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cached_tokens,
                    raw: parsed,
                }
            }
            "result" => {
                let subtype = parsed.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
                let is_error = subtype == "error" || parsed.get("error").is_some();

                if is_error {
                    let err_msg = parsed
                        .get("error")
                        .and_then(|e| e.as_str())
                        .or_else(|| parsed.get("result").and_then(|r| r.as_str()))
                        .unwrap_or("Unknown result error")
                        .to_string();
                    AgentEvent::ResultError {
                        error: err_msg,
                        raw: parsed,
                    }
                } else {
                    let res_msg = parsed
                        .get("result")
                        .and_then(|r| r.as_str())
                        .unwrap_or("Execution completed")
                        .to_string();
                    AgentEvent::ResultSuccess {
                        result: res_msg,
                        raw: parsed,
                    }
                }
            }
            _ => AgentEvent::MalformedEvent {
                raw_line: trimmed.to_string(),
                error: format!("Unrecognized event type '{event_type}'"),
            },
        }
    }
}

#[async_trait]
impl WorkerAdapter for ClaudeCodeAdapter {
    fn provider_name(&self) -> &str {
        "claude-code"
    }

    async fn run_worker(
        &self,
        request: WorkerRequest,
        event_sender: Sender<AgentEvent>,
    ) -> Result<WorkerResult, AdapterError> {
        let started_at = Utc::now();
        debug!(
            "Starting Claude Code worker '{}' in {:?}",
            request.worker_name, request.worktree_dir
        );

        let mut cmd = Command::new(&self.binary_path);

        // Build explicit argument array - NEVER shell interpolate!
        cmd.arg("-p")
            .arg(&request.prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose");

        if let Some(turns) = request.max_turns {
            cmd.arg("--max-turns").arg(turns.to_string());
        }

        if let Some(budget) = request.max_budget_usd {
            cmd.arg("--max-budget-usd").arg(format!("{budget:.2}"));
        }

        cmd.current_dir(&request.worktree_dir);
        cmd.stdin(Stdio::null()); // Non-interactive: never hang on stdin
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true); // Terminate child if supervisor handle is dropped

        for (k, v) in &request.env_vars {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(AdapterError::Io)?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AdapterError::ExecutionFailed("Failed to capture stdout".into()))?;

        let mut reader = BufReader::new(stdout).lines();

        let mut observed_usage = ObservedUsage::default();
        let mut malformed_count = 0;
        let mut final_message = None;
        let mut child_success = false;

        let timeout_duration = Duration::from_secs(request.timeout_seconds);

        let process_future = async {
            while let Ok(Some(line)) = reader.next_line().await {
                let event = Self::parse_line(&line);

                match &event {
                    AgentEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cached_tokens,
                        ..
                    } => {
                        observed_usage.input_tokens += input_tokens;
                        observed_usage.output_tokens += output_tokens;
                        if let Some(cached) = cached_tokens {
                            observed_usage.cache_read_input_tokens =
                                Some(observed_usage.cache_read_input_tokens.unwrap_or(0) + cached);
                        }
                    }
                    AgentEvent::ResultSuccess { result, .. } => {
                        child_success = true;
                        final_message = Some(result.clone());
                    }
                    AgentEvent::ResultError { error, .. } => {
                        child_success = false;
                        final_message = Some(error.clone());
                    }
                    AgentEvent::MalformedEvent { raw_line, error } => {
                        malformed_count += 1;
                        warn!("Malformed event from Claude: {error} | line: {raw_line}");
                    }
                    _ => {}
                }

                let _ = event_sender.send(event).await;
            }

            child.wait().await
        };

        let status_result = tokio::time::timeout(timeout_duration, process_future).await;

        let exit_status = match status_result {
            Ok(wait_res) => wait_res.map_err(AdapterError::Io)?,
            Err(_) => {
                error!(
                    "Worker '{}' timed out after {}s",
                    request.worker_name, request.timeout_seconds
                );
                return Err(AdapterError::Timeout(request.timeout_seconds));
            }
        };

        let exit_code = exit_status.code();
        let completed_at = Utc::now();

        // Enforce strict schema validation: if malformed events occurred, flag it
        if malformed_count > 0 {
            return Err(AdapterError::MalformedEventError(format!(
                "Encountered {malformed_count} malformed JSONL stream events"
            )));
        }

        let is_success = exit_status.success() && (child_success || exit_code == Some(0));
        let model_name = request.model.as_deref().unwrap_or("claude-3-7-sonnet");
        let estimated_cost = observed_usage.calculate_estimated_cost(model_name);

        Ok(WorkerResult {
            exit_code,
            success: is_success,
            observed_usage,
            estimated_cost_usd: estimated_cost,
            final_message,
            malformed_event_count: malformed_count,
            started_at,
            completed_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_system_init() {
        let line = r#"{"type":"system","session_id":"sess-123","model":"claude-3-7-sonnet"}"#;
        let event = ClaudeCodeAdapter::parse_line(line);
        match event {
            AgentEvent::SystemInit {
                session_id, model, ..
            } => {
                assert_eq!(session_id, Some("sess-123".into()));
                assert_eq!(model, Some("claude-3-7-sonnet".into()));
            }
            _ => panic!("Expected SystemInit"),
        }
    }

    #[test]
    fn test_parse_tool_use_and_result() {
        let line_use = r#"{"type":"tool_use","tool":"write_file","input":{"path":"src/test.ts"}}"#;
        let event_use = ClaudeCodeAdapter::parse_line(line_use);
        match event_use {
            AgentEvent::ToolUse { tool, input, .. } => {
                assert_eq!(tool, "write_file");
                assert_eq!(input["path"], "src/test.ts");
            }
            _ => panic!("Expected ToolUse"),
        }

        let line_res = r#"{"type":"tool_result","tool":"write_file","output":"File written"}"#;
        let event_res = ClaudeCodeAdapter::parse_line(line_res);
        match event_res {
            AgentEvent::ToolResult { tool, output, .. } => {
                assert_eq!(tool, "write_file");
                assert_eq!(output, "File written");
            }
            _ => panic!("Expected ToolResult"),
        }
    }

    #[test]
    fn test_parse_usage() {
        let line = r#"{"type":"usage","usage":{"input_tokens":1500,"output_tokens":350,"cache_read_input_tokens":500}}"#;
        let event = ClaudeCodeAdapter::parse_line(line);
        match event {
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                cached_tokens,
                ..
            } => {
                assert_eq!(input_tokens, 1500);
                assert_eq!(output_tokens, 350);
                assert_eq!(cached_tokens, Some(500));
            }
            _ => panic!("Expected Usage"),
        }
    }

    #[test]
    fn test_parse_malformed_event() {
        let line = "Not a json object at all!";
        let event = ClaudeCodeAdapter::parse_line(line);
        match event {
            AgentEvent::MalformedEvent { raw_line, error } => {
                assert_eq!(raw_line, line);
                assert!(error.contains("JSON parse error"));
            }
            _ => panic!("Expected MalformedEvent"),
        }

        let unknown_type = r#"{"type":"alien_telemetry","data":"xyz"}"#;
        let event_unknown = ClaudeCodeAdapter::parse_line(unknown_type);
        match event_unknown {
            AgentEvent::MalformedEvent { error, .. } => {
                assert!(error.contains("Unrecognized event type"));
            }
            _ => panic!("Expected MalformedEvent"),
        }
    }
}
