use crate::context::WorkContext;
use crate::index::CodebaseIndex;
use crate::suggest::Suggestion;
use anyhow::Result;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResetOption {
    Index,
    Suggestions,
    Summaries,
    Glossary,
    Memory,
    GroupingAi,
    QuestionCache,
    PipelineMetrics,
    SuggestionQuality,
    ImplementationHarness,
}

#[derive(Debug, Clone)]
pub enum Command {
    LoadRepo {
        root: PathBuf,
    },
    RefreshSuggestions,
    PreviewSuggestion {
        suggestion_id: Uuid,
    },
    ApplySuggestion {
        suggestion_id: Uuid,
        preview_hash: String,
    },
    FixReviewFindings {
        finding_ids: Vec<Uuid>,
    },
    StartShip,
    ConfirmShip,
    AskQuestion {
        text: String,
    },
    UndoLastChange,
    ResetData {
        options: Vec<ResetOption>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Scan,
    Suggest,
    Preview,
    Apply,
    Review,
    Ship,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    Validation,
    Conflict,
    Auth,
    Network,
    Io,
    Internal,
}

#[derive(Debug, Clone)]
pub struct UiState {
    pub repo_root: PathBuf,
    pub context: WorkContext,
    pub index: CodebaseIndex,
    pub suggestions: Vec<Suggestion>,
}

#[derive(Debug, Clone)]
pub enum Event {
    StateUpdated(Box<UiState>),
    Toast {
        message: String,
        level: ToastLevel,
    },
    Progress {
        stage: Stage,
        done: usize,
        total: usize,
    },
    Error {
        code: ErrorCode,
        message: String,
        details: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct RepoSnapshot {
    pub root: PathBuf,
    pub index: CodebaseIndex,
    pub context: WorkContext,
}

#[derive(Debug, Clone)]
pub struct FixPreview {
    pub summary: String,
    pub outcome: String,
    pub files: Vec<PathBuf>,
    pub preview_hash: String,
}

#[derive(Debug, Clone)]
pub struct ApplyRequest {
    pub suggestion_id: Uuid,
    pub preview_hash: String,
}

#[derive(Debug, Clone)]
pub struct AppliedFile {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ApplyResult {
    pub description: String,
    pub files: Vec<AppliedFile>,
}

#[derive(Debug, Clone)]
pub struct ChangeSet {
    pub files: Vec<AppliedFile>,
}

#[derive(Debug, Clone)]
pub struct FixContext {
    pub problem_summary: String,
    pub outcome: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct ReviewFinding {
    pub id: Uuid,
    pub file: String,
    pub line: Option<usize>,
    pub severity: String,
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct ReviewReport {
    pub findings: Vec<ReviewFinding>,
    pub summary: String,
}

pub trait Engine {
    fn scan_and_suggest<'a>(
        &'a self,
        repo: &'a RepoSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Suggestion>>> + Send + 'a>>;

    fn build_preview<'a>(
        &'a self,
        repo: &'a RepoSnapshot,
        suggestion_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<FixPreview>> + Send + 'a>>;

    fn apply_with_harness<'a>(
        &'a self,
        repo: &'a RepoSnapshot,
        request: ApplyRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ApplyResult>> + Send + 'a>>;

    fn adversarial_review<'a>(
        &'a self,
        change_set: &'a ChangeSet,
        ctx: &'a FixContext,
    ) -> Pin<Box<dyn Future<Output = Result<ReviewReport>> + Send + 'a>>;
}
