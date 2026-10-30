use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum StateError {
    #[error("Invalid state transition from {from:?} to {to:?}: {reason}")]
    InvalidTransition {
        from: RunState,
        to: RunState,
        reason: String,
    },
    #[error("IO error in run store: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Run state file not found at {0}")]
    StateNotFound(PathBuf),
    #[error("Run already in terminal state: {0:?}")]
    TerminalState(RunState),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunState {
    Preflight,
    ContractProposal,
    ContractApproval,
    Implementation,
    Checkpoint,
    Integration,
    Verification,
    Repair,
    ReviewReady,
    Failed,
    Cancelled,
    NeedsUser,
}

impl RunState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Failed | Self::Cancelled | Self::ReviewReady)
    }

    pub fn can_transition_to(&self, next: &RunState) -> Result<(), StateError> {
        if self == next {
            return Ok(());
        }

        // Terminal states cannot transition to anything
        if matches!(self, Self::Failed | Self::Cancelled) {
            return Err(StateError::TerminalState(*self));
        }

        // Terminal failure / cancellation / user pause can happen from any non-terminal state
        if matches!(next, Self::Failed | Self::Cancelled | Self::NeedsUser) {
            return Ok(());
        }

        // NeedsUser can resume to where it was or to repair/preflight
        if *self == Self::NeedsUser {
            return Ok(());
        }

        let valid = match self {
            Self::Preflight => matches!(
                next,
                Self::ContractProposal | Self::ContractApproval | Self::Implementation
            ),
            Self::ContractProposal => {
                matches!(next, Self::ContractApproval | Self::ContractProposal)
            }
            Self::ContractApproval => {
                matches!(next, Self::Implementation | Self::ContractProposal)
            }
            Self::Implementation => matches!(next, Self::Checkpoint),
            Self::Checkpoint => matches!(
                next,
                Self::Integration | Self::Verification | Self::Implementation
            ),
            Self::Integration => matches!(next, Self::Verification | Self::Repair),
            Self::Verification => matches!(next, Self::ReviewReady | Self::Repair),
            Self::Repair => matches!(
                next,
                Self::Implementation | Self::Integration | Self::Verification
            ),
            Self::ReviewReady => matches!(next, Self::Repair),
            Self::NeedsUser => true,
            Self::Failed | Self::Cancelled => false,
        };

        if valid {
            Ok(())
        } else {
            Err(StateError::InvalidTransition {
                from: *self,
                to: *next,
                reason: format!("Transition from {self} to {next} is disallowed by state machine"),
            })
        }
    }
}

impl fmt::Display for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preflight => write!(f, "PREFLIGHT"),
            Self::ContractProposal => write!(f, "CONTRACT_PROPOSAL"),
            Self::ContractApproval => write!(f, "CONTRACT_APPROVAL"),
            Self::Implementation => write!(f, "IMPLEMENTATION"),
            Self::Checkpoint => write!(f, "CHECKPOINT"),
            Self::Integration => write!(f, "INTEGRATION"),
            Self::Verification => write!(f, "VERIFICATION"),
            Self::Repair => write!(f, "REPAIR"),
            Self::ReviewReady => write!(f, "REVIEW_READY"),
            Self::Failed => write!(f, "FAILED"),
            Self::Cancelled => write!(f, "CANCELLED"),
            Self::NeedsUser => write!(f, "NEEDS_USER"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "details")]
pub enum TransitionOutcome {
    Success,
    Failure(String),
    Warning(String),
    UserActionRequired(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateTransitionRecord {
    pub transition_id: String,
    pub run_id: String,
    pub timestamp: DateTime<Utc>,
    pub from_state: RunState,
    pub to_state: RunState,
    pub input_artifact_ids: Vec<String>,
    pub outcome: TransitionOutcome,
    pub diagnostic_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStateSnapshot {
    pub run_id: String,
    pub current_state: RunState,
    pub base_commit: Option<String>,
    pub contract_hash: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct RunStore {
    root_dir: PathBuf,
    run_id: String,
    run_dir: PathBuf,
    artifacts_dir: PathBuf,
}

impl RunStore {
    pub fn new(root_dir: impl AsRef<Path>, run_id: impl Into<String>) -> Result<Self, StateError> {
        let run_id = run_id.into();
        let root_dir = root_dir.as_ref().to_path_buf();
        let run_dir = root_dir.join(".synapse").join("runs").join(&run_id);
        let artifacts_dir = run_dir.join("artifacts");

        fs::create_dir_all(&artifacts_dir)?;

        Ok(Self {
            root_dir,
            run_id,
            run_dir,
            artifacts_dir,
        })
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    pub fn artifacts_dir(&self) -> &Path {
        &self.artifacts_dir
    }

    pub fn state_file_path(&self) -> PathBuf {
        self.run_dir.join("state.json")
    }

    pub fn transitions_file_path(&self) -> PathBuf {
        self.run_dir.join("transitions.jsonl")
    }

    pub fn initialize(
        &self,
        base_commit: Option<String>,
        contract_hash: Option<String>,
    ) -> Result<RunStateSnapshot, StateError> {
        let now = Utc::now();
        let snapshot = RunStateSnapshot {
            run_id: self.run_id.clone(),
            current_state: RunState::Preflight,
            base_commit,
            contract_hash,
            created_at: now,
            updated_at: now,
            metadata: serde_json::json!({}),
        };

        self.save_state_snapshot(&snapshot)?;

        // Record initial transition from Preflight to Preflight (init)
        let init_transition = StateTransitionRecord {
            transition_id: Uuid::new_v4().to_string(),
            run_id: self.run_id.clone(),
            timestamp: now,
            from_state: RunState::Preflight,
            to_state: RunState::Preflight,
            input_artifact_ids: vec![],
            outcome: TransitionOutcome::Success,
            diagnostic_summary: Some("Run initialized in PREFLIGHT state".into()),
        };
        self.append_transition(&init_transition)?;

        Ok(snapshot)
    }

    pub fn load_state_snapshot(&self) -> Result<RunStateSnapshot, StateError> {
        let path = self.state_file_path();
        if !path.exists() {
            return Err(StateError::StateNotFound(path));
        }
        let data = fs::read_to_string(&path)?;
        let snapshot: RunStateSnapshot = serde_json::from_str(&data)?;
        Ok(snapshot)
    }

    pub fn save_state_snapshot(&self, snapshot: &RunStateSnapshot) -> Result<(), StateError> {
        let path = self.state_file_path();
        let tmp_path = self.run_dir.join("state.json.tmp");

        let serialized = serde_json::to_string_pretty(snapshot)?;
        fs::write(&tmp_path, serialized)?;
        fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    pub fn transition_to(
        &self,
        to_state: RunState,
        input_artifact_ids: Vec<String>,
        outcome: TransitionOutcome,
        diagnostic_summary: Option<String>,
    ) -> Result<RunStateSnapshot, StateError> {
        let mut current = self.load_state_snapshot()?;
        current.current_state.can_transition_to(&to_state)?;

        let from_state = current.current_state;
        let now = Utc::now();

        let record = StateTransitionRecord {
            transition_id: Uuid::new_v4().to_string(),
            run_id: self.run_id.clone(),
            timestamp: now,
            from_state,
            to_state,
            input_artifact_ids,
            outcome,
            diagnostic_summary,
        };

        self.append_transition(&record)?;

        current.current_state = to_state;
        current.updated_at = now;
        self.save_state_snapshot(&current)?;

        Ok(current)
    }

    pub fn append_transition(&self, record: &StateTransitionRecord) -> Result<(), StateError> {
        let transitions_path = self.transitions_file_path();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(transitions_path)?;

        let json_line = serde_json::to_string(record)?;
        writeln!(file, "{json_line}")?;
        file.flush()?;
        Ok(())
    }

    pub fn list_transitions(&self) -> Result<Vec<StateTransitionRecord>, StateError> {
        let path = self.transitions_file_path();
        if !path.exists() {
            return Ok(vec![]);
        }

        let content = fs::read_to_string(&path)?;
        let mut records = Vec::new();
        for line in content.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                let rec: StateTransitionRecord = serde_json::from_str(trimmed)?;
                records.push(rec);
            }
        }
        Ok(records)
    }

    pub fn save_artifact(&self, filename: &str, content: &[u8]) -> Result<PathBuf, StateError> {
        let path = self.artifacts_dir.join(filename);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, content)?;
        Ok(path)
    }

    pub fn get_artifact_path(&self, filename: &str) -> PathBuf {
        self.artifacts_dir.join(filename)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_valid_state_transitions() {
        assert!(
            RunState::Preflight
                .can_transition_to(&RunState::Implementation)
                .is_ok()
        );
        assert!(
            RunState::Implementation
                .can_transition_to(&RunState::Checkpoint)
                .is_ok()
        );
        assert!(
            RunState::Checkpoint
                .can_transition_to(&RunState::Verification)
                .is_ok()
        );
        assert!(
            RunState::Verification
                .can_transition_to(&RunState::ReviewReady)
                .is_ok()
        );
    }

    #[test]
    fn test_invalid_state_transitions() {
        assert!(
            RunState::Preflight
                .can_transition_to(&RunState::ReviewReady)
                .is_err()
        );
        assert!(
            RunState::Failed
                .can_transition_to(&RunState::Preflight)
                .is_err()
        );
        assert!(
            RunState::Cancelled
                .can_transition_to(&RunState::Implementation)
                .is_err()
        );
    }

    #[test]
    fn test_run_store_lifecycle() {
        let temp = TempDir::new().unwrap();
        let store = RunStore::new(temp.path(), "test-run-1").unwrap();

        let snapshot = store.initialize(Some("abcdef".into()), None).unwrap();
        assert_eq!(snapshot.current_state, RunState::Preflight);
        assert_eq!(snapshot.base_commit, Some("abcdef".into()));

        let updated = store
            .transition_to(
                RunState::Implementation,
                vec!["spec-hash-1".into()],
                TransitionOutcome::Success,
                Some("Started worker".into()),
            )
            .unwrap();
        assert_eq!(updated.current_state, RunState::Implementation);

        let transitions = store.list_transitions().unwrap();
        assert_eq!(transitions.len(), 2);
        assert_eq!(transitions[1].from_state, RunState::Preflight);
        assert_eq!(transitions[1].to_state, RunState::Implementation);

        // Test artifact save
        let art_path = store.save_artifact("test.txt", b"hello world").unwrap();
        assert!(art_path.exists());
        assert_eq!(fs::read_to_string(art_path).unwrap(), "hello world");
    }
}
