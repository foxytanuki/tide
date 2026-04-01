use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::tree::{
    expand_to_window_by_id, find_folder_flat_index_by_path, find_window_flat_index_by_id, flatten,
    folder_path_for_path, get_node, next_visible_item, FlatItem, FlatNodeKind, TreeNode,
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
    pub sidebar: SidebarState,
    pub renames: RenameState,
    pub reorder: ReorderState,
    pub ai: AiState,
    /// Terminal size for mouse hit-testing (width, height).
    pub terminal_size: (u16, u16),
}

pub struct SidebarState {
    pub pane_id: String,
    pub home_pane_id: String,
    pub window_id: String,
    pub preview: PreviewState,
    /// Number of internal window-focus notifications to suppress.
    pub ignore_window_changes: u8,
    /// Window ID expected while suppression is active.
    pub pending_internal_focus_window: Option<String>,
    /// Circuit breaker: true after attempting RestorePreview before CloseWindow.
    /// Prevents infinite requeue loop if RestorePreview fails persistently.
    pub close_restore_attempted: bool,
    /// Saved "without sidebar" window layouts for pane width restoration.
    /// Maps window_id → (terminal_width_at_save, layout_string).
    pub pane_layouts: HashMap<String, (u16, String)>,
    /// Windows that successfully had the 3x2 helper applied.
    pub helper_managed_windows: HashSet<String>,
    /// Window ID that the debounced preview will switch to.
    /// Set on cursor movement, cleared when preview actually executes.
    /// Used by view to show the active marker on the correct window during debounce.
    pub pending_preview_id: Option<String>,
}

pub struct RenameState {
    /// Windows tracked for rename stabilization.
    pub pending: HashMap<String, PendingRename>,
    /// Most recent pending-rename target, used to keep selection stable.
    pub last_window_id: Option<String>,
}

pub struct ReorderState {
    pub pending_selection: Option<SelectionTarget>,
    pub snapshot: Option<ReorderSnapshot>,
}

pub struct ReorderSnapshot {
    pub tree: Vec<TreeNode>,
    pub cursor: usize,
}

pub struct AiState {
    /// Pane IDs currently running AI processes (actively working).
    pub panes: HashSet<String>,
    /// Window IDs derived from ai_panes (for sidebar indicator).
    pub windows: HashSet<String>,
    /// Pane IDs currently highlighted with AI background.
    pub highlighted_panes: HashSet<String>,
    /// AI process CPU tracking for activity detection.
    /// Maps pane_id → (ai_pid, last_cpu_ticks, polls_since_active, consecutive_bursts).
    pub cpu_tracker: HashMap<String, (u32, u64, u16, u8)>,
    /// Count of %output events per pane since last poll (for streaming detection).
    /// Reset to 0 after each classify_active_panes call.
    pub output_counts: HashMap<String, u32>,
    /// Polls to skip %output counts after window switch (pane redraw noise).
    pub output_suppress: u8,
    /// Window IDs where AI recently finished, with completion timestamp.
    /// Used to show fading badge: ○ (new moon) → disappear after timeout.
    pub recently_finished: HashMap<String, Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Renaming { window_id: String },
    RenamingFolder { folder_name: String },
    CreatingProject,
    Moving { subject: MoveSubject },
    ConfirmClose { window_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionTarget {
    Window(String),
    Folder(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveSubject {
    Item(SelectionTarget),
    Project { folder_name: String },
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
            sidebar: SidebarState {
                pane_id: sidebar_pane_id,
                home_pane_id,
                window_id: sidebar_window_id,
                preview: PreviewState::Home,
                ignore_window_changes: 0,
                pending_internal_focus_window: None,
                close_restore_attempted: false,
                pane_layouts: HashMap::new(),
                helper_managed_windows: HashSet::new(),
                pending_preview_id: None,
            },
            renames: RenameState {
                pending: HashMap::new(),
                last_window_id: None,
            },
            reorder: ReorderState {
                pending_selection: None,
                snapshot: None,
            },
            ai: AiState {
                panes: HashSet::new(),
                windows: HashSet::new(),
                highlighted_panes: HashSet::new(),
                cpu_tracker: HashMap::new(),
                output_counts: HashMap::new(),
                output_suppress: 0,
                recently_finished: HashMap::new(),
            },
            terminal_size: (80, 24),
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

    pub fn replace_tree_preserve_selection(
        &mut self,
        new_tree: Vec<TreeNode>,
        selected_target: Option<&SelectionTarget>,
    ) {
        self.tree = new_tree;
        if let Some(SelectionTarget::Window(window_id)) = selected_target {
            expand_to_window_by_id(&mut self.tree, window_id);
        }
        self.rebuild_flat();
        if let Some(target) = selected_target {
            match target {
                SelectionTarget::Window(window_id) => {
                    if let Some(index) =
                        find_window_flat_index_by_id(&self.flat_items, &self.tree, window_id)
                    {
                        self.cursor = index;
                    }
                }
                SelectionTarget::Folder(folder_path) => {
                    if let Some(index) =
                        find_folder_flat_index_by_path(&self.flat_items, &self.tree, folder_path)
                    {
                        self.cursor = index;
                    }
                }
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
            // If cursor landed on an expanded folder, skip forward to first non-folder child
            self.snap_cursor_off_expanded_folder();
        }
    }

    /// If cursor is on an expanded folder, move it to the next non-expanded-folder item.
    fn snap_cursor_off_expanded_folder(&mut self) {
        if let Some(item) = self.flat_items.get(self.cursor) {
            if item.kind == FlatNodeKind::Folder && item.expanded {
                if let Some(next) = next_visible_item(&self.flat_items, self.cursor) {
                    self.cursor = next;
                }
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

    pub fn selected_selection_target(&self) -> Option<SelectionTarget> {
        let item = self.flat_items.get(self.cursor)?;
        match get_node(&self.tree, &item.path).ok()? {
            TreeNode::Window { info } => Some(SelectionTarget::Window(info.id.clone())),
            TreeNode::Folder { .. } => folder_path_for_path(&self.tree, &item.path)
                .ok()
                .map(SelectionTarget::Folder),
        }
    }

    pub fn begin_reorder_preview(&mut self) {
        self.reorder.snapshot = Some(ReorderSnapshot {
            tree: self.tree.clone(),
            cursor: self.cursor,
        });
    }

    pub fn cancel_reorder_preview(&mut self) {
        if let Some(snapshot) = self.reorder.snapshot.take() {
            self.tree = snapshot.tree;
            self.rebuild_flat();
            self.cursor = snapshot.cursor.min(self.flat_items.len().saturating_sub(1));
        }
    }

    pub fn clear_reorder_preview(&mut self) {
        self.reorder.snapshot = None;
    }

    /// Find any window ID in the tree that is not `exclude`.
    pub fn find_another_window_id(&self, exclude: &str) -> Option<String> {
        fn search(nodes: &[TreeNode], exclude: &str) -> Option<String> {
            for node in nodes {
                match node {
                    TreeNode::Window { info } if info.id != exclude => {
                        return Some(info.id.clone());
                    }
                    TreeNode::Folder { children, .. } => {
                        if let Some(id) = search(children, exclude) {
                            return Some(id);
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        search(&self.tree, exclude)
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
        model.replace_tree_preserve_selection(
            first_tree,
            Some(&SelectionTarget::Window("@2".to_string())),
        );
        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@2")
        );
        assert_eq!(model.cursor(), 2);

        let mut second_tree = build_tree(&[
            window("@1", 1, "new:uno"),
            window("@2", 2, "new:dos"),
            window("@3", 3, "dev"),
        ]);
        if let TreeNode::Folder { expanded, .. } = &mut second_tree[0] {
            *expanded = false;
        }

        model.replace_tree_preserve_selection(
            second_tree,
            Some(&SelectionTarget::Window("@2".to_string())),
        );

        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@2")
        );
        assert_eq!(model.cursor(), 2);
        assert!(matches!(
            model.flat_items()[model.cursor()].kind,
            crate::tree::FlatNodeKind::Window
        ));
        assert_eq!(model.flat_items().len(), 4);
    }
}
