use std::collections::{HashMap, HashSet};

use crate::tree::{
    expand_to_window_by_id, find_window_flat_index_by_id, flatten, get_node, FlatItem, TreeNode,
    WindowInfo,
};

pub struct Model {
    tree: Vec<TreeNode>,
    flat_items: Vec<FlatItem>,
    cursor: usize,
    pub session_name: String,
    pub session_id: String,
    pub mode: Mode,
    pub input_buffer: String,
    pub input_cursor: usize,
    pub should_quit: bool,
    pub restart_requested: bool,
    pub error_message: Option<String>,
    pub info_message: Option<String>,
    // Preview state
    pub sidebar_pane_id: String,
    pub home_pane_id: String,
    pub sidebar_window_id: String,
    pub preview: PreviewState,
    pub layout_without_sidebar_by_window: HashMap<String, String>,
    /// Number of internal window-focus notifications to suppress.
    pub ignore_window_changes: u8,
    /// Window ID expected while suppression is active.
    pub pending_internal_focus_window: Option<String>,
    /// Windows tracked for rename stabilization.
    pub pending_renames: HashMap<String, PendingRename>,
    /// Most recent pending-rename target, used to keep selection stable.
    pub pending_rename_last_window_id: Option<String>,
    /// Pane IDs currently running AI processes (actively working).
    pub ai_panes: HashSet<String>,
    /// Window IDs derived from ai_panes (for sidebar indicator).
    pub ai_windows: HashSet<String>,
    /// Pane IDs currently highlighted with AI background.
    pub highlighted_panes: HashSet<String>,
    /// AI process CPU tracking for activity detection.
    /// Maps pane_id → (ai_pid, last_cpu_ticks, polls_since_active).
    pub ai_cpu_tracker: HashMap<String, (u32, u64, u16)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Renaming { window_id: String },
    RenamingFolder { folder_name: String },
    CreatingProject,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRename {
    pub target_name: String,
    pub observed_count: u8,
}

impl Model {
    pub fn new(
        session_name: String,
        session_id: String,
        sidebar_pane_id: String,
        home_pane_id: String,
        sidebar_window_id: String,
    ) -> Self {
        Self {
            tree: Vec::new(),
            flat_items: Vec::new(),
            cursor: 0,
            session_name,
            session_id,
            mode: Mode::Normal,
            input_buffer: String::new(),
            input_cursor: 0,
            should_quit: false,
            restart_requested: false,
            error_message: None,
            info_message: None,
            sidebar_pane_id,
            home_pane_id,
            sidebar_window_id,
            preview: PreviewState::Home,
            layout_without_sidebar_by_window: HashMap::new(),
            ignore_window_changes: 0,
            pending_internal_focus_window: None,
            pending_renames: HashMap::new(),
            pending_rename_last_window_id: None,
            ai_panes: HashSet::new(),
            ai_windows: HashSet::new(),
            highlighted_panes: HashSet::new(),
            ai_cpu_tracker: HashMap::new(),
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

    pub fn replace_tree_preserve_selection(&mut self, new_tree: Vec<TreeNode>, selected_window_id: Option<&str>) {
        self.tree = new_tree;
        if let Some(window_id) = selected_window_id {
            expand_to_window_by_id(&mut self.tree, window_id);
        }
        self.rebuild_flat();
        if let Some(window_id) = selected_window_id {
            if let Some(index) =
                find_window_flat_index_by_id(&self.flat_items, &self.tree, window_id)
            {
                self.cursor = index;
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{build_tree, WindowInfo};

    fn test_model() -> Model {
        Model::new(
            "s".to_string(),
            "$1".to_string(),
            "%sidebar".to_string(),
            "%home".to_string(),
            "@home".to_string(),
        )
    }

    fn window(id: &str, index: usize, name: &str) -> WindowInfo {
        WindowInfo {
            id: id.to_string(),
            index,
            name: name.to_string(),
            active: false,
        }
    }

    #[test]
    fn replace_tree_preserve_selection_expands_folder_for_target() {
        let mut model = test_model();
        let first = vec![
            window("@1", 1, "new:one"),
            window("@2", 2, "new:two"),
            window("@3", 3, "dev"),
        ];
        let first_tree = build_tree(&first);
        model.replace_tree_preserve_selection(first_tree, Some("@2"));
        assert_eq!(model.selected_window_info().map(|w| w.id.as_str()), Some("@2"));
        assert_eq!(model.cursor(), 2);

        let mut second_tree = build_tree(&[
            window("@1", 1, "new:uno"),
            window("@2", 2, "new:dos"),
            window("@3", 3, "dev"),
        ]);
        if let TreeNode::Folder { expanded, .. } = &mut second_tree[0] {
            *expanded = false;
        }

        model.replace_tree_preserve_selection(second_tree, Some("@2"));

        assert_eq!(model.selected_window_info().map(|w| w.id.as_str()), Some("@2"));
        assert_eq!(model.cursor(), 2);
        assert!(matches!(
            model.flat_items()[model.cursor()].kind,
            crate::tree::FlatNodeKind::Window
        ));
        assert_eq!(model.flat_items().len(), 4);
    }
}
