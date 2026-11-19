use std::sync::Arc;

use crate::editor::Editor;
use crate::plugin::{Info, PluginParameters};

pub struct PluginCache {
    pub info: Info,
    pub params: Arc<dyn PluginParameters>,
    pub editor: Option<Box<dyn Editor>>,
    /// The words of the arrangements the last `effGetSpeakerArrangement` pointed the host at:
    /// they live here, so the pointers stay valid after the call returns.
    pub arrangement_words: [Vec<u32>; 2],
}

impl PluginCache {
    pub fn new(
        info: &Info,
        params: Arc<dyn PluginParameters>,
        editor: Option<Box<dyn Editor>>,
    ) -> Self {
        Self {
            info: info.clone(),
            params,
            editor,
            arrangement_words: [Vec::new(), Vec::new()],
        }
    }
}
