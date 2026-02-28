use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tracing::debug;

use crate::cmd::Cmd;
use crate::model::{Mode, Model, PendingRename, PreviewState};
use crate::msg::Msg;
use crate::tree::{
    build_tree, find_parent_folder, get_node, get_node_mut, next_visible_item, prev_visible_item,
    toggle_expand, FlatNodeKind, TreeNode, WindowInfo,
};
use crate::view::tree_item_at;

const MISSING_PENDING_RENAME_THRESHOLD: u8 = 6;

pub fn update(model: &mut Model, msg: Msg) -> Vec<Cmd> {
    // Clear error on any user action (exclude background events)
    if !matches!(
        msg,
        Msg::WindowChanged
            | Msg::WindowRenamed { .. }
            | Msg::WindowListLoaded(_)
            | Msg::WindowFocusChanged(_)
            | Msg::AiProcessPollResult { .. }
    ) {
        model.error_message = None;
        model.info_message = None;
    }

    match msg {
        Msg::CursorUp => handle_cursor_up(model),
        Msg::CursorDown => handle_cursor_down(model),
        Msg::SelectItem => handle_select_item(model),
        Msg::CollapseOrParent => handle_collapse_or_parent(model),
        Msg::ToggleFolder => handle_toggle_folder(model),
        Msg::Escape => vec![Cmd::FocusRightPane],
        Msg::NewWindow => {
            let name = determine_new_window_name(model);
            vec![Cmd::NewWindow { name }]
        }
        Msg::NewProject => {
            clear_input(model);
            model.mode = Mode::CreatingProject;
            vec![Cmd::Render]
        }
        Msg::RenameWindow => handle_rename_window(model),
        Msg::CloseWindow => handle_close_window(model),
        Msg::WindowFocusChanged(window_id) => handle_window_focus_changed(model, window_id),
        Msg::WindowChanged => vec![Cmd::ListWindows],
        Msg::WindowRenamed { window_id, name } => handle_window_renamed(model, window_id, name),
        Msg::WindowListLoaded(windows) => handle_window_list_loaded(model, windows),
        Msg::AiProcessPollResult { panes, windows } => {
            use std::time::Instant;

            const RECENTLY_FINISHED_TIMEOUT: std::time::Duration =
                std::time::Duration::from_secs(5 * 60);

            let now = Instant::now();
            let mut recently_changed = false;

            // Detect windows that just finished AI (were active, now gone)
            for old_win in &model.ai_windows {
                if !windows.contains(old_win) {
                    model.recently_finished_ai.insert(old_win.clone(), now);
                    recently_changed = true;
                }
            }
            // Remove windows that became active again
            for new_win in &windows {
                if model.recently_finished_ai.remove(new_win).is_some() {
                    recently_changed = true;
                }
            }
            // Expire old entries
            let before_len = model.recently_finished_ai.len();
            model.recently_finished_ai.retain(|_, finished_at| {
                now.duration_since(*finished_at) < RECENTLY_FINISHED_TIMEOUT
            });
            if model.recently_finished_ai.len() != before_len {
                recently_changed = true;
            }

            let ai_changed = panes != model.ai_panes || windows != model.ai_windows;
            model.ai_panes = panes;
            model.ai_windows = windows;

            if ai_changed {
                vec![Cmd::CheckBorder, Cmd::Render]
            } else if recently_changed {
                vec![Cmd::Render]
            } else {
                vec![]
            }
        }
        Msg::MouseClick { row } => {
            if !matches!(model.mode, Mode::Normal) {
                return vec![];
            }
            match tree_item_at(model, row) {
                Some(index) => handle_mouse_click(model, index),
                None => vec![],
            }
        }
        Msg::MouseScrollUp => {
            if !matches!(model.mode, Mode::Normal) {
                return vec![];
            }
            handle_cursor_up(model)
        }
        Msg::MouseScrollDown => {
            if !matches!(model.mode, Mode::Normal) {
                return vec![];
            }
            handle_cursor_down(model)
        }
        Msg::Key(event) => handle_key(model, event),
        Msg::Restart => {
            model.should_quit = true;
            model.restart_requested = true;
            vec![Cmd::ResetAllBorders, Cmd::RestorePreview, Cmd::Restart]
        }
        Msg::Quit => {
            model.should_quit = true;
            vec![Cmd::ResetAllBorders, Cmd::RestorePreview, Cmd::Quit]
        }
    }
}

fn handle_window_focus_changed(model: &mut Model, window_id: String) -> Vec<Cmd> {
    // Suppress only the internally expected window-focus events.
    // If an unexpected window_id arrives while suppression is active,
    // treat it as a real user switch instead of consuming suppression.
    if model.ignore_window_changes > 0 {
        let expected = model.pending_internal_focus_window.as_deref();
        if expected == Some(window_id.as_str()) {
            model.ignore_window_changes -= 1;
            if model.ignore_window_changes == 0 {
                model.pending_internal_focus_window = None;
            }
            return vec![Cmd::EnsureSidebarWidth];
        }
    }

    // Ignore events for the window the sidebar is already in.
    if window_id == model.sidebar_window_id {
        return vec![Cmd::EnsureSidebarWidth];
    }

    // User explicitly switched windows; clear preview state.
    model.preview = PreviewState::Home;
    model.ignore_window_changes = 0;
    model.pending_internal_focus_window = None;
    vec![
        Cmd::FollowToWindow { window_id },
        Cmd::EnsureSidebarWidth,
        Cmd::ListWindows,
    ]
}

fn handle_window_renamed(model: &mut Model, window_id: String, name: String) -> Vec<Cmd> {
    if let Some(pending) = model.pending_renames.get_mut(&window_id) {
        let expected_name = pending.target_name.clone();
        if name != expected_name {
            debug!(
                id = window_id.as_str(),
                current = name.as_str(),
                expected = expected_name.as_str(),
                "pending rename mismatch from event, correcting immediately"
            );
            pending.observed_count = 0;
            model.pending_rename_last_window_id = Some(window_id.clone());
            return vec![Cmd::RenameWindow {
                id: window_id,
                name: expected_name,
            }];
        }

        pending.observed_count = pending.observed_count.saturating_add(1);
        debug!(
            id = window_id.as_str(),
            name = name.as_str(),
            observed = pending.observed_count,
            "pending rename observed from event"
        );
        Vec::new()
    } else {
        vec![Cmd::ListWindows]
    }
}

fn handle_window_list_loaded(model: &mut Model, windows: Vec<WindowInfo>) -> Vec<Cmd> {
    // Save folder expanded state before rebuilding.
    let expanded_state = collect_folder_expanded(model.tree());
    let mut selected_window_id = derive_selected_window_id(model);

    let selected_exists = selected_window_id
        .as_deref()
        .is_some_and(|id| windows.iter().any(|w| w.id == *id));
    debug!(
        pending_count = model.pending_renames.len(),
        pending_last = model.pending_rename_last_window_id.as_deref(),
        mode = ?model.mode,
        selected = selected_window_id.as_deref(),
        exists = selected_exists,
        cursor = model.cursor(),
        "window list loaded"
    );

    bump_last_pending_if_missing(model, &windows, &mut selected_window_id);

    let mut new_tree = build_tree(&windows);
    restore_folder_expanded(&mut new_tree, &expanded_state);
    let selected_ref = selected_window_id.as_deref();
    model.replace_tree_preserve_selection(new_tree, selected_ref);

    let (mut followup_cmds, stale_pending_ids) = reconcile_pending_renames(model, &windows);
    clear_stale_pending_renames(model, stale_pending_ids);
    clear_orphaned_last_pending_rename(model);

    debug!(
        cursor = model.cursor(),
        selected = selected_ref,
        "window list selection restored"
    );

    clear_mode_if_missing_target(model, &windows);
    followup_cmds.push(Cmd::Render);
    followup_cmds
}

fn derive_selected_window_id(model: &Model) -> Option<String> {
    model
        .pending_rename_last_window_id
        .as_ref()
        .filter(|id| model.pending_renames.contains_key(*id))
        .cloned()
        .or_else(|| match &model.mode {
            Mode::Renaming { window_id } => Some(window_id.clone()),
            _ => model.selected_window_info().map(|info| info.id.clone()),
        })
}

fn bump_last_pending_if_missing(
    model: &mut Model,
    windows: &[WindowInfo],
    selected_window_id: &mut Option<String>,
) {
    if let Some(last_id) = model.pending_rename_last_window_id.clone() {
        let last_exists = windows.iter().any(|w| w.id == last_id);
        if !last_exists {
            if let Some(pending) = model.pending_renames.get_mut(&last_id) {
                pending.observed_count = pending.observed_count.saturating_add(1);
                debug!(
                    id = last_id.as_str(),
                    observed = pending.observed_count,
                    "latest pending rename target missing from window list"
                );
            }
            *selected_window_id = None;
        }
    }
}

fn reconcile_pending_renames(model: &mut Model, windows: &[WindowInfo]) -> (Vec<Cmd>, Vec<String>) {
    let mut followup_cmds = Vec::new();
    let mut stale_pending_ids = Vec::new();

    for (id, pending) in &mut model.pending_renames {
        // Skip the last-tracked ID if it was already incremented above.
        if model.pending_rename_last_window_id.as_deref() == Some(id.as_str())
            && !windows.iter().any(|w| w.id == *id)
        {
            // Already handled in the last_id block — skip to avoid
            // double-incrementing observed_count.
            if pending.observed_count >= MISSING_PENDING_RENAME_THRESHOLD {
                stale_pending_ids.push(id.clone());
            }
            continue;
        }

        if let Some(current) = windows.iter().find(|w| w.id == *id) {
            if current.name != pending.target_name {
                debug!(
                    id = id,
                    current = current.name.as_str(),
                    expected = pending.target_name.as_str(),
                    observed = pending.observed_count,
                    "pending rename mismatch, correcting"
                );
                followup_cmds.push(Cmd::RenameWindow {
                    id: id.clone(),
                    name: pending.target_name.clone(),
                });
                pending.observed_count = 0;
            } else {
                pending.observed_count = pending.observed_count.saturating_add(1);
                debug!(
                    id = id,
                    name = current.name.as_str(),
                    observed = pending.observed_count,
                    "pending rename observed"
                );
            }
        } else {
            pending.observed_count = pending.observed_count.saturating_add(1);
            if pending.observed_count >= MISSING_PENDING_RENAME_THRESHOLD {
                stale_pending_ids.push(id.clone());
            }
        }
    }

    (followup_cmds, stale_pending_ids)
}

fn clear_stale_pending_renames(model: &mut Model, stale_pending_ids: Vec<String>) {
    for stale_id in stale_pending_ids {
        debug!(
            id = stale_id.as_str(),
            "pending rename stale (missing) and cleared"
        );
        model.pending_renames.remove(&stale_id);
        if model.pending_rename_last_window_id.as_deref() == Some(stale_id.as_str()) {
            model.pending_rename_last_window_id = None;
        }
    }
}

fn clear_orphaned_last_pending_rename(model: &mut Model) {
    if let Some(last_id) = model.pending_rename_last_window_id.as_deref() {
        if !model.pending_renames.contains_key(last_id) {
            model.pending_rename_last_window_id = None;
        }
    }
}

fn join_folder_path(prefix: Option<&str>, name: &str) -> String {
    match prefix {
        Some(p) => format!("{}:{}", p, name),
        None => name.to_string(),
    }
}

fn reset_to_normal_mode(model: &mut Model) {
    model.mode = Mode::Normal;
    clear_input(model);
}

fn exit_to_normal_mode(model: &mut Model) -> Vec<Cmd> {
    reset_to_normal_mode(model);
    vec![Cmd::Render]
}

fn clear_mode_if_missing_target(model: &mut Model, windows: &[WindowInfo]) {
    let should_reset = match &model.mode {
        Mode::Renaming { window_id } | Mode::ConfirmClose { window_id } => {
            !windows.iter().any(|w| w.id == *window_id)
        }
        Mode::RenamingFolder { folder_name } => {
            let folder_prefix = format!("{folder_name}:");
            windows.iter().all(|w| !w.name.starts_with(&folder_prefix))
        }
        Mode::Normal | Mode::CreatingProject => false,
    };

    if should_reset {
        reset_to_normal_mode(model);
    }
}

fn handle_cursor_up(model: &mut Model) -> Vec<Cmd> {
    if let Some(prev) = prev_visible_item(model.flat_items(), model.cursor()) {
        model.set_cursor(prev);
        preview_current_item(model)
    } else {
        vec![]
    }
}

fn handle_cursor_down(model: &mut Model) -> Vec<Cmd> {
    if let Some(next) = next_visible_item(model.flat_items(), model.cursor()) {
        model.set_cursor(next);
        preview_current_item(model)
    } else {
        vec![]
    }
}

/// Preview: if cursor is on a window, swap its pane into the right slot.
fn preview_current_item(model: &mut Model) -> Vec<Cmd> {
    let window_id = model.selected_window_info().map(|info| info.id.clone());
    if let Some(id) = window_id {
        model.pending_preview_id = Some(id.clone());
        vec![Cmd::PreviewWindow { id }, Cmd::Render]
    } else {
        model.pending_preview_id = None;
        vec![Cmd::Render]
    }
}

fn handle_select_item(model: &mut Model) -> Vec<Cmd> {
    let item = match model.flat_items().get(model.cursor()) {
        Some(item) => item.clone(),
        None => return vec![],
    };

    match item.kind {
        FlatNodeKind::Folder => {
            model.mutate_tree(|tree| {
                if let Ok(node) = get_node_mut(tree, &item.path) {
                    toggle_expand(node);
                }
            });
            vec![Cmd::Render]
        }
        FlatNodeKind::Window => {
            if let Some(info) = model.selected_window_info() {
                vec![
                    Cmd::PreviewWindow {
                        id: info.id.clone(),
                    },
                    Cmd::FocusRightPane,
                ]
            } else {
                vec![]
            }
        }
    }
}

fn handle_mouse_click(model: &mut Model, index: usize) -> Vec<Cmd> {
    let item = match model.flat_items().get(index) {
        Some(item) => item.clone(),
        None => return vec![],
    };

    match item.kind {
        FlatNodeKind::Folder => {
            model.set_cursor(index);
            model.mutate_tree(|tree| {
                if let Ok(node) = get_node_mut(tree, &item.path) {
                    toggle_expand(node);
                }
            });
            vec![Cmd::Render]
        }
        FlatNodeKind::Window => {
            if model.cursor() == index {
                // Already selected → follow (focus right pane)
                handle_select_item(model)
            } else {
                // Move cursor + preview
                model.set_cursor(index);
                preview_current_item(model)
            }
        }
    }
}

fn handle_collapse_or_parent(model: &mut Model) -> Vec<Cmd> {
    let item = match model.flat_items().get(model.cursor()) {
        Some(item) => item.clone(),
        None => return vec![],
    };

    match item.kind {
        FlatNodeKind::Folder => {
            let collapsed = model.mutate_tree(|tree| {
                if let Ok(TreeNode::Folder { expanded, .. }) = get_node_mut(tree, &item.path) {
                    if *expanded {
                        *expanded = false;
                        return true;
                    }
                }
                false
            });
            if collapsed {
                return vec![Cmd::Render];
            }
            if let Some(parent_idx) = find_parent_folder(model.flat_items(), model.cursor()) {
                model.set_cursor(parent_idx);
                vec![Cmd::Render]
            } else {
                vec![]
            }
        }
        FlatNodeKind::Window => {
            if let Some(parent_idx) = find_parent_folder(model.flat_items(), model.cursor()) {
                model.set_cursor(parent_idx);
                vec![Cmd::Render]
            } else {
                vec![]
            }
        }
    }
}

fn handle_toggle_folder(model: &mut Model) -> Vec<Cmd> {
    let item = match model.flat_items().get(model.cursor()) {
        Some(item) => item.clone(),
        None => return vec![],
    };

    if item.kind == FlatNodeKind::Folder {
        model.mutate_tree(|tree| {
            if let Ok(node) = get_node_mut(tree, &item.path) {
                toggle_expand(node);
            }
        });
        vec![Cmd::Render]
    } else {
        vec![]
    }
}

fn handle_rename_window(model: &mut Model) -> Vec<Cmd> {
    let item = match model.flat_items().get(model.cursor()) {
        Some(item) => item.clone(),
        None => return vec![],
    };

    match item.kind {
        FlatNodeKind::Window => {
            let info = match model.selected_window_info() {
                Some(info) => info.clone(),
                None => return vec![],
            };
            let full_name = reconstruct_full_name(model, model.cursor(), &info.name);
            set_input(model, full_name);
            model.mode = Mode::Renaming {
                window_id: info.id.clone(),
            };
            vec![Cmd::Render]
        }
        FlatNodeKind::Folder => {
            let folder_full_name = reconstruct_folder_full_name(model, &item.path);
            if folder_full_name.is_empty() {
                return vec![];
            }
            set_input(model, folder_full_name.clone());
            model.mode = Mode::RenamingFolder {
                folder_name: folder_full_name,
            };
            vec![Cmd::Render]
        }
    }
}

/// Reconstruct full tmux name for a window by walking up ancestor folders.
/// e.g. if window "edit" is in subfolder "sub" in folder "proj", returns "proj:sub:edit".
fn reconstruct_full_name(model: &Model, flat_idx: usize, leaf_name: &str) -> String {
    let mut parts = vec![leaf_name.to_string()];
    let mut idx = flat_idx;
    while let Some(parent_idx) = find_parent_folder(model.flat_items(), idx) {
        if let Some(parent_item) = model.flat_items().get(parent_idx) {
            if let Ok(TreeNode::Folder { name, .. }) = get_node(model.tree(), &parent_item.path) {
                parts.push(name.clone());
            }
        }
        idx = parent_idx;
    }
    parts.reverse();
    parts.join(":")
}

/// Reconstruct full colon-separated path for a folder node.
/// e.g. subfolder "sub" under folder "proj" returns "proj:sub".
fn reconstruct_folder_full_name(model: &Model, path: &[usize]) -> String {
    let mut parts = Vec::new();
    // Walk from root to this node, collecting folder names
    for depth in 0..path.len() {
        let ancestor_path = &path[..=depth];
        if let Ok(TreeNode::Folder { name, .. }) = get_node(model.tree(), ancestor_path) {
            parts.push(name.clone());
        }
    }
    parts.join(":")
}

fn handle_close_window(model: &mut Model) -> Vec<Cmd> {
    let window_id = match model.selected_window_info() {
        Some(info) => info.id.clone(),
        None => return vec![],
    };
    model.mode = Mode::ConfirmClose { window_id };
    vec![Cmd::Render]
}

fn handle_key(model: &mut Model, event: KeyEvent) -> Vec<Cmd> {
    match &model.mode {
        Mode::Normal => handle_normal_key(model, event),
        Mode::Renaming { .. } => handle_renaming_key(model, event),
        Mode::RenamingFolder { .. } => handle_renaming_folder_key(model, event),
        Mode::CreatingProject => handle_creating_project_key(model, event),
        Mode::ConfirmClose { .. } => handle_confirm_close_key(model, event),
    }
}

fn is_plain_key(event: &KeyEvent) -> bool {
    event.modifiers.is_empty() || event.modifiers == KeyModifiers::SHIFT
}

/// Convert a character index to a byte offset in the string.
fn char_to_byte_offset(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(s.len())
}

/// Handle common text input keys (arrows, backspace, char insert).
/// `input_cursor` is a character index (not byte offset).
/// Returns Some(cmds) if the key was handled, None for Enter/Esc/other.
fn handle_input_key(model: &mut Model, code: KeyCode) -> Option<Vec<Cmd>> {
    match code {
        KeyCode::Left => {
            if model.input_cursor > 0 {
                model.input_cursor -= 1;
            }
            Some(vec![Cmd::Render])
        }
        KeyCode::Right => {
            if model.input_cursor < model.input_buffer.chars().count() {
                model.input_cursor += 1;
            }
            Some(vec![Cmd::Render])
        }
        KeyCode::Home => {
            model.input_cursor = 0;
            Some(vec![Cmd::Render])
        }
        KeyCode::End => {
            model.input_cursor = model.input_buffer.chars().count();
            Some(vec![Cmd::Render])
        }
        KeyCode::Backspace => {
            if model.input_cursor > 0 {
                model.input_cursor -= 1;
                let byte_idx = char_to_byte_offset(&model.input_buffer, model.input_cursor);
                model.input_buffer.remove(byte_idx);
            }
            Some(vec![Cmd::Render])
        }
        KeyCode::Delete => {
            if model.input_cursor < model.input_buffer.chars().count() {
                let byte_idx = char_to_byte_offset(&model.input_buffer, model.input_cursor);
                model.input_buffer.remove(byte_idx);
            }
            Some(vec![Cmd::Render])
        }
        KeyCode::Char(c) => {
            let byte_idx = char_to_byte_offset(&model.input_buffer, model.input_cursor);
            model.input_buffer.insert(byte_idx, c);
            model.input_cursor += 1;
            Some(vec![Cmd::Render])
        }
        _ => None,
    }
}

fn clear_input(model: &mut Model) {
    model.input_buffer.clear();
    model.input_cursor = 0;
}

fn set_input(model: &mut Model, value: String) {
    model.input_cursor = value.chars().count();
    model.input_buffer = value;
}

fn handle_normal_key(model: &mut Model, event: KeyEvent) -> Vec<Cmd> {
    if !is_plain_key(&event) {
        if event.modifiers == KeyModifiers::CONTROL && event.code == KeyCode::Char('c') {
            return update(model, Msg::Quit);
        }
        return vec![];
    }

    match event.code {
        KeyCode::Char('j') | KeyCode::Down => update(model, Msg::CursorDown),
        KeyCode::Char('k') | KeyCode::Up => update(model, Msg::CursorUp),
        KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => update(model, Msg::SelectItem),
        KeyCode::Char('h') | KeyCode::Left => update(model, Msg::CollapseOrParent),
        KeyCode::Char(' ') => update(model, Msg::ToggleFolder),
        KeyCode::Esc => update(model, Msg::Escape),
        KeyCode::Char('r') => update(model, Msg::RenameWindow),
        KeyCode::Char('x') => update(model, Msg::CloseWindow),
        KeyCode::Char('c') => update(model, Msg::NewWindow),
        KeyCode::Char('C') => update(model, Msg::NewProject),
        KeyCode::Char('R') => update(model, Msg::Restart),
        KeyCode::Char('q') => update(model, Msg::Quit),
        _ => vec![],
    }
}

fn handle_renaming_key(model: &mut Model, event: KeyEvent) -> Vec<Cmd> {
    if !is_plain_key(&event) {
        return vec![];
    }
    if let Some(cmds) = handle_input_key(model, event.code) {
        return cmds;
    }
    match event.code {
        KeyCode::Enter => {
            let mode = model.mode.clone();
            if let Mode::Renaming { window_id } = mode {
                let new_name = model.input_buffer.trim().to_string();
                if new_name.is_empty() {
                    return vec![Cmd::Render];
                }
                // Short-circuit if name is unchanged — avoid unnecessary
                // tmux traffic and pending-rename tracking.
                // Compare against the full tmux name (folder:child for foldered
                // windows), not just the leaf name stored in info.name.
                let already_named = model.flat_items().iter().enumerate().any(|(idx, item)| {
                    if let Ok(TreeNode::Window { info }) = get_node(model.tree(), &item.path) {
                        if info.id != window_id {
                            return false;
                        }
                        let full_name = reconstruct_full_name(model, idx, &info.name);
                        full_name == new_name
                    } else {
                        false
                    }
                });
                if already_named {
                    model.mode = Mode::Normal;
                    clear_input(model);
                    return vec![Cmd::Render];
                }
                model.mode = Mode::Normal;
                model.pending_renames.insert(
                    window_id.clone(),
                    PendingRename {
                        target_name: new_name.clone(),
                        observed_count: 0,
                    },
                );
                model.pending_rename_last_window_id = Some(window_id.clone());
                clear_input(model);
                vec![
                    Cmd::RenameWindow {
                        id: window_id,
                        name: new_name,
                    },
                    Cmd::Render,
                ]
            } else {
                vec![]
            }
        }
        KeyCode::Esc => exit_to_normal_mode(model),
        _ => vec![],
    }
}

fn handle_creating_project_key(model: &mut Model, event: KeyEvent) -> Vec<Cmd> {
    if !is_plain_key(&event) {
        return vec![];
    }
    if let Some(cmds) = handle_input_key(model, event.code) {
        return cmds;
    }
    match event.code {
        KeyCode::Enter => {
            let name = model.input_buffer.trim().to_string();
            if name.is_empty() {
                return vec![Cmd::Render];
            }
            model.mode = Mode::Normal;
            clear_input(model);
            let window_name = format!("{}:tab1", name);
            vec![Cmd::NewWindow { name: window_name }]
        }
        KeyCode::Esc => exit_to_normal_mode(model),
        _ => vec![],
    }
}

fn handle_renaming_folder_key(model: &mut Model, event: KeyEvent) -> Vec<Cmd> {
    if !is_plain_key(&event) {
        return vec![];
    }
    if let Some(cmds) = handle_input_key(model, event.code) {
        return cmds;
    }
    match event.code {
        KeyCode::Enter => {
            let mode = model.mode.clone();
            if let Mode::RenamingFolder { folder_name } = mode {
                let new_name = model.input_buffer.trim().to_string();
                if new_name.is_empty() || new_name == folder_name {
                    model.mode = Mode::Normal;
                    clear_input(model);
                    return vec![Cmd::Render];
                }
                model.mode = Mode::Normal;
                clear_input(model);

                let children = collect_folder_children(model.tree(), &folder_name);
                let mut cmds = Vec::new();
                for (window_id, child_name) in &children {
                    let full_name = format!("{}:{}", new_name, child_name);
                    model.pending_renames.insert(
                        window_id.clone(),
                        PendingRename {
                            target_name: full_name.clone(),
                            observed_count: 0,
                        },
                    );
                    model.pending_rename_last_window_id = Some(window_id.clone());
                    cmds.push(Cmd::RenameWindow {
                        id: window_id.clone(),
                        name: full_name,
                    });
                }
                cmds.push(Cmd::Render);
                cmds
            } else {
                vec![]
            }
        }
        KeyCode::Esc => exit_to_normal_mode(model),
        _ => vec![],
    }
}

fn handle_confirm_close_key(model: &mut Model, event: KeyEvent) -> Vec<Cmd> {
    if !is_plain_key(&event) {
        return vec![];
    }
    match event.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            let mode = model.mode.clone();
            if let Mode::ConfirmClose { window_id } = mode {
                model.mode = Mode::Normal;
                vec![Cmd::CloseWindow { id: window_id }, Cmd::Render]
            } else {
                vec![]
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            model.mode = Mode::Normal;
            vec![Cmd::Render]
        }
        _ => vec![],
    }
}

/// Collect all window names from the tree as full tmux names (folder:child).
fn collect_window_names(nodes: &[TreeNode]) -> Vec<String> {
    collect_window_names_inner(nodes, None)
}

fn collect_window_names_inner(nodes: &[TreeNode], prefix: Option<&str>) -> Vec<String> {
    let mut names = Vec::new();
    for node in nodes {
        match node {
            TreeNode::Window { info } => {
                let name = match prefix {
                    Some(p) => format!("{}:{}", p, info.name),
                    None => info.name.clone(),
                };
                names.push(name);
            }
            TreeNode::Folder { name, children, .. } => {
                let full_prefix = join_folder_path(prefix, name);
                names.extend(collect_window_names_inner(children, Some(&full_prefix)));
            }
        }
    }
    names
}

/// Generate a unique tab name like "tab1", "tab2", ... that doesn't collide
/// with existing window names. If `folder` is Some, checks against "folder:tabN".
fn next_tab_name(existing_names: &[String], folder: Option<&str>) -> String {
    for i in 1.. {
        let candidate = match folder {
            Some(f) => format!("{}:tab{}", f, i),
            None => format!("tab{}", i),
        };
        if !existing_names.iter().any(|n| n == &candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// Determine the name for a new window based on cursor context.
/// If cursor is on/in a folder, prefix with the folder name (e.g. "proj:tab1").
/// Uses sequential numbering: tab1, tab2, tab3, ...
fn determine_new_window_name(model: &Model) -> String {
    let existing = collect_window_names(model.tree());

    let item = match model.flat_items().get(model.cursor()) {
        Some(item) => item,
        None => return next_tab_name(&existing, None),
    };

    if let Some(selected) = model.selected_window_info() {
        if let Some(pending) = model.pending_renames.get(selected.id.as_str()) {
            if let Some((folder, _)) = pending.target_name.rsplit_once(':') {
                let generated = next_tab_name(&existing, Some(folder));
                debug!(
                    cursor = model.cursor(),
                    generated, "new window name from pending rename context"
                );
                return generated;
            }
        }
    }

    match item.kind {
        FlatNodeKind::Folder => {
            let folder_full = reconstruct_folder_full_name(model, &item.path);
            if !folder_full.is_empty() {
                let generated = next_tab_name(&existing, Some(&folder_full));
                debug!(
                    cursor = model.cursor(),
                    generated, "new window name from folder"
                );
                generated
            } else {
                debug!(cursor = model.cursor(), "new window name fallback");
                next_tab_name(&existing, None)
            }
        }
        FlatNodeKind::Window => {
            if let Some(parent_idx) = find_parent_folder(model.flat_items(), model.cursor()) {
                if let Some(parent_item) = model.flat_items().get(parent_idx) {
                    let parent_full = reconstruct_folder_full_name(model, &parent_item.path);
                    if !parent_full.is_empty() {
                        let generated = next_tab_name(&existing, Some(&parent_full));
                        debug!(
                            cursor = model.cursor(),
                            generated, "new window name from parent folder"
                        );
                        return generated;
                    }
                }
            }
            debug!(
                cursor = model.cursor(),
                "new window name fallback from window w/o folder"
            );
            next_tab_name(&existing, None)
        }
    }
}

/// Collect (window_id, suffix) pairs for all windows in a folder (recursively).
/// `folder_path` is the colon-separated full path, e.g. "proj" or "proj:sub".
/// `suffix` is the part after the folder, e.g. for folder "proj" containing
/// subfolder "sub" with window "edit", suffix is "sub:edit".
fn collect_folder_children(tree: &[TreeNode], folder_path: &str) -> Vec<(String, String)> {
    if let Some(TreeNode::Folder { children, .. }) = find_folder_by_path(tree, folder_path) {
        let mut result = Vec::new();
        collect_folder_children_recursive(children, None, &mut result);
        return result;
    }
    Vec::new()
}

/// Navigate to a folder node by colon-separated path (e.g. "proj" or "proj:sub").
fn find_folder_by_path<'a>(tree: &'a [TreeNode], path: &str) -> Option<&'a TreeNode> {
    let parts: Vec<&str> = path.split(':').collect();
    let mut nodes = tree;
    let mut target: Option<&TreeNode> = None;
    for part in &parts {
        let mut found = false;
        for node in nodes {
            if let TreeNode::Folder { name, children, .. } = node {
                if name == part {
                    target = Some(node);
                    nodes = children;
                    found = true;
                    break;
                }
            }
        }
        if !found {
            return None;
        }
    }
    target
}

fn collect_folder_children_recursive(
    nodes: &[TreeNode],
    prefix: Option<&str>,
    out: &mut Vec<(String, String)>,
) {
    for node in nodes {
        match node {
            TreeNode::Window { info } => {
                let suffix = match prefix {
                    Some(p) => format!("{}:{}", p, info.name),
                    None => info.name.clone(),
                };
                out.push((info.id.clone(), suffix));
            }
            TreeNode::Folder { name, children, .. } => {
                let new_prefix = join_folder_path(prefix, name);
                collect_folder_children_recursive(children, Some(&new_prefix), out);
            }
        }
    }
}

/// Collect folder full-path → expanded state from the tree.
/// Uses colon-separated paths as keys (e.g. "proj", "proj:sub") to avoid
/// collisions between same-named subfolders under different parents.
fn collect_folder_expanded(nodes: &[TreeNode]) -> HashMap<String, bool> {
    let mut map = HashMap::new();
    collect_folder_expanded_inner(nodes, None, &mut map);
    map
}

fn collect_folder_expanded_inner(
    nodes: &[TreeNode],
    prefix: Option<&str>,
    map: &mut HashMap<String, bool>,
) {
    for node in nodes {
        if let TreeNode::Folder {
            name,
            expanded,
            children,
        } = node
        {
            let full_path = join_folder_path(prefix, name);
            map.insert(full_path.clone(), *expanded);
            collect_folder_expanded_inner(children, Some(&full_path), map);
        }
    }
}

/// Restore folder expanded state after tree rebuild
fn restore_folder_expanded(nodes: &mut [TreeNode], state: &HashMap<String, bool>) {
    restore_folder_expanded_inner(nodes, None, state);
}

fn restore_folder_expanded_inner(
    nodes: &mut [TreeNode],
    prefix: Option<&str>,
    state: &HashMap<String, bool>,
) {
    for node in nodes {
        if let TreeNode::Folder {
            name,
            expanded,
            children,
        } = node
        {
            let full_path = join_folder_path(prefix, name);
            if let Some(&was_expanded) = state.get(full_path.as_str()) {
                *expanded = was_expanded;
            }
            restore_folder_expanded_inner(children, Some(&full_path), state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_model() -> Model {
        Model::new(
            "s".to_string(),
            "$1".to_string(),
            "%sidebar".to_string(),
            "%home".to_string(),
            "@home".to_string(),
        )
    }

    fn assert_follow_window(cmds: &[Cmd], window_id: &str) {
        assert_eq!(cmds.len(), 3);
        assert!(matches!(
            &cmds[0],
            Cmd::FollowToWindow { window_id: id } if id == window_id
        ));
        assert!(matches!(cmds[1], Cmd::EnsureSidebarWidth));
        assert!(matches!(cmds[2], Cmd::ListWindows));
    }

    fn assert_ensure_only(cmds: &[Cmd]) {
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::EnsureSidebarWidth));
    }

    #[test]
    fn unexpected_focus_change_is_not_swallowed_during_suppression() {
        let mut model = test_model();
        model.preview = PreviewState::Previewing {
            original_window_id: "@home".to_string(),
            original_home_pane_id: "%home".to_string(),
        };
        model.ignore_window_changes = 2;
        model.pending_internal_focus_window = Some("@internal".to_string());

        let cmds = update(&mut model, Msg::WindowFocusChanged("@user".to_string()));

        assert_eq!(model.preview, PreviewState::Home);
        assert_eq!(model.ignore_window_changes, 0);
        assert_eq!(model.pending_internal_focus_window, None);
        assert_follow_window(&cmds, "@user");
    }

    use crate::tree::WindowInfo;

    fn wi(id: &str, index: usize, name: &str) -> WindowInfo {
        WindowInfo {
            id: id.to_string(),
            index,
            name: name.to_string(),
            active: false,
        }
    }

    fn assert_new_window_name(cmds: &[Cmd], expected: &str) {
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            Cmd::NewWindow { name } => assert_eq!(name, expected),
            other => panic!("expected NewWindow, got {:?}", other),
        }
    }

    #[test]
    fn new_window_on_folder_gets_prefix() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "proj:edit"), wi("@2", 2, "proj:term")];
        update(&mut model, Msg::WindowListLoaded(windows));
        // expanded folder is skipped, cursor lands on first child
        assert_eq!(model.cursor(), 1);
        // collapse the folder so cursor can sit on it
        model.set_cursor(0);
        model.mutate_tree(|tree| {
            if let TreeNode::Folder { expanded, .. } = &mut tree[0] {
                *expanded = false;
            }
        });
        // after collapse + rebuild, cursor stays on the now-collapsed folder
        assert_eq!(model.cursor(), 0);
        let cmds = update(&mut model, Msg::NewWindow);
        assert_new_window_name(&cmds, "proj:tab1");
    }

    #[test]
    fn new_window_on_child_gets_prefix() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "proj:edit"), wi("@2", 2, "proj:term")];
        update(&mut model, Msg::WindowListLoaded(windows));
        // move cursor to child window (index 1 = proj/edit)
        model.set_cursor(1);
        let cmds = update(&mut model, Msg::NewWindow);
        assert_new_window_name(&cmds, "proj:tab1");
    }

    #[test]
    fn new_window_on_root_window_is_plain() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "proj:edit"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(windows));
        // flat: [0]=folder "proj", [1]=window "edit", [2]=window "scratch"
        model.set_cursor(2);
        let cmds = update(&mut model, Msg::NewWindow);
        assert_new_window_name(&cmds, "tab1");
    }

    #[test]
    fn collect_window_names_preserves_nested_folder_paths() {
        let windows = vec![
            wi("@1", 1, "proj:sub:edit"),
            wi("@2", 2, "proj:sub:term"),
            wi("@3", 3, "scratch"),
        ];
        let tree = build_tree(&windows);
        let names = collect_window_names(&tree);
        assert!(names.iter().any(|n| n == "proj:sub:edit"));
        assert!(names.iter().any(|n| n == "proj:sub:term"));
        assert!(names.iter().any(|n| n == "scratch"));
    }

    #[test]
    fn new_window_name_uses_full_pending_nested_folder_path() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "proj:sub:tab1"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(windows));
        // flat: [0]=proj [1]=sub [2]=tab1 [3]=scratch
        model.set_cursor(2);
        model.pending_renames.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "proj:sub:renamed".to_string(),
                observed_count: 0,
            },
        );

        let cmds = update(&mut model, Msg::NewWindow);
        assert_new_window_name(&cmds, "proj:sub:tab2");
    }

    #[test]
    fn renaming_nested_window_to_same_name_is_noop() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "proj:sub:edit"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(windows));
        model.set_cursor(2);
        model.mode = Mode::Renaming {
            window_id: "@1".to_string(),
        };
        model.input_buffer = "proj:sub:edit".to_string();
        model.input_cursor = model.input_buffer.chars().count();

        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert_eq!(model.mode, Mode::Normal);
        assert!(model.pending_renames.is_empty());
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Render));
    }

    #[test]
    fn expected_focus_change_consumes_suppression_counter() {
        let mut model = test_model();
        model.ignore_window_changes = 2;
        model.pending_internal_focus_window = Some("@target".to_string());

        let first = update(&mut model, Msg::WindowFocusChanged("@target".to_string()));
        assert_ensure_only(&first);
        assert_eq!(model.ignore_window_changes, 1);
        assert_eq!(
            model.pending_internal_focus_window.as_deref(),
            Some("@target")
        );

        let second = update(&mut model, Msg::WindowFocusChanged("@target".to_string()));
        assert_ensure_only(&second);
        assert_eq!(model.ignore_window_changes, 0);
        assert_eq!(model.pending_internal_focus_window, None);
    }

    #[test]
    fn window_list_loaded_restores_cursor_by_window_id() {
        let mut model = test_model();
        let before = vec![
            wi("@1", 1, "root-a"),
            wi("@2", 2, "scratch"),
            wi("@3", 3, "root-b"),
        ];
        update(&mut model, Msg::WindowListLoaded(before));

        // Select window @2 in the flat list.
        model.set_cursor(1);

        let after = vec![
            wi("@1", 1, "root-a"),
            wi("@2", 2, "proj:new"),
            wi("@3", 3, "root-b"),
        ];
        update(&mut model, Msg::WindowListLoaded(after));

        assert_eq!(model.cursor(), 2);
        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@2")
        );
    }

    #[test]
    fn window_list_loaded_preserves_renaming_mode_when_target_exists() {
        let mut model = test_model();
        let before = vec![wi("@1", 1, "dev"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(before));

        // Cursor can drift for any reason (e.g. async refresh timing), so keep a stale cursor.
        model.set_cursor(1);
        model.mode = Mode::Renaming {
            window_id: "@1".to_string(),
        };

        let after = vec![wi("@1", 1, "new:new"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(after));

        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@1")
        );
        assert_eq!(model.cursor(), 1);
        // Renaming mode preserved: target window @1 still exists
        assert!(matches!(model.mode, Mode::Renaming { .. }));
    }

    #[test]
    fn window_list_loaded_cancels_renaming_mode_when_target_gone() {
        let mut model = test_model();
        let before = vec![wi("@1", 1, "dev"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(before));

        model.mode = Mode::Renaming {
            window_id: "@1".to_string(),
        };

        // @1 is gone from the new list
        let after = vec![wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(after));

        assert_eq!(model.mode, Mode::Normal);
    }

    #[test]
    fn window_list_loaded_prefers_pending_rename_id_over_cursor() {
        let mut model = test_model();
        let before = vec![wi("@1", 1, "dev"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(before));
        model.set_cursor(2);
        model.pending_renames.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "dev:new".to_string(),
                observed_count: 0,
            },
        );
        model.pending_rename_last_window_id = Some("@1".to_string());

        let after = vec![wi("@1", 1, "new:new"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(after));

        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@1")
        );
        assert_eq!(model.cursor(), 1);
        assert!(model.pending_renames.contains_key("@1"));
    }

    #[test]
    fn new_window_name_uses_pending_rename_folder_when_context_collapses() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "new:old"), wi("@2", 2, "dev")];
        update(&mut model, Msg::WindowListLoaded(windows));
        model.set_cursor(1);
        model.pending_renames.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 0,
            },
        );

        let cmds = update(&mut model, Msg::NewWindow);
        assert_new_window_name(&cmds, "new:tab1");
    }

    #[test]
    fn window_list_loaded_keeps_pending_rename_if_target_missing_once() {
        let mut model = test_model();
        let initial = vec![wi("@1", 1, "new:new"), wi("@2", 2, "dev")];
        update(&mut model, Msg::WindowListLoaded(initial));
        model.pending_renames.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 1,
            },
        );
        model.pending_rename_last_window_id = Some("@1".to_string());

        let missing = vec![wi("@2", 2, "dev"), wi("@3", 3, "scratch")];
        update(&mut model, Msg::WindowListLoaded(missing));

        assert!(model.pending_renames.contains_key("@1"));
    }

    #[test]
    fn window_list_loaded_clears_pending_rename_after_repeated_missing_updates() {
        let mut model = test_model();
        let initial = vec![wi("@1", 1, "new:new")];
        update(&mut model, Msg::WindowListLoaded(initial));
        model.pending_renames.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 1,
            },
        );
        model.pending_rename_last_window_id = Some("@1".to_string());

        for _ in 0..5 {
            let missing = vec![wi("@2", 2, "dev")];
            update(&mut model, Msg::WindowListLoaded(missing));
        }

        assert!(!model.pending_renames.contains_key("@1"));
        assert_eq!(model.pending_rename_last_window_id, None);
    }

    #[test]
    fn window_list_loaded_keeps_pending_rename_when_target_name_stable() {
        let mut model = test_model();
        let initial = vec![wi("@1", 1, "new:new"), wi("@2", 2, "dev")];
        update(&mut model, Msg::WindowListLoaded(initial));
        model.pending_renames.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 5,
            },
        );

        let stable = vec![wi("@1", 1, "new:new"), wi("@2", 2, "dev")];
        update(&mut model, Msg::WindowListLoaded(stable));

        assert!(model.pending_renames.contains_key("@1"));
    }

    #[test]
    fn window_renamed_pending_mismatch_triggers_immediate_rename() {
        let mut model = test_model();
        model.pending_renames.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 3,
            },
        );

        let cmds = update(
            &mut model,
            Msg::WindowRenamed {
                window_id: "@1".to_string(),
                name: "dev".to_string(),
            },
        );

        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            &cmds[0],
            Cmd::RenameWindow { id, name } if id == "@1" && name == "new:new"
        ));
        assert_eq!(
            model.pending_renames.get("@1").map(|p| p.observed_count),
            Some(0)
        );
    }

    #[test]
    fn window_renamed_pending_match_does_not_trigger_refresh() {
        let mut model = test_model();
        model.pending_renames.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 3,
            },
        );

        let cmds = update(
            &mut model,
            Msg::WindowRenamed {
                window_id: "@1".to_string(),
                name: "new:new".to_string(),
            },
        );

        assert!(cmds.is_empty());
        assert_eq!(
            model.pending_renames.get("@1").map(|p| p.observed_count),
            Some(4)
        );
    }

    #[test]
    fn window_renamed_non_pending_triggers_refresh() {
        let mut model = test_model();
        let cmds = update(
            &mut model,
            Msg::WindowRenamed {
                window_id: "@2".to_string(),
                name: "dev".to_string(),
            },
        );

        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::ListWindows));
    }

    #[test]
    fn window_list_loaded_corrects_all_pending_renames() {
        let mut model = test_model();
        model.pending_renames.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 0,
            },
        );
        model.pending_renames.insert(
            "@2".to_string(),
            PendingRename {
                target_name: "app:main".to_string(),
                observed_count: 0,
            },
        );
        model.pending_rename_last_window_id = Some("@2".to_string());

        let cmds = update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@1", 1, "dev"), wi("@2", 2, "app:dev")]),
        );

        let rename_count = cmds
            .iter()
            .filter(|cmd| matches!(cmd, Cmd::RenameWindow { .. }))
            .count();
        assert_eq!(rename_count, 2);
        assert!(cmds.iter().any(|cmd| {
            matches!(
                cmd,
                Cmd::RenameWindow { id, name } if id == "@1" && name == "new:new"
            )
        }));
        assert!(cmds.iter().any(|cmd| {
            matches!(
                cmd,
                Cmd::RenameWindow { id, name } if id == "@2" && name == "app:main"
            )
        }));
        assert!(matches!(cmds.last(), Some(Cmd::Render)));
    }
}
