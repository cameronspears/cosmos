use crate::ui;

/// Messages from background tasks to the main UI thread.
pub enum BackgroundMessage {
    /// Ship workflow progress update
    ShipProgress(ui::ShipStep),
    /// Ship workflow completed successfully with PR URL
    ShipComplete(String),
    /// Ship workflow error
    ShipError(String),
    /// Cache reset completed
    ResetComplete {
        options: Vec<crate::cache::ResetOption>,
    },
    /// Git stash completed (save my work)
    StashComplete { message: String },
    /// Discard changes completed
    DiscardComplete,
    /// Generic error (used for push/etc)
    Error(String),
    /// New version available - show update panel
    UpdateAvailable { latest_version: String },
    /// Update download progress (0-100)
    UpdateProgress { percent: u8 },
    /// Update failed
    UpdateError(String),
}
