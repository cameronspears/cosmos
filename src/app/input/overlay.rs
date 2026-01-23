use crate::app::background;
use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::suggest;
use crate::ui::{App, LoadingState, Overlay};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

/// Handle key events when an overlay is active
pub(super) fn handle_overlay_input(
    app: &mut App,
    key: KeyEvent,
    ctx: &RuntimeContext,
) -> Result<()> {
    // Handle overlay mode
    if app.overlay != Overlay::None {
        // Inquiry privacy preview overlay
        if let Overlay::InquiryPreview { question, .. } = &app.overlay {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
                KeyCode::Down => app.overlay_scroll_down(),
                KeyCode::Up => app.overlay_scroll_up(),
                KeyCode::Enter => {
                    if let Err(e) = app.config.allow_ai(app.session_cost) {
                        app.show_toast(&e);
                        return Ok(());
                    }
                    let question = question.clone();
                    let index_clone = ctx.index.clone();
                    let context_clone = app.context.clone();
                    let tx_question = ctx.tx.clone();
                    let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
                    app.loading = LoadingState::Answering;
                    app.close_overlay();
                    background::spawn_background(ctx.tx.clone(), "ask_question_preview", async move {
                        let mem = if repo_memory_context.trim().is_empty() {
                            None
                        } else {
                            Some(repo_memory_context)
                        };
                        match suggest::llm::ask_question(
                            &index_clone,
                            &context_clone,
                            &question,
                            mem,
                        )
                        .await
                        {
                            Ok((answer, usage)) => {
                                let _ = tx_question.send(BackgroundMessage::QuestionResponse {
                                    answer,
                                    usage,
                                });
                            }
                            Err(e) => {
                                let _ = tx_question.send(BackgroundMessage::Error(e.to_string()));
                            }
                        }
                    });
                }
                _ => {}
            }
            return Ok(());
        }

        // Handle Reset cosmos overlay
        if let Overlay::Reset { .. } = &app.overlay {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    app.close_overlay();
                }
                KeyCode::Down => {
                    app.reset_navigate(1);
                }
                KeyCode::Up => {
                    app.reset_navigate(-1);
                }
                KeyCode::Char(' ') => {
                    app.reset_toggle_selected();
                }
                KeyCode::Enter => {
                    let selected = app.get_reset_selections();
                    if selected.is_empty() {
                        app.show_toast("Select at least one reset option");
                        return Ok(());
                    }

                    app.loading = LoadingState::Resetting;
                    app.close_overlay();

                    let tx_reset = ctx.tx.clone();
                    background::spawn_background(ctx.tx.clone(), "reset_cosmos", async move {
                        match crate::cache::reset_cosmos(&selected).await {
                            Ok(_) => {
                                let _ =
                                    tx_reset.send(BackgroundMessage::ResetComplete { options: selected });
                            }
                            Err(e) => {
                                let _ = tx_reset.send(BackgroundMessage::Error(e.to_string()));
                            }
                        }
                    });
                }
                _ => {}
            }
            return Ok(());
        }

        // Handle Startup Check overlay
        if let Overlay::StartupCheck {
            confirming_discard, ..
        } = &app.overlay
        {
            let confirming = *confirming_discard;
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    app.close_overlay();
                }
                KeyCode::Char('y') if confirming => {
                    // Discard changes and continue
                    app.close_overlay();
                    app.should_quit = true;
                }
                KeyCode::Enter => {
                    if confirming {
                        // Discard changes and continue
                        app.close_overlay();
                        app.should_quit = true;
                    } else {
                        app.startup_check_confirm_discard(true);
                    }
                }
                KeyCode::Char('d') if confirming => {
                    // Discard changes and continue
                    app.close_overlay();
                    app.should_quit = true;
                }
                KeyCode::Char('c') if confirming => {
                    // Cancel discard confirmation
                    app.startup_check_confirm_discard(false);
                }
                KeyCode::Char('n') if confirming => {
                    // Cancel confirmation
                    app.startup_check_confirm_discard(false);
                }
                _ => {}
            }
            return Ok(());
        }

        // Handle other overlays (generic scroll/close)
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
            KeyCode::Down => app.overlay_scroll_down(),
            KeyCode::Up => app.overlay_scroll_up(),
            _ => {}
        }
        return Ok(());
    }

    Ok(())
}
