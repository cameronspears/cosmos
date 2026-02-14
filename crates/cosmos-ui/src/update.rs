//! Shell-mode updater stubs.

use anyhow::Result;

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub latest_version: String,
}

pub async fn check_for_update() -> Result<Option<UpdateInfo>> {
    Ok(None)
}

pub fn run_update<F>(_target_version: &str, _on_progress: F) -> Result<()>
where
    F: Fn(u8) + Send + 'static,
{
    Err(anyhow::anyhow!("Self-update is disabled in UI shell mode"))
}
