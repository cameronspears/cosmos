//! Workflow state machine for the fix-and-ship process
//!
//! Tracks progress through: analyze -> fix -> test -> review -> commit -> PR

use crate::diff::UnifiedDiff;

/// The current state of the fix workflow
#[derive(Debug, Clone, Default)]
pub enum WorkflowState {
    /// No active workflow
    #[default]
    Idle,
    
    /// Generating a fix with AI
    GeneratingFix {
        file_path: String,
    },
    
    /// Reviewing the generated diff before applying
    ReviewingDiff {
        file_path: String,
        diff: UnifiedDiff,
        original_content: String,
    },
    
    /// Diff has been applied to the file
    Applied {
        file_path: String,
        original_content: String,
    },
    
    /// Running tests
    Testing {
        file_path: String,
    },
    
    /// Waiting for AI review of changes
    AwaitingReview {
        file_path: String,
    },
    
    /// Changes reviewed, ready to commit
    ReadyToCommit {
        files: Vec<String>,
        suggested_message: Option<String>,
    },
    
    /// Committed, ready to push
    ReadyToPush {
        branch: String,
        commit_sha: String,
    },
    
    /// PR created
    Complete {
        pr_url: String,
    },
    
    /// Something went wrong
    Error {
        message: String,
        can_retry: bool,
    },
}

impl WorkflowState {
    /// Human-readable status for display
    pub fn status_text(&self) -> &'static str {
        match self {
            WorkflowState::Idle => "Ready",
            WorkflowState::GeneratingFix { .. } => "Generating fix...",
            WorkflowState::ReviewingDiff { .. } => "Review diff",
            WorkflowState::Applied { .. } => "Applied",
            WorkflowState::Testing { .. } => "Testing...",
            WorkflowState::AwaitingReview { .. } => "AI reviewing...",
            WorkflowState::ReadyToCommit { .. } => "Ready to commit",
            WorkflowState::ReadyToPush { .. } => "Ready to push",
            WorkflowState::Complete { .. } => "Complete!",
            WorkflowState::Error { .. } => "Error",
        }
    }
    
    /// Get the current file being worked on, if any
    pub fn current_file(&self) -> Option<&str> {
        match self {
            WorkflowState::GeneratingFix { file_path } => Some(file_path),
            WorkflowState::ReviewingDiff { file_path, .. } => Some(file_path),
            WorkflowState::Applied { file_path, .. } => Some(file_path),
            WorkflowState::Testing { file_path } => Some(file_path),
            WorkflowState::AwaitingReview { file_path } => Some(file_path),
            WorkflowState::ReadyToCommit { files, .. } => files.first().map(|s| s.as_str()),
            _ => None,
        }
    }
    
    /// Check if workflow is in an active state
    pub fn is_active(&self) -> bool {
        !matches!(self, WorkflowState::Idle | WorkflowState::Complete { .. } | WorkflowState::Error { .. })
    }
    
    /// Check if user can cancel the current operation
    pub fn is_cancelable(&self) -> bool {
        matches!(
            self,
            WorkflowState::ReviewingDiff { .. }
                | WorkflowState::Applied { .. }
                | WorkflowState::ReadyToCommit { .. }
                | WorkflowState::ReadyToPush { .. }
        )
    }
    
    /// Get available actions for the current state
    pub fn available_actions(&self) -> Vec<WorkflowAction> {
        match self {
            WorkflowState::Idle => vec![WorkflowAction::StartFix],
            WorkflowState::GeneratingFix { .. } => vec![],
            WorkflowState::ReviewingDiff { .. } => vec![
                WorkflowAction::ApplyDiff,
                WorkflowAction::Cancel,
            ],
            WorkflowState::Applied { .. } => vec![
                WorkflowAction::RunTests,
                WorkflowAction::RequestReview,
                WorkflowAction::Commit,
                WorkflowAction::Revert,
            ],
            WorkflowState::Testing { .. } => vec![],
            WorkflowState::AwaitingReview { .. } => vec![],
            WorkflowState::ReadyToCommit { .. } => vec![
                WorkflowAction::Commit,
                WorkflowAction::Cancel,
            ],
            WorkflowState::ReadyToPush { .. } => vec![
                WorkflowAction::Push,
                WorkflowAction::CreatePR,
                WorkflowAction::Cancel,
            ],
            WorkflowState::Complete { .. } => vec![
                WorkflowAction::OpenPR,
                WorkflowAction::Reset,
            ],
            WorkflowState::Error { can_retry, .. } => {
                let mut actions = vec![WorkflowAction::Reset];
                if *can_retry {
                    actions.insert(0, WorkflowAction::Retry);
                }
                actions
            }
        }
    }
}

/// Actions that can be taken during the workflow
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowAction {
    StartFix,
    ApplyDiff,
    RunTests,
    RequestReview,
    Commit,
    Push,
    CreatePR,
    OpenPR,
    Revert,
    Cancel,
    Reset,
    Retry,
}

impl WorkflowAction {
    /// Keyboard shortcut for this action
    pub fn key(&self) -> char {
        match self {
            WorkflowAction::StartFix => 'a',
            WorkflowAction::ApplyDiff => 'y',
            WorkflowAction::RunTests => 't',
            WorkflowAction::RequestReview => 'r',
            WorkflowAction::Commit => 'c',
            WorkflowAction::Push => 'p',
            WorkflowAction::CreatePR => 'P',
            WorkflowAction::OpenPR => 'o',
            WorkflowAction::Revert => 'z',
            WorkflowAction::Cancel => 'q',
            WorkflowAction::Reset => 'R',
            WorkflowAction::Retry => 'r',
        }
    }
    
    /// Human-readable label
    pub fn label(&self) -> &'static str {
        match self {
            WorkflowAction::StartFix => "AI Fix",
            WorkflowAction::ApplyDiff => "Apply",
            WorkflowAction::RunTests => "Test",
            WorkflowAction::RequestReview => "Review",
            WorkflowAction::Commit => "Commit",
            WorkflowAction::Push => "Push",
            WorkflowAction::CreatePR => "Create PR",
            WorkflowAction::OpenPR => "Open PR",
            WorkflowAction::Revert => "Revert",
            WorkflowAction::Cancel => "Cancel",
            WorkflowAction::Reset => "Reset",
            WorkflowAction::Retry => "Retry",
        }
    }
}

/// Workflow manager that handles state transitions
#[derive(Debug, Default)]
pub struct Workflow {
    pub state: WorkflowState,
    /// History of files fixed in this session
    pub fixed_files: Vec<String>,
}

impl Workflow {
    pub fn new() -> Self {
        Self::default()
    }
    
    /// Start a fix workflow for a file
    pub fn start_fix(&mut self, file_path: String) {
        self.state = WorkflowState::GeneratingFix { file_path };
    }
    
    /// Set the generated diff for review
    pub fn set_diff(&mut self, file_path: String, diff: UnifiedDiff, original_content: String) {
        self.state = WorkflowState::ReviewingDiff {
            file_path,
            diff,
            original_content,
        };
    }
    
    /// Mark diff as applied
    pub fn mark_applied(&mut self, file_path: String, original_content: String) {
        self.state = WorkflowState::Applied {
            file_path,
            original_content,
        };
    }
    
    /// Start testing
    pub fn start_testing(&mut self, file_path: String) {
        self.state = WorkflowState::Testing { file_path };
    }
    
    /// Start AI review
    pub fn start_review(&mut self, file_path: String) {
        self.state = WorkflowState::AwaitingReview { file_path };
    }
    
    /// Mark ready to commit
    pub fn ready_to_commit(&mut self, files: Vec<String>, suggested_message: Option<String>) {
        self.state = WorkflowState::ReadyToCommit {
            files,
            suggested_message,
        };
    }
    
    /// Mark ready to push
    pub fn ready_to_push(&mut self, branch: String, commit_sha: String) {
        self.state = WorkflowState::ReadyToPush { branch, commit_sha };
    }
    
    /// Mark workflow complete
    pub fn complete(&mut self, pr_url: String) {
        self.state = WorkflowState::Complete { pr_url };
    }
    
    /// Set error state
    pub fn error(&mut self, message: String, can_retry: bool) {
        self.state = WorkflowState::Error { message, can_retry };
    }
    
    /// Reset to idle
    pub fn reset(&mut self) {
        self.state = WorkflowState::Idle;
    }
    
    /// Record a fixed file
    pub fn record_fix(&mut self, file_path: String) {
        if !self.fixed_files.contains(&file_path) {
            self.fixed_files.push(file_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workflow_transitions() {
        let mut workflow = Workflow::new();
        assert!(matches!(workflow.state, WorkflowState::Idle));
        
        workflow.start_fix("test.rs".to_string());
        assert!(workflow.state.is_active());
        assert_eq!(workflow.state.current_file(), Some("test.rs"));
    }
}

