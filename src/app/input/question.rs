use crate::app::background;
use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::suggest;
use crate::ui::{App, LoadingState};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

/// Handle key events in question (ask cosmos) mode
pub(super) fn handle_question_input(
    app: &mut App,
    key: KeyEvent,
    ctx: &RuntimeContext,
) -> Result<()> {
    match key.code {
        KeyCode::Esc => app.exit_question(),
        KeyCode::Up if app.question_input.is_empty() => app.question_suggestion_up(),
        KeyCode::Down if app.question_input.is_empty() => app.question_suggestion_down(),
        KeyCode::Tab => app.use_selected_suggestion(),
        KeyCode::Enter => submit_question(app, ctx)?,
        KeyCode::Backspace => app.question_pop(),
        KeyCode::Char(c) => app.question_push(c),
        _ => {}
    }
    Ok(())
}

/// Submit a question to the LLM
fn submit_question(app: &mut App, ctx: &RuntimeContext) -> Result<()> {
    // If input is empty, use the selected suggestion first
    if app.question_input.is_empty() && !app.question_suggestions.is_empty() {
        app.use_selected_suggestion();
    }
    let question = app.take_question();
    if question.is_empty() {
        return Ok(());
    }

    // Send question directly to LLM
    let index_clone = ctx.index.clone();
    let context_clone = app.context.clone();
    let tx_question = ctx.tx.clone();
    let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);

    app.loading = LoadingState::Answering;

    background::spawn_background(ctx.tx.clone(), "ask_question", async move {
        let mem = if repo_memory_context.trim().is_empty() {
            None
        } else {
            Some(repo_memory_context)
        };
        match suggest::llm::ask_question(&index_clone, &context_clone, &question, mem).await {
            Ok((answer, usage)) => {
                let _ = tx_question.send(BackgroundMessage::QuestionResponse { answer, usage });
            }
            Err(e) => {
                let _ = tx_question.send(BackgroundMessage::Error(e.to_string()));
            }
        }
    });
    Ok(())
}
