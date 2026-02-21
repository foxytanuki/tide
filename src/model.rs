use std::collections::HashMap;

use crate::tree::{flatten, get_node, FlatItem, TreeNode, WindowInfo};

pub struct Model {
    tree: Vec<TreeNode>,
    flat_items: Vec<FlatItem>,
    cursor: usize,
    pub session_name: String,
    pub mode: Mode,
    pub input_buffer: String,
    pub should_quit: bool,
    pub error_message: Option<String>,
    // Preview state
    pub sidebar_pane_id: String,
    pub home_pane_id: String,
    pub sidebar_window_id: String,
    pub preview: PreviewState,
    pub layout_without_sidebar_by_window: HashMap<String, String>,
    /// Number of session-window-changed events to suppress.
    /// join-pane and select-window can each trigger events; set to 2 to absorb both.
    pub ignore_window_changes: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Renaming { window_id: String },
    ConfirmClose { window_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewState {
    Home,
    Previewing {
        original_window_id: String,
        original_home_pane_id: String,
    },
}

impl Model {
    pub fn new(
        session_name: String,
        sidebar_pane_id: String,
        home_pane_id: String,
        sidebar_window_id: String,
    ) -> Self {
        Self {
            tree: Vec::new(),
            flat_items: Vec::new(),
            cursor: 0,
            session_name,
            mode: Mode::Normal,
            input_buffer: String::new(),
            should_quit: false,
            error_message: None,
            sidebar_pane_id,
            home_pane_id,
            sidebar_window_id,
            preview: PreviewState::Home,
            layout_without_sidebar_by_window: HashMap::new(),
            ignore_window_changes: 0,
        }
    }

    pub fn tree(&self) -> &[TreeNode] {
        &self.tree
    }

    pub fn flat_items(&self) -> &[FlatItem] {
        &self.flat_items
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set_cursor(&mut self, idx: usize) {
        self.cursor = idx;
    }

    /// Replace the tree and rebuild the flat list + clamp cursor.
    pub fn replace_tree(&mut self, new_tree: Vec<TreeNode>) {
        self.tree = new_tree;
        self.rebuild_flat();
    }

    /// Mutate the tree in-place, then rebuild the flat list + clamp cursor.
    pub fn mutate_tree<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Vec<TreeNode>) -> R,
    {
        let result = f(&mut self.tree);
        self.rebuild_flat();
        result
    }

    fn rebuild_flat(&mut self) {
        self.flat_items = flatten(&self.tree);
        if self.flat_items.is_empty() {
            self.cursor = 0;
        } else {
            let max_index = self.flat_items.len() - 1;
            if self.cursor > max_index {
                self.cursor = max_index;
            }
        }
    }

    pub fn selected_node(&self) -> Option<&TreeNode> {
        let item = self.flat_items.get(self.cursor)?;
        get_node(&self.tree, &item.path).ok()
    }

    pub fn selected_window_info(&self) -> Option<&WindowInfo> {
        match self.selected_node()? {
            TreeNode::Window { info } => Some(info),
            TreeNode::Folder { .. } => None,
        }
    }
}
