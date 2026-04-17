use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::tree::{
    collect_folder_expanded, expand_to_window_by_id, flatten, folder_path_for_path, get_node,
    next_visible_item, FlatItem, FlatNodeKind, TreeNode, WindowInfo,
};

pub struct Model {
    tree: Vec<TreeNode>,
    flat_items: Vec<FlatItem>,
    pub window_list_snapshot: Option<Vec<WindowInfo>>,
    folder_expanded_snapshot: HashMap<String, bool>,
    window_flat_indices: HashMap<String, usize>,
    folder_flat_indices: HashMap<String, usize>,
    selection_targets: Vec<Option<SelectionTarget>>,
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
    /// Ignore self-inflicted layout-change validation until this deadline.
    pub ignore_layout_change_until: Option<Instant>,
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
    /// Consecutive empty AI polls, used to back off the expensive pane scan.
    pub idle_polls: u8,
    /// Remaining 500ms ticks to skip before the next AI poll.
    pub poll_skip_ticks: u8,
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
            window_list_snapshot: None,
            folder_expanded_snapshot: HashMap::new(),
            window_flat_indices: HashMap::new(),
            folder_flat_indices: HashMap::new(),
            selection_targets: Vec::new(),
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
                ignore_layout_change_until: None,
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
                idle_polls: 0,
                poll_skip_ticks: 0,
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
            if let Some(index) = self.flat_index_for_selection_target(target) {
                self.cursor = index;
            }
        }
    }

    pub fn set_window_list_snapshot(&mut self, windows: Vec<WindowInfo>) {
        self.window_list_snapshot = Some(windows);
    }

    pub fn folder_expanded_snapshot(&self) -> &HashMap<String, bool> {
        &self.folder_expanded_snapshot
    }

    pub fn rename_window_leaves(&mut self, leaf_names_by_id: &HashMap<String, String>) {
        fn rename_nodes(nodes: &mut [TreeNode], leaf_names_by_id: &HashMap<String, String>) {
            for node in nodes {
                match node {
                    TreeNode::Window { info } => {
                        if let Some(new_name) = leaf_names_by_id.get(info.id.as_str()) {
                            info.name = new_name.clone();
                        }
                    }
                    TreeNode::Folder { children, .. } => rename_nodes(children, leaf_names_by_id),
                }
            }
        }

        rename_nodes(&mut self.tree, leaf_names_by_id);
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
        self.folder_expanded_snapshot = collect_folder_expanded(&self.tree);
        self.window_flat_indices.clear();
        self.folder_flat_indices.clear();
        self.selection_targets.clear();
        self.selection_targets.reserve(self.flat_items.len());
        for (idx, item) in self.flat_items.iter().enumerate() {
            match item.kind {
                FlatNodeKind::Window => {
                    if let Ok(TreeNode::Window { info }) = get_node(&self.tree, &item.path) {
                        self.window_flat_indices.insert(info.id.clone(), idx);
                        self.selection_targets
                            .push(Some(SelectionTarget::Window(info.id.clone())));
                    } else {
                        self.selection_targets.push(None);
                    }
                }
                FlatNodeKind::Folder => {
                    if let Ok(folder_path) = folder_path_for_path(&self.tree, &item.path) {
                        self.folder_flat_indices.insert(folder_path.clone(), idx);
                        self.selection_targets
                            .push(Some(SelectionTarget::Folder(folder_path)));
                    } else {
                        self.selection_targets.push(None);
                    }
                }
            }
        }
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

    pub fn flat_index_for_window_id(&self, window_id: &str) -> Option<usize> {
        self.window_flat_indices.get(window_id).copied()
    }

    pub fn flat_index_for_folder_path(&self, folder_path: &str) -> Option<usize> {
        self.folder_flat_indices.get(folder_path).copied()
    }

    pub fn flat_index_for_selection_target(&self, target: &SelectionTarget) -> Option<usize> {
        match target {
            SelectionTarget::Window(window_id) => self.flat_index_for_window_id(window_id),
            SelectionTarget::Folder(folder_path) => self.flat_index_for_folder_path(folder_path),
        }
    }

    pub fn selected_window_info(&self) -> Option<&WindowInfo> {
        match self.selected_node()? {
            TreeNode::Window { info } => Some(info),
            TreeNode::Folder { .. } => None,
        }
    }

    pub fn selected_selection_target(&self) -> Option<SelectionTarget> {
        self.selection_targets.get(self.cursor)?.clone()
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

    #[test]
    fn selected_selection_target_uses_rebuilt_cache() {
        let mut model = test_model();
        let tree = build_tree(&[window("@1", 1, "proj:one"), window("@2", 2, "dev")]);
        model.replace_tree_preserve_selection(tree, None);

        assert_eq!(model.flat_index_for_window_id("@1"), Some(1));
        assert_eq!(model.flat_index_for_folder_path("proj"), Some(0));

        model.set_cursor(1);
        assert_eq!(
            model.selected_selection_target(),
            Some(SelectionTarget::Window("@1".into()))
        );
    }
}
