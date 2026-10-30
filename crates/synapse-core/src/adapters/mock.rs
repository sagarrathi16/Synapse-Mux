use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::mpsc::Sender;

use super::{
    AdapterError, AgentEvent, ObservedUsage, WorkerAdapter, WorkerRequest, WorkerResult,
    claude::ClaudeCodeAdapter,
};

pub struct MockAdapter {
    fixture_path: Option<PathBuf>,
    simulate_failure: bool,
    simulate_timeout: bool,
    write_files: Vec<(String, String)>,
    observed_usage: ObservedUsage,
}

impl Default for MockAdapter {
    fn default() -> Self {
        Self {
            fixture_path: None,
            simulate_failure: false,
            simulate_timeout: false,
            write_files: Vec::new(),
            observed_usage: ObservedUsage {
                input_tokens: 1000,
                output_tokens: 250,
                cache_read_input_tokens: Some(100),
                cache_creation_input_tokens: None,
            },
        }
    }
}

impl MockAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_fixture(mut self, fixture_path: impl Into<PathBuf>) -> Self {
        self.fixture_path = Some(fixture_path.into());
        self
    }

    pub fn with_file_write(
        mut self,
        rel_path: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        self.write_files.push((rel_path.into(), content.into()));
        self
    }

    pub fn with_failure(mut self, simulate: bool) -> Self {
        self.simulate_failure = simulate;
        self
    }

    pub fn with_timeout(mut self, simulate: bool) -> Self {
        self.simulate_timeout = simulate;
        self
    }
}

#[async_trait]
impl WorkerAdapter for MockAdapter {
    fn provider_name(&self) -> &str {
        "mock-worker"
    }

    async fn run_worker(
        &self,
        request: WorkerRequest,
        event_sender: Sender<AgentEvent>,
    ) -> Result<WorkerResult, AdapterError> {
        let started_at = Utc::now();

        if self.simulate_timeout {
            tokio::time::sleep(Duration::from_millis(50)).await;
            return Err(AdapterError::Timeout(request.timeout_seconds));
        }

        // Apply mock file changes in worktree
        for (rel_path, content) in &self.write_files {
            let full_path = request.worktree_dir.join(rel_path);
            if let Some(parent) = full_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            fs::write(&full_path, content)?;
        }

        let mut malformed_count = 0;

        // Stream fixture lines if provided
        if let Some(ref path) = self.fixture_path {
            if path.exists() {
                let data = fs::read_to_string(path)?;
                for line in data.lines() {
                    let event = ClaudeCodeAdapter::parse_line(line);
                    if matches!(event, AgentEvent::MalformedEvent { .. }) {
                        malformed_count += 1;
                    }
                    let _ = event_sender.send(event).await;
                }
            }
        } else {
            // Default simulated events
            let _ = event_sender
                .send(AgentEvent::SystemInit {
                    session_id: Some("mock-session".into()),
                    model: Some("mock-model".into()),
                    raw: serde_json::json!({"type": "system"}),
                })
                .await;

            let _ = event_sender
                .send(AgentEvent::AssistantMessage {
                    content: "Performing mock operations".into(),
                    raw: serde_json::json!({"type": "assistant"}),
                })
                .await;

            let _ = event_sender
                .send(AgentEvent::Usage {
                    input_tokens: self.observed_usage.input_tokens,
                    output_tokens: self.observed_usage.output_tokens,
                    cached_tokens: self.observed_usage.cache_read_input_tokens,
                    raw: serde_json::json!({"type": "usage"}),
                })
                .await;

            if self.simulate_failure {
                let _ = event_sender
                    .send(AgentEvent::ResultError {
                        error: "Simulated worker failure".into(),
                        raw: serde_json::json!({"type": "result", "subtype": "error"}),
                    })
                    .await;
            } else {
                let _ = event_sender
                    .send(AgentEvent::ResultSuccess {
                        result: "Simulated completion".into(),
                        raw: serde_json::json!({"type": "result", "subtype": "success"}),
                    })
                    .await;
            }
        }

        if malformed_count > 0 {
            return Err(AdapterError::MalformedEventError(format!(
                "Encountered {malformed_count} malformed stream events"
            )));
        }

        let completed_at = Utc::now();
        let success = !self.simulate_failure;
        let exit_code = if success { Some(0) } else { Some(1) };
        let estimated_cost = self.observed_usage.calculate_estimated_cost("mock-model");

        Ok(WorkerResult {
            exit_code,
            success,
            observed_usage: self.observed_usage.clone(),
            estimated_cost_usd: estimated_cost,
            final_message: if success {
                Some("Completed successfully".into())
            } else {
                Some("Failed".into())
            },
            malformed_event_count: malformed_count,
            started_at,
            completed_at,
        })
    }
}
