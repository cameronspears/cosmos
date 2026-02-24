use super::{App, RuntimeContext};

pub(super) fn llm_available_for_apply() -> bool {
    #[cfg(test)]
    {
        true
    }
    #[cfg(not(test))]
    {
        cosmos_engine::llm::is_available()
    }
}

pub(super) fn prompt_api_key_setup(app: &mut App, reason: &str) {
    app.open_api_key_overlay(Some(reason.to_string()));
}

pub(super) fn refresh_suggestions_now(app: &mut App, ctx: &RuntimeContext, reason: &str) {
    if !app.suggestion_focus_selected_once {
        app.open_suggestion_focus_overlay();
        return;
    }
    if !crate::app::background::request_suggestions_refresh(
        app,
        ctx.tx.clone(),
        ctx.repo_path.clone(),
        reason,
    ) && !cosmos_engine::llm::is_available()
    {
        prompt_api_key_setup(
            app,
            "No API key configured yet. Add your Cerebras key to continue.",
        );
    }
}
