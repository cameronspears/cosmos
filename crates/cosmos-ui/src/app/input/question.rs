use crate::app::background;
use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::ui::{App, InputMode, ASK_STARTER_QUESTIONS};
use anyhow::Result;
use cosmos_adapters::util::{hash_bytes, hash_str, resolve_repo_path_allow_new};
use crossterm::event::{KeyCode, KeyEvent};
use std::path::PathBuf;

/// Handle key events in question (ask cosmos) mode
pub(super) fn handle_question_input(
    app: &mut App,
    key: KeyEvent,
    ctx: &RuntimeContext,
) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.question_input.clear();
            app.question_suggestion_selected = 0;
        }
        KeyCode::Up if app.question_input.is_empty() => app.question_suggestion_up(),
        KeyCode::Down if app.question_input.is_empty() => app.question_suggestion_down(),
        KeyCode::Enter => submit_question(app, ctx)?,
        KeyCode::Backspace => app.question_pop(),
        KeyCode::Char(c) => app.question_push(c),
        _ => {}
    }
    Ok(())
}

/// Compute a context hash for cache validation
/// Uses a deterministic fingerprint that includes code identity and working context.
fn compute_context_hash(app: &App) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();

    // Stable code identity from index.
    if let Some(git_head) = app.index.git_head.as_ref() {
        "git_head".hash(&mut hasher);
        git_head.hash(&mut hasher);
    } else {
        let mut file_entries: Vec<(String, String)> = app
            .index
            .files
            .iter()
            .map(|(path, file)| {
                (
                    path.to_string_lossy().to_string(),
                    file.content_hash.clone(),
                )
            })
            .collect();
        file_entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, content_hash) in file_entries {
            path.hash(&mut hasher);
            content_hash.hash(&mut hasher);
        }
    }

    // Current branch and high-level context markers.
    app.context.branch.hash(&mut hasher);
    app.context.inferred_focus.hash(&mut hasher);
    app.context.modified_count.hash(&mut hasher);

    // Include digests of all changed files to avoid stale cache hits when counts are unchanged.
    let mut changed_files: Vec<PathBuf> = app
        .context
        .uncommitted_files
        .iter()
        .chain(app.context.staged_files.iter())
        .chain(app.context.untracked_files.iter())
        .cloned()
        .collect();
    changed_files.sort();
    changed_files.dedup();
    for rel_path in changed_files {
        rel_path.hash(&mut hasher);
        match resolve_repo_path_allow_new(&app.repo_path, &rel_path) {
            Ok(resolved) => match std::fs::read(&resolved.absolute) {
                Ok(bytes) => hash_bytes(&bytes).hash(&mut hasher),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => "<missing>".hash(&mut hasher),
                Err(_) => "<unreadable>".hash(&mut hasher),
            },
            Err(_) => "<unresolvable>".hash(&mut hasher),
        }
    }

    // Include prompt memory context digest since it influences generated answers.
    hash_str(&app.repo_memory.to_prompt_context(12, 900)).hash(&mut hasher);

    // Include project ethos digest so ask cache invalidates when ETHOS.md changes.
    let ethos_digest = match std::fs::read_to_string(app.repo_path.join("ETHOS.md")) {
        Ok(content) if !content.trim().is_empty() => hash_str(content.trim()),
        Ok(_) => "<ethos-empty>".to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "<ethos-missing>".to_string(),
        Err(_) => "<ethos-unreadable>".to_string(),
    };
    ethos_digest.hash(&mut hasher);

    format!("{:016x}", hasher.finish())
}

/// Submit a question to the LLM
fn submit_question(app: &mut App, ctx: &RuntimeContext) -> Result<()> {
    // If input is empty, use the selected suggestion first
    if app.question_input.is_empty() && !ASK_STARTER_QUESTIONS.is_empty() {
        app.use_selected_suggestion();
    }
    let question = app.take_question();
    if question.is_empty() {
        return Ok(());
    }
    app.input_mode = InputMode::Question;
    let request_id = app.begin_ask_request();

    // Check cache first
    let context_hash = compute_context_hash(app);
    if let Some(cached_answer) = app.question_cache.get(&question, &context_hash) {
        // Cache hit! Use cached answer directly
        let _ = ctx.tx.send(BackgroundMessage::QuestionResponse {
            request_id,
            answer: cached_answer.to_string(),
            usage: None, // No usage for cached response
        });
        return Ok(());
    }

    // Cache miss - send question to LLM
    let index_clone = ctx.index.clone();
    let context_clone = app.context.clone();
    let tx_question = ctx.tx.clone();
    let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
    let question_for_cache = question.clone();
    let context_hash_for_cache = context_hash;

    background::spawn_background(ctx.tx.clone(), "ask_question", async move {
        let mem = if repo_memory_context.trim().is_empty() {
            None
        } else {
            Some(repo_memory_context)
        };
        match cosmos_engine::llm::ask_question(&index_clone, &context_clone, &question, mem).await {
            Ok((answer, usage)) => {
                // Send response with cache metadata for storage
                let _ = tx_question.send(BackgroundMessage::QuestionResponseWithCache {
                    request_id,
                    question: question_for_cache,
                    answer,
                    usage,
                    context_hash: context_hash_for_cache,
                });
            }
            Err(e) => {
                let _ = tx_question.send(BackgroundMessage::QuestionError {
                    request_id,
                    error: e.to_string(),
                });
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmos_core::context::WorkContext;
    use cosmos_core::index::CodebaseIndex;
    use cosmos_core::suggest::SuggestionEngine;
    use crossterm::event::KeyModifiers;
    use std::collections::HashMap;
    use std::sync::mpsc;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_test_app(repo_root: &std::path::Path) -> App {
        let index = CodebaseIndex {
            root: repo_root.to_path_buf(),
            files: HashMap::new(),
            index_errors: Vec::new(),
            git_head: None,
        };
        let suggestions = SuggestionEngine::new(index.clone());
        let context = WorkContext {
            branch: "feature/context-hash".to_string(),
            uncommitted_files: vec![PathBuf::from("src/lib.rs")],
            staged_files: Vec::new(),
            untracked_files: Vec::new(),
            inferred_focus: Some("src".to_string()),
            modified_count: 1,
            repo_root: repo_root.to_path_buf(),
        };
        App::new(index, suggestions, context)
    }

    #[test]
    fn context_hash_changes_when_file_content_changes_with_same_counts() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_question_hash_test_{}", nanos));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let file_path = root.join("src/lib.rs");
        std::fs::write(&file_path, "pub fn value() -> i32 { 1 }\n").unwrap();

        let app = make_test_app(&root);
        let first = compute_context_hash(&app);

        std::fs::write(&file_path, "pub fn value() -> i32 { 2 }\n").unwrap();
        let second = compute_context_hash(&app);

        assert_ne!(first, second);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn context_hash_changes_when_ethos_changes() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_question_hash_ethos_test_{}", nanos));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("ETHOS.md"), "# Ethos\n\nPlain language first.\n").unwrap();

        let app = make_test_app(&root);
        let first = compute_context_hash(&app);

        std::fs::write(root.join("ETHOS.md"), "# Ethos\n\nSafety before speed.\n").unwrap();
        let second = compute_context_hash(&app);

        assert_ne!(first, second);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enter_with_empty_input_uses_selected_starter_question() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_question_submit_starter_test_{}", nanos));
        std::fs::create_dir_all(&root).unwrap();

        let mut app = make_test_app(&root);
        app.context.uncommitted_files.clear();
        app.context.modified_count = 0;
        app.start_question();
        app.question_suggestion_selected = 2;

        let selected = ASK_STARTER_QUESTIONS[2].to_string();
        let context_hash = compute_context_hash(&app);
        app.question_cache
            .set(selected, "starter-cached-answer".to_string(), context_hash);

        let (tx, rx) = mpsc::channel();
        let index = app.index.clone();
        let ctx = RuntimeContext {
            index: &index,
            repo_path: &root,
            tx: &tx,
        };

        handle_question_input(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &ctx,
        )
        .unwrap();

        assert_eq!(app.input_mode, crate::ui::InputMode::Question);
        assert!(app.ask_in_flight);
        match rx.recv_timeout(Duration::from_millis(200)).unwrap() {
            BackgroundMessage::QuestionResponse { answer, .. } => {
                assert_eq!(answer, "starter-cached-answer")
            }
            _ => panic!("unexpected message variant"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enter_with_typed_input_uses_typed_question() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_question_submit_typed_test_{}", nanos));
        std::fs::create_dir_all(&root).unwrap();

        let mut app = make_test_app(&root);
        app.context.uncommitted_files.clear();
        app.context.modified_count = 0;
        app.start_question();
        app.question_suggestion_selected = 1;
        app.question_input = "How does retries flow work?".to_string();

        let context_hash = compute_context_hash(&app);
        app.question_cache.set(
            app.question_input.clone(),
            "typed-cached-answer".to_string(),
            context_hash,
        );

        let (tx, rx) = mpsc::channel();
        let index = app.index.clone();
        let ctx = RuntimeContext {
            index: &index,
            repo_path: &root,
            tx: &tx,
        };

        handle_question_input(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &ctx,
        )
        .unwrap();

        assert_eq!(app.input_mode, crate::ui::InputMode::Question);
        assert!(app.ask_in_flight);
        assert!(app.question_input.is_empty());
        match rx.recv_timeout(Duration::from_millis(200)).unwrap() {
            BackgroundMessage::QuestionResponse { answer, .. } => {
                assert_eq!(answer, "typed-cached-answer")
            }
            _ => panic!("unexpected message variant"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn esc_clears_question_input_and_selection() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_question_escape_test_{}", nanos));
        std::fs::create_dir_all(&root).unwrap();

        let mut app = make_test_app(&root);
        app.start_question();
        app.question_input = "temporary input".to_string();
        app.question_suggestion_selected = 4;

        let (tx, _rx) = mpsc::channel();
        let index = app.index.clone();
        let ctx = RuntimeContext {
            index: &index,
            repo_path: &root,
            tx: &tx,
        };

        handle_question_input(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &ctx,
        )
        .unwrap();

        assert_eq!(app.input_mode, crate::ui::InputMode::Question);
        assert!(app.question_input.is_empty());
        assert_eq!(app.question_suggestion_selected, 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enter_submits_question_while_suggestions_are_generating() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!(
            "cosmos_question_submit_during_suggest_test_{}",
            nanos
        ));
        std::fs::create_dir_all(&root).unwrap();

        let mut app = make_test_app(&root);
        app.loading = crate::ui::LoadingState::GeneratingSuggestions;
        app.context.uncommitted_files.clear();
        app.context.modified_count = 0;
        app.start_question();
        app.question_input = "How is context ranked?".to_string();

        let context_hash = compute_context_hash(&app);
        app.question_cache.set(
            app.question_input.clone(),
            "answer while suggesting".to_string(),
            context_hash,
        );

        let (tx, rx) = mpsc::channel();
        let index = app.index.clone();
        let ctx = RuntimeContext {
            index: &index,
            repo_path: &root,
            tx: &tx,
        };

        handle_question_input(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &ctx,
        )
        .unwrap();

        assert!(app.ask_in_flight);
        match rx.recv_timeout(Duration::from_millis(200)).unwrap() {
            BackgroundMessage::QuestionResponse { answer, .. } => {
                assert_eq!(answer, "answer while suggesting")
            }
            _ => panic!("unexpected message variant"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
