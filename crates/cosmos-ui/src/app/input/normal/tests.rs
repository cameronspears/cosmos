use super::*;
use cosmos_core::context::WorkContext;
use cosmos_core::index::CodebaseIndex;
use cosmos_core::suggest::SuggestionEngine;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use git2::{Repository, Signature};
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::tempdir;

// ========================================================================
// ApplyError User Message Tests
// ========================================================================

#[test]
fn test_apply_error_apply_not_confirmed() {
    let err = ApplyError::ApplyNotConfirmed;
    let msg = err.user_message();
    assert!(msg.contains("scope preview"));
}

#[test]
fn test_apply_error_already_applying() {
    let err = ApplyError::AlreadyApplying;
    let msg = err.user_message();
    assert!(msg.contains("in progress"));
    assert!(msg.contains("failed")); // Must contain for toast visibility
}

#[test]
fn test_apply_error_suggestion_not_validated() {
    let err = ApplyError::SuggestionNotValidated;
    let msg = err.user_message();
    assert!(msg.contains("validated"));
    assert!(msg.contains("failed"));
}

#[test]
fn test_apply_error_suggestion_not_found() {
    let err = ApplyError::SuggestionNotFound;
    let msg = err.user_message();
    assert!(msg.contains("no longer exists"));
    assert!(msg.contains("failed")); // Must contain for toast visibility
}

#[test]
fn test_apply_error_git_status_failed() {
    let err = ApplyError::GitStatusFailed("repository not found".into());
    let msg = err.user_message();
    assert!(msg.contains("Git error"));
    assert!(msg.contains("repository not found"));
}

#[test]
fn test_apply_error_dirty_working_tree() {
    let err = ApplyError::DirtyWorkingTree;
    let msg = err.user_message();
    assert!(msg.contains("changes"));
    assert!(msg.contains("stash"));
    assert!(msg.contains("failed")); // Must contain for toast visibility
}

#[test]
fn test_apply_error_files_changed_single() {
    let err = ApplyError::FilesChanged(vec![PathBuf::from("src/main.rs")]);
    let msg = err.user_message();
    assert!(msg.contains("files changed"));
    assert!(msg.contains("src/main.rs"));
    assert!(msg.contains("Refresh suggestions"));
    assert!(msg.contains("failed")); // Must contain for toast visibility
}

#[test]
fn test_apply_error_files_changed_multiple() {
    let err = ApplyError::FilesChanged(vec![
        PathBuf::from("a.rs"),
        PathBuf::from("b.rs"),
        PathBuf::from("c.rs"),
        PathBuf::from("d.rs"),
    ]);
    let msg = err.user_message();
    assert!(msg.contains("a.rs"));
    assert!(msg.contains("b.rs"));
    assert!(msg.contains("c.rs"));
    assert!(msg.contains("+1 more"));
    assert!(msg.contains("failed")); // Must contain for toast visibility
}

#[test]
fn test_apply_error_unsafe_path() {
    let err = ApplyError::UnsafePath(PathBuf::from("../evil"), "path traversal".into());
    let msg = err.user_message();
    assert!(msg.contains("unsafe path"));
    assert!(msg.contains("../evil"));
    assert!(msg.contains("failed")); // Must contain for toast visibility
}

#[test]
fn test_apply_error_file_read_failed() {
    let err = ApplyError::FileReadFailed(PathBuf::from("missing.rs"), "not found".into());
    let msg = err.user_message();
    assert!(msg.contains("couldn't read"));
    assert!(msg.contains("missing.rs"));
    assert!(msg.contains("failed")); // Must contain for toast visibility
}

// ========================================================================
// ApplyError Debug Trait Tests
// ========================================================================

#[test]
fn test_apply_error_is_debug() {
    // Ensure Debug trait is implemented for logging
    let err = ApplyError::ApplyNotConfirmed;
    let debug_str = format!("{:?}", err);
    assert!(debug_str.contains("ApplyNotConfirmed"));
}

#[test]
fn test_apply_error_is_clone() {
    // Ensure Clone trait is implemented
    let err = ApplyError::DirtyWorkingTree;
    let cloned = err.clone();
    assert_eq!(err.user_message(), cloned.user_message());
}

#[test]
fn enter_is_blocked_while_suggestion_refinement_in_progress() {
    let mut root = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    root.push(format!("cosmos_normal_mode_test_{}", nanos));
    std::fs::create_dir_all(&root).unwrap();

    let index = CodebaseIndex {
        root: root.clone(),
        files: HashMap::new(),
        index_errors: Vec::new(),
        git_head: Some("deadbeef".to_string()),
    };
    let suggestions = SuggestionEngine::new(index.clone());
    let context = WorkContext {
        branch: "main".to_string(),
        uncommitted_files: Vec::new(),
        staged_files: Vec::new(),
        untracked_files: Vec::new(),
        inferred_focus: None,
        modified_count: 0,
        repo_root: root.clone(),
    };
    let mut app = App::new(index.clone(), suggestions, context);
    app.active_panel = ActivePanel::Suggestions;
    app.workflow_step = WorkflowStep::Suggestions;
    app.suggestion_refinement_in_progress = true;

    let (tx, _rx) = mpsc::channel();
    let ctx = crate::app::RuntimeContext {
        index: &index,
        repo_path: &root,
        tx: &tx,
    };

    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    handle_normal_mode(&mut app, key, &ctx).unwrap();

    let toast = app
        .toast
        .as_ref()
        .map(|t| t.message.clone())
        .unwrap_or_default();
    assert!(toast.contains("still refining"));
    assert_eq!(app.workflow_step, WorkflowStep::Suggestions);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn k_opens_api_key_overlay() {
    let mut root = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    root.push(format!("cosmos_api_key_overlay_test_{}", nanos));
    std::fs::create_dir_all(&root).unwrap();

    let index = CodebaseIndex {
        root: root.clone(),
        files: HashMap::new(),
        index_errors: Vec::new(),
        git_head: Some("deadbeef".to_string()),
    };
    let suggestions = SuggestionEngine::new(index.clone());
    let context = WorkContext {
        branch: "main".to_string(),
        uncommitted_files: Vec::new(),
        staged_files: Vec::new(),
        untracked_files: Vec::new(),
        inferred_focus: None,
        modified_count: 0,
        repo_root: root.clone(),
    };
    let mut app = App::new(index.clone(), suggestions, context);
    app.active_panel = ActivePanel::Suggestions;
    app.workflow_step = WorkflowStep::Suggestions;

    let (tx, _rx) = mpsc::channel();
    let ctx = crate::app::RuntimeContext {
        index: &index,
        repo_path: &root,
        tx: &tx,
    };

    handle_normal_mode(
        &mut app,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
        &ctx,
    )
    .unwrap();

    assert!(matches!(app.overlay, Overlay::ApiKeySetup { .. }));

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn enter_opens_apply_plan_without_mutation() {
    std::env::set_var("OPENROUTER_API_KEY", "test-key");
    let mut root = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    root.push(format!("cosmos_apply_arm_test_{}", nanos));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn demo() {}\n").unwrap();

    let index = CodebaseIndex {
        root: root.clone(),
        files: HashMap::new(),
        index_errors: Vec::new(),
        git_head: Some("deadbeef".to_string()),
    };
    let mut suggestions = SuggestionEngine::new(index.clone());
    let suggestion = cosmos_core::suggest::Suggestion::new(
        cosmos_core::suggest::SuggestionKind::Improvement,
        cosmos_core::suggest::Priority::High,
        PathBuf::from("src/lib.rs"),
        "Improve demo".to_string(),
        cosmos_core::suggest::SuggestionSource::LlmDeep,
    )
    .with_validation_state(cosmos_core::suggest::SuggestionValidationState::Validated)
    .with_line(1);
    let suggestion_id = suggestion.id;
    suggestions.suggestions.push(suggestion);

    let context = WorkContext {
        branch: "main".to_string(),
        uncommitted_files: Vec::new(),
        staged_files: Vec::new(),
        untracked_files: Vec::new(),
        inferred_focus: None,
        modified_count: 0,
        repo_root: root.clone(),
    };
    let mut app = App::new(index.clone(), suggestions, context);
    app.active_panel = ActivePanel::Suggestions;
    app.workflow_step = WorkflowStep::Suggestions;

    let (tx, _rx) = mpsc::channel();
    let ctx = crate::app::RuntimeContext {
        index: &index,
        repo_path: &root,
        tx: &tx,
    };

    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    handle_normal_mode(&mut app, key, &ctx).unwrap();

    assert_eq!(app.armed_suggestion_id, Some(suggestion_id));
    assert!(!app.armed_file_hashes.is_empty());
    assert!(matches!(app.overlay, Overlay::ApplyPlan { .. }));
    assert_eq!(app.loading, LoadingState::None);
    assert!(app.pending_changes.is_empty());

    std::env::remove_var("OPENROUTER_API_KEY");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn apply_arm_resets_on_selection_change() {
    std::env::set_var("OPENROUTER_API_KEY", "test-key");
    let mut root = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    root.push(format!("cosmos_apply_arm_reset_test_{}", nanos));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.join("src/b.rs"), "fn b() {}\n").unwrap();

    let index = CodebaseIndex {
        root: root.clone(),
        files: HashMap::new(),
        index_errors: Vec::new(),
        git_head: Some("deadbeef".to_string()),
    };
    let mut suggestions = SuggestionEngine::new(index.clone());
    suggestions.suggestions.push(
        cosmos_core::suggest::Suggestion::new(
            cosmos_core::suggest::SuggestionKind::Improvement,
            cosmos_core::suggest::Priority::High,
            PathBuf::from("src/a.rs"),
            "Improve A".to_string(),
            cosmos_core::suggest::SuggestionSource::LlmDeep,
        )
        .with_validation_state(cosmos_core::suggest::SuggestionValidationState::Validated)
        .with_line(1),
    );
    suggestions.suggestions.push(
        cosmos_core::suggest::Suggestion::new(
            cosmos_core::suggest::SuggestionKind::Improvement,
            cosmos_core::suggest::Priority::High,
            PathBuf::from("src/b.rs"),
            "Improve B".to_string(),
            cosmos_core::suggest::SuggestionSource::LlmDeep,
        )
        .with_validation_state(cosmos_core::suggest::SuggestionValidationState::Validated)
        .with_line(1),
    );

    let context = WorkContext {
        branch: "main".to_string(),
        uncommitted_files: Vec::new(),
        staged_files: Vec::new(),
        untracked_files: Vec::new(),
        inferred_focus: None,
        modified_count: 0,
        repo_root: root.clone(),
    };
    let mut app = App::new(index.clone(), suggestions, context);
    app.active_panel = ActivePanel::Suggestions;
    app.workflow_step = WorkflowStep::Suggestions;

    let (tx, _rx) = mpsc::channel();
    let ctx = crate::app::RuntimeContext {
        index: &index,
        repo_path: &root,
        tx: &tx,
    };

    handle_normal_mode(
        &mut app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &ctx,
    )
    .unwrap();
    assert!(app.armed_suggestion_id.is_some());

    app.navigate_down();
    assert!(app.armed_suggestion_id.is_none());
    assert!(app.armed_file_hashes.is_empty());

    std::env::remove_var("OPENROUTER_API_KEY");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn apply_plan_cancel_clears_apply_confirmation() {
    std::env::set_var("OPENROUTER_API_KEY", "test-key");
    let mut root = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    root.push(format!("cosmos_apply_arm_esc_test_{}", nanos));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn demo() {}\n").unwrap();

    let index = CodebaseIndex {
        root: root.clone(),
        files: HashMap::new(),
        index_errors: Vec::new(),
        git_head: Some("deadbeef".to_string()),
    };
    let mut suggestions = SuggestionEngine::new(index.clone());
    suggestions.suggestions.push(
        cosmos_core::suggest::Suggestion::new(
            cosmos_core::suggest::SuggestionKind::Improvement,
            cosmos_core::suggest::Priority::High,
            PathBuf::from("src/lib.rs"),
            "Improve demo".to_string(),
            cosmos_core::suggest::SuggestionSource::LlmDeep,
        )
        .with_validation_state(cosmos_core::suggest::SuggestionValidationState::Validated)
        .with_line(1),
    );
    let context = WorkContext {
        branch: "main".to_string(),
        uncommitted_files: Vec::new(),
        staged_files: Vec::new(),
        untracked_files: Vec::new(),
        inferred_focus: None,
        modified_count: 0,
        repo_root: root.clone(),
    };
    let mut app = App::new(index.clone(), suggestions, context);
    app.active_panel = ActivePanel::Suggestions;
    app.workflow_step = WorkflowStep::Suggestions;

    let (tx, _rx) = mpsc::channel();
    let ctx = crate::app::RuntimeContext {
        index: &index,
        repo_path: &root,
        tx: &tx,
    };

    handle_normal_mode(
        &mut app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &ctx,
    )
    .unwrap();
    assert!(matches!(app.overlay, Overlay::ApplyPlan { .. }));
    assert!(app.armed_suggestion_id.is_some());

    crate::app::input::handle_key_event(
        &mut app,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &ctx,
    )
    .unwrap();
    assert_eq!(app.overlay, Overlay::None);
    assert!(app.armed_suggestion_id.is_none());
    assert!(app.armed_file_hashes.is_empty());
    let toast = app
        .toast
        .as_ref()
        .map(|t| t.message.clone())
        .unwrap_or_default();
    assert!(toast.contains("Apply canceled"));

    std::env::remove_var("OPENROUTER_API_KEY");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn apply_plan_confirm_reports_files_changed_since_preview() {
    std::env::set_var("OPENROUTER_API_KEY", "test-key");
    let mut root = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    root.push(format!("cosmos_apply_arm_changed_test_{}", nanos));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn demo() {}\n").unwrap();

    let index = CodebaseIndex {
        root: root.clone(),
        files: HashMap::new(),
        index_errors: Vec::new(),
        git_head: Some("deadbeef".to_string()),
    };
    let mut suggestions = SuggestionEngine::new(index.clone());
    suggestions.suggestions.push(
        cosmos_core::suggest::Suggestion::new(
            cosmos_core::suggest::SuggestionKind::Improvement,
            cosmos_core::suggest::Priority::High,
            PathBuf::from("src/lib.rs"),
            "Improve demo".to_string(),
            cosmos_core::suggest::SuggestionSource::LlmDeep,
        )
        .with_validation_state(cosmos_core::suggest::SuggestionValidationState::Validated)
        .with_line(1),
    );
    let context = WorkContext {
        branch: "main".to_string(),
        uncommitted_files: Vec::new(),
        staged_files: Vec::new(),
        untracked_files: Vec::new(),
        inferred_focus: None,
        modified_count: 0,
        repo_root: root.clone(),
    };
    let mut app = App::new(index.clone(), suggestions, context);
    app.active_panel = ActivePanel::Suggestions;
    app.workflow_step = WorkflowStep::Suggestions;

    let (tx, _rx) = mpsc::channel();
    let ctx = crate::app::RuntimeContext {
        index: &index,
        repo_path: &root,
        tx: &tx,
    };

    handle_normal_mode(
        &mut app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &ctx,
    )
    .unwrap();
    assert!(matches!(app.overlay, Overlay::ApplyPlan { .. }));
    std::fs::write(root.join("src/lib.rs"), "fn demo() { println!(\"x\"); }\n").unwrap();
    crate::app::input::handle_key_event(
        &mut app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &ctx,
    )
    .unwrap();

    let toast = app
        .toast
        .as_ref()
        .map(|t| t.message.clone())
        .unwrap_or_default();
    assert!(toast.contains("files changed"));
    assert!(app.armed_suggestion_id.is_none());
    assert_eq!(app.overlay, Overlay::None);

    std::env::remove_var("OPENROUTER_API_KEY");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_plan_confirm_starts_apply_flow() {
    std::env::set_var("OPENROUTER_API_KEY", "test-key");
    let (_dir, repo_path) = init_temp_git_repo_with_file();

    let index = CodebaseIndex {
        root: repo_path.clone(),
        files: HashMap::new(),
        index_errors: Vec::new(),
        git_head: Some("deadbeef".to_string()),
    };
    let mut suggestions = SuggestionEngine::new(index.clone());
    suggestions.suggestions.push(
        cosmos_core::suggest::Suggestion::new(
            cosmos_core::suggest::SuggestionKind::Improvement,
            cosmos_core::suggest::Priority::High,
            PathBuf::from("src/lib.rs"),
            "Improve demo".to_string(),
            cosmos_core::suggest::SuggestionSource::LlmDeep,
        )
        .with_validation_state(cosmos_core::suggest::SuggestionValidationState::Validated)
        .with_line(1),
    );
    let context = WorkContext {
        branch: "main".to_string(),
        uncommitted_files: Vec::new(),
        staged_files: Vec::new(),
        untracked_files: Vec::new(),
        inferred_focus: None,
        modified_count: 0,
        repo_root: repo_path.clone(),
    };
    let mut app = App::new(index.clone(), suggestions, context);
    app.active_panel = ActivePanel::Suggestions;
    app.workflow_step = WorkflowStep::Suggestions;

    let (tx, _rx) = mpsc::channel();
    let ctx = crate::app::RuntimeContext {
        index: &index,
        repo_path: &repo_path,
        tx: &tx,
    };

    crate::app::input::handle_key_event(
        &mut app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &ctx,
    )
    .unwrap();
    assert!(matches!(app.overlay, Overlay::ApplyPlan { .. }));

    crate::app::input::handle_key_event(
        &mut app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &ctx,
    )
    .unwrap();

    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.loading, LoadingState::GeneratingFix);
    assert!(app.armed_suggestion_id.is_none());
    assert!(app.armed_file_hashes.is_empty());

    std::env::remove_var("OPENROUTER_API_KEY");
}

#[test]
fn dismiss_key_removes_active_suggestion() {
    std::env::set_var("OPENROUTER_API_KEY", "test-key");
    let mut root = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    root.push(format!("cosmos_dismiss_test_{}", nanos));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn demo() {}\n").unwrap();

    let index = CodebaseIndex {
        root: root.clone(),
        files: HashMap::new(),
        index_errors: Vec::new(),
        git_head: Some("deadbeef".to_string()),
    };
    let mut suggestions = SuggestionEngine::new(index.clone());
    suggestions.suggestions.push(
        cosmos_core::suggest::Suggestion::new(
            cosmos_core::suggest::SuggestionKind::Improvement,
            cosmos_core::suggest::Priority::High,
            PathBuf::from("src/lib.rs"),
            "Improve demo".to_string(),
            cosmos_core::suggest::SuggestionSource::LlmDeep,
        )
        .with_validation_state(cosmos_core::suggest::SuggestionValidationState::Validated)
        .with_line(1),
    );
    let context = WorkContext {
        branch: "main".to_string(),
        uncommitted_files: Vec::new(),
        staged_files: Vec::new(),
        untracked_files: Vec::new(),
        inferred_focus: None,
        modified_count: 0,
        repo_root: root.clone(),
    };
    let mut app = App::new(index.clone(), suggestions, context);
    app.active_panel = ActivePanel::Suggestions;
    app.workflow_step = WorkflowStep::Suggestions;
    assert_eq!(app.suggestions.active_suggestions().len(), 1);

    let (tx, _rx) = mpsc::channel();
    let ctx = crate::app::RuntimeContext {
        index: &index,
        repo_path: &root,
        tx: &tx,
    };

    handle_normal_mode(
        &mut app,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        &ctx,
    )
    .unwrap();

    assert_eq!(app.suggestions.active_suggestions().len(), 0);
    assert!(app.suggestions.suggestions[0].dismissed);

    std::env::remove_var("OPENROUTER_API_KEY");
    let _ = std::fs::remove_dir_all(root);
}

fn init_temp_git_repo_with_file() -> (tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let repo_path = dir.path().to_path_buf();
    let repo = Repository::init(&repo_path).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Test User").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();

    std::fs::create_dir_all(repo_path.join("src")).unwrap();
    std::fs::write(repo_path.join("src/lib.rs"), "fn demo() {}\n").unwrap();

    let sig = Signature::now("Test User", "test@example.com").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("src/lib.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
        .unwrap();
    (dir, repo_path)
}

#[test]
fn finalization_refuses_if_branch_changed_during_apply() {
    let (_dir, repo_path) = init_temp_git_repo_with_file();
    let source_branch = git_ops::current_status(&repo_path).unwrap().branch;

    let repo = Repository::open(&repo_path).unwrap();
    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature/other", &head_commit, false).unwrap();
    git_ops::checkout_branch(&repo_path, "feature/other").unwrap();

    let suggestion = cosmos_core::suggest::Suggestion::new(
        cosmos_core::suggest::SuggestionKind::Improvement,
        cosmos_core::suggest::Priority::High,
        PathBuf::from("src/lib.rs"),
        "Improve demo".to_string(),
        cosmos_core::suggest::SuggestionSource::LlmDeep,
    );
    let branch_name =
        git_ops::generate_fix_branch_name(&suggestion.id.to_string(), &suggestion.summary);

    let result = finalize_harness_result_on_branch(
        &repo_path,
        &source_branch,
        &suggestion,
        &[ImplementationAppliedFile {
            path: PathBuf::from("src/lib.rs"),
            summary: "Modified".to_string(),
            content: "fn demo() { println!(\"x\"); }\n".to_string(),
        }],
    );
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(
        err.status,
        ImplementationFinalizationStatus::FailedBeforeFinalize
    );
    assert!(err.message.contains("active branch changed"));

    // Ensure finalization did not create/switch to a fix branch.
    let status = git_ops::current_status(&repo_path).unwrap();
    assert_eq!(status.branch, "feature/other");
    assert!(Repository::open(&repo_path)
        .unwrap()
        .find_branch(&branch_name, git2::BranchType::Local)
        .is_err());
}

#[test]
fn finalization_rolls_back_on_unsafe_path_and_deletes_branch() {
    let (_dir, repo_path) = init_temp_git_repo_with_file();
    let source_branch = git_ops::current_status(&repo_path).unwrap().branch;

    let suggestion = cosmos_core::suggest::Suggestion::new(
        cosmos_core::suggest::SuggestionKind::Improvement,
        cosmos_core::suggest::Priority::High,
        PathBuf::from("src/lib.rs"),
        "Improve demo".to_string(),
        cosmos_core::suggest::SuggestionSource::LlmDeep,
    );
    let branch_name =
        git_ops::generate_fix_branch_name(&suggestion.id.to_string(), &suggestion.summary);

    let result = finalize_harness_result_on_branch(
        &repo_path,
        &source_branch,
        &suggestion,
        &[ImplementationAppliedFile {
            path: PathBuf::from("../evil"),
            summary: "Nope".to_string(),
            content: "bad".to_string(),
        }],
    );
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.status, ImplementationFinalizationStatus::RolledBack);
    assert!(!err.mutation_on_failure);

    // After rollback, we should be back on the source branch with a clean working tree.
    let status = git_ops::current_status(&repo_path).unwrap();
    assert_eq!(status.branch, source_branch);
    assert!(status.staged.is_empty());
    assert!(status.modified.is_empty());

    // Branch should be cleaned up.
    let repo = Repository::open(&repo_path).unwrap();
    assert!(repo
        .find_branch(&branch_name, git2::BranchType::Local)
        .is_err());
}

#[test]
fn finalization_success_stages_only_payload_files() {
    let (_dir, repo_path) = init_temp_git_repo_with_file();
    let source_branch = git_ops::current_status(&repo_path).unwrap().branch;

    let suggestion = cosmos_core::suggest::Suggestion::new(
        cosmos_core::suggest::SuggestionKind::Improvement,
        cosmos_core::suggest::Priority::High,
        PathBuf::from("src/lib.rs"),
        "Improve demo".to_string(),
        cosmos_core::suggest::SuggestionSource::LlmDeep,
    );

    let (branch, changes) = finalize_harness_result_on_branch(
        &repo_path,
        &source_branch,
        &suggestion,
        &[ImplementationAppliedFile {
            path: PathBuf::from("src/lib.rs"),
            summary: "Modified: demo".to_string(),
            content: "fn demo() { println!(\"x\"); }\n".to_string(),
        }],
    )
    .unwrap();

    // Should have created/switch to a fix branch and stage only the approved file.
    assert_eq!(git_ops::current_status(&repo_path).unwrap().branch, branch);
    let status = git_ops::current_status(&repo_path).unwrap();
    assert_eq!(status.staged, vec!["src/lib.rs".to_string()]);
    assert!(status.modified.is_empty());
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].0, PathBuf::from("src/lib.rs"));

    let content = std::fs::read_to_string(repo_path.join("src/lib.rs")).unwrap();
    assert!(content.contains("println!"));
}
