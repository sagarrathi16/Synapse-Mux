pub mod claude;
pub mod mock;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AdapterError {
    #[error("Adapter execution failed: {0}")]
    ExecutionFailed(String),
    #[error("Process timed out after {0} seconds")]
    Timeout(u64),
    #[error("Process exited with non-zero code: {0:?}")]
    NonZeroExit(Option<i32>),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization / parse error: {0}")]
    ParseError(String),
    #[error("Worker encountered fatal malformed events: {0}")]
    MalformedEventError(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObservedUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

impl ObservedUsage {
    pub fn calculate_estimated_cost(&self, model: &str) -> f64 {
        // Versioned local pricing table: "2026-10-pricing-v1"
        // Claude 3.7 Sonnet pricing: $3.00 / 1M input, $15.00 / 1M output, $0.30 / 1M cache read
        let (input_rate, output_rate, cache_rate) = if model.contains("opus") {
            (15.0 / 1_000_000.0, 75.0 / 1_000_000.0, 1.50 / 1_000_000.0)
        } else {
            (3.0 / 1_000_000.0, 15.0 / 1_000_000.0, 0.30 / 1_000_000.0)
        };

        let normal_input = self.input_tokens as f64 * input_rate;
        let normal_output = self.output_tokens as f64 * output_rate;
        let cache_input = self.cache_read_input_tokens.unwrap_or(0) as f64 * cache_rate;

        normal_input + normal_output + cache_input
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum AgentEvent {
    SystemInit {
        session_id: Option<String>,
        model: Option<String>,
        raw: serde_json::Value,
    },
    AssistantMessage {
        content: String,
        raw: serde_json::Value,
    },
    ToolUse {
        tool: String,
        input: serde_json::Value,
        raw: serde_json::Value,
    },
    ToolResult {
        tool: String,
        output: String,
        raw: serde_json::Value,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: Option<u64>,
        raw: serde_json::Value,
    },
    ResultSuccess {
        result: String,
        raw: serde_json::Value,
    },
    ResultError {
        error: String,
        raw: serde_json::Value,
    },
    MalformedEvent {
        raw_line: String,
        error: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRequest {
    pub run_id: String,
    pub worker_name: String,
    pub prompt: String,
    pub worktree_dir: PathBuf,
    pub timeout_seconds: u64,
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerResult {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub observed_usage: ObservedUsage,
    pub estimated_cost_usd: f64,
    pub final_message: Option<String>,
    pub malformed_event_count: usize,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
}

#[async_trait]
pub trait WorkerAdapter: Send + Sync {
    fn provider_name(&self) -> &str;

    async fn run_worker(
        &self,
        request: WorkerRequest,
        event_sender: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<WorkerResult, AdapterError>;
}
