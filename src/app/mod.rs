pub mod background;
pub mod bootstrap;
pub mod input;
pub mod messages;
pub mod runtime;

#[allow(unused_imports)]
pub use messages::BackgroundMessage;
pub use runtime::run_tui;

use crate::index::CodebaseIndex;
use std::path::PathBuf;
use std::sync::mpsc;

pub struct RuntimeContext<'a> {
    pub index: &'a CodebaseIndex,
    pub repo_path: &'a PathBuf,
    pub tx: &'a mpsc::Sender<messages::BackgroundMessage>,
}
