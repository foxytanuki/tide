use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tracing::debug;

use crate::cmd::Cmd;
use crate::model::{Mode, Model, PendingRename, PreviewState};
use crate::msg::Msg;
use crate::tree::{
    build_tree, find_parent_folder, get_node, get_node_mut, next_visible_item, prev_visible_item,
    toggle_expand, FlatNodeKind, TreeNode,
};

pub fn update(model: &mut Model, msg: Msg) -> Vec<Cmd> {
    const MISSING_PENDING_RENAME_THRESHOLD: u8 = 6;

    // Clear error on any user action
    if !matches!(
        msg,
        Msg::WindowChanged | Msg::WindowRenamed { .. } | Msg::WindowListLoaded(_)
    ) {
        model.error_message = None;
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
            let name = generate_project_name(model);
            vec![Cmd::NewWindow {
                name: format!("{}:main", name),
            }]
        }
        Msg::RenameWindow => handle_rename_window(model),
        Msg::CloseWindow => handle_close_window(model),
        Msg::WindowFocusChanged(window_id) => {
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

            // User explicitly switched windows; clear preview state
            model.preview = PreviewState::Home;
            model.ignore_window_changes = 0;
            model.pending_internal_focus_window = None;
            vec![
                Cmd::FollowToWindow { window_id },
                Cmd::EnsureSidebarWidth,
                Cmd::ListWindows,
            ]
        }
        Msg::WindowChanged => vec![Cmd::ListWindows],
        Msg::WindowRenamed { window_id, name } => {
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
        Msg::WindowListLoaded(windows) => {
            // Save folder expanded state before rebuilding
            let expanded_state = collect_folder_expanded(model.tree());
            let mut followup_cmds = Vec::new();
            let mut selected_window_id = model
                .pending_rename_last_window_id
                .as_ref()
                .filter(|id| model.pending_renames.contains_key(*id))
                .cloned()
                .or_else(|| match &model.mode {
                    Mode::Renaming { window_id } => Some(window_id.clone()),
                    _ => model.selected_window_info().map(|info| info.id.clone()),
                });

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
                    selected_window_id = None;
                }
            }

            let mut new_tree = build_tree(&windows);
            restore_folder_expanded(&mut new_tree, &expanded_state);
            let selected_ref = selected_window_id.as_deref();
            model.replace_tree_preserve_selection(new_tree, selected_ref);

            let mut stale_pending_ids = Vec::new();
            for (id, pending) in &mut model.pending_renames {
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

            for stale_id in stale_pending_ids {
                debug!(id = stale_id.as_str(), "pending rename stale (missing) and cleared");
                model.pending_renames.remove(&stale_id);
                if model.pending_rename_last_window_id.as_deref() == Some(stale_id.as_str()) {
                    model.pending_rename_last_window_id = None;
                }
            }

            if let Some(last_id) = model.pending_rename_last_window_id.as_deref() {
                if !model.pending_renames.contains_key(last_id) {
                    model.pending_rename_last_window_id = None;
                }
            }

            debug!(
                cursor = model.cursor(),
                selected = selected_ref,
                "window list selection restored"
            );

            // If in Renaming/ConfirmClose mode, check target still exists
            match &model.mode {
                Mode::Renaming { window_id } | Mode::ConfirmClose { window_id } => {
                    let still_exists = windows.iter().any(|w| w.id == *window_id);
                    if !still_exists {
                        model.mode = Mode::Normal;
                        model.input_buffer.clear();
                    } else if matches!(model.mode, Mode::Renaming { .. }) {
                        model.mode = Mode::Normal;
                        model.input_buffer.clear();
                    }
                }
                Mode::Normal => {}
            }

            if followup_cmds.is_empty() {
                vec![Cmd::Render]
            } else {
                followup_cmds.push(Cmd::Render);
                followup_cmds
            }
        }
        Msg::Key(event) => handle_key(model, event),
        Msg::Quit => {
            model.should_quit = true;
            vec![Cmd::RestorePreview, Cmd::Quit]
        }
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
fn preview_current_item(model: &Model) -> Vec<Cmd> {
    if let Some(info) = model.selected_window_info() {
        vec![
            Cmd::PreviewWindow {
                id: info.id.clone(),
            },
            Cmd::Render,
        ]
    } else {
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
    let info = match model.selected_window_info() {
        Some(info) => (info.id.clone(), info.name.clone()),
        None => return vec![],
    };
    model.input_buffer = info.1;
    model.mode = Mode::Renaming { window_id: info.0 };
    vec![Cmd::Render]
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
        Mode::ConfirmClose { .. } => handle_confirm_close_key(model, event),
    }
}

fn is_plain_key(event: &KeyEvent) -> bool {
    event.modifiers.is_empty() || event.modifiers == KeyModifiers::SHIFT
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
        KeyCode::Char('q') => update(model, Msg::Quit),
        _ => vec![],
    }
}

fn handle_renaming_key(model: &mut Model, event: KeyEvent) -> Vec<Cmd> {
    // Allow Shift (for uppercase), reject Ctrl/Alt/Super
    if !is_plain_key(&event) {
        return vec![];
    }
    match event.code {
        KeyCode::Enter => {
            let mode = model.mode.clone();
            if let Mode::Renaming { window_id } = mode {
                let new_name = model.input_buffer.trim().to_string();
                if new_name.is_empty() {
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
                model.input_buffer.clear();
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
        KeyCode::Esc => {
            model.mode = Mode::Normal;
            model.input_buffer.clear();
            vec![Cmd::Render]
        }
        KeyCode::Backspace => {
            model.input_buffer.pop();
            vec![Cmd::Render]
        }
        KeyCode::Char(c) => {
            model.input_buffer.push(c);
            vec![Cmd::Render]
        }
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

/// Determine the name for a new window based on cursor context.
/// If cursor is on/in a folder, prefix with the folder name (e.g. "proj:new").
fn determine_new_window_name(model: &Model) -> String {
    let item = match model.flat_items().get(model.cursor()) {
        Some(item) => item,
        None => return "new".to_string(),
    };

    if let Some(selected) = model.selected_window_info() {
        if let Some(pending) = model.pending_renames.get(selected.id.as_str()) {
            if let Some((folder, _)) = pending.target_name.split_once(':') {
                let generated = format!("{}:new", folder);
                debug!(
                    cursor = model.cursor(),
                    generated,
                    "new window name from pending rename context"
                );
                return generated;
            }
        }
    }

    match item.kind {
        FlatNodeKind::Folder => {
            if let Ok(TreeNode::Folder { name, .. }) = get_node(model.tree(), &item.path) {
                let generated = format!("{}:new", name);
                debug!(
                    cursor = model.cursor(),
                    generated,
                    "new window name from folder"
                );
                generated
            } else {
                debug!(cursor = model.cursor(), "new window name fallback");
                "new".to_string()
            }
        }
        FlatNodeKind::Window => {
            if let Some(parent_idx) = find_parent_folder(model.flat_items(), model.cursor()) {
                if let Some(parent_item) = model.flat_items().get(parent_idx) {
                    if let Ok(TreeNode::Folder { name, .. }) =
                        get_node(model.tree(), &parent_item.path)
                    {
                        let generated = format!("{}:new", name);
                        debug!(
                            cursor = model.cursor(),
                            generated,
                            "new window name from parent folder"
                        );
                        return generated;
                    }
                }
            }
            debug!(
                cursor = model.cursor(),
                "new window name fallback from window w/o folder"
            );
            "new".to_string()
        }
    }
}

/// Generate a unique project name (project1, project2, ...).
fn generate_project_name(model: &Model) -> String {
    for i in 1.. {
        let candidate = format!("project{}", i);
        let exists = model.tree().iter().any(|node| {
            matches!(node, TreeNode::Folder { name, .. } if name == &candidate)
        });
        if !exists {
            return candidate;
        }
    }
    unreachable!()
}

/// Collect folder name → expanded state from the tree
fn collect_folder_expanded(nodes: &[TreeNode]) -> HashMap<String, bool> {
    let mut map = HashMap::new();
    for node in nodes {
        if let TreeNode::Folder {
            name,
            expanded,
            children,
        } = node
        {
            map.insert(name.clone(), *expanded);
            // Recurse into children for nested folders
            let child_map = collect_folder_expanded(children);
            map.extend(child_map);
        }
    }
    map
}

/// Restore folder expanded state after tree rebuild
fn restore_folder_expanded(nodes: &mut [TreeNode], state: &HashMap<String, bool>) {
    for node in nodes {
        if let TreeNode::Folder {
            name,
            expanded,
            children,
        } = node
        {
            if let Some(&was_expanded) = state.get(name.as_str()) {
                *expanded = was_expanded;
            }
            restore_folder_expanded(children, state);
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
        let windows = vec![
            wi("@1", 1, "proj:edit"),
            wi("@2", 2, "proj:term"),
        ];
        update(&mut model, Msg::WindowListLoaded(windows));
        // cursor=0 is the folder "proj"
        assert_eq!(model.cursor(), 0);
        let cmds = update(&mut model, Msg::NewWindow);
        assert_new_window_name(&cmds, "proj:new");
    }

    #[test]
    fn new_window_on_child_gets_prefix() {
        let mut model = test_model();
        let windows = vec![
            wi("@1", 1, "proj:edit"),
            wi("@2", 2, "proj:term"),
        ];
        update(&mut model, Msg::WindowListLoaded(windows));
        // move cursor to child window (index 1 = proj/edit)
        model.set_cursor(1);
        let cmds = update(&mut model, Msg::NewWindow);
        assert_new_window_name(&cmds, "proj:new");
    }

    #[test]
    fn new_window_on_root_window_is_plain() {
        let mut model = test_model();
        let windows = vec![
            wi("@1", 1, "proj:edit"),
            wi("@2", 2, "scratch"),
        ];
        update(&mut model, Msg::WindowListLoaded(windows));
        // flat: [0]=folder "proj", [1]=window "edit", [2]=window "scratch"
        model.set_cursor(2);
        let cmds = update(&mut model, Msg::NewWindow);
        assert_new_window_name(&cmds, "new");
    }

    #[test]
    fn expected_focus_change_consumes_suppression_counter() {
        let mut model = test_model();
        model.ignore_window_changes = 2;
        model.pending_internal_focus_window = Some("@target".to_string());

        let first = update(&mut model, Msg::WindowFocusChanged("@target".to_string()));
        assert_ensure_only(&first);
        assert_eq!(model.ignore_window_changes, 1);
        assert_eq!(model.pending_internal_focus_window.as_deref(), Some("@target"));

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
        assert_eq!(model.selected_window_info().map(|w| w.id.as_str()), Some("@2"));
    }

    #[test]
    fn window_list_loaded_uses_renaming_mode_window_id_even_if_cursor_stale() {
        let mut model = test_model();
        let before = vec![
            wi("@1", 1, "dev"),
            wi("@2", 2, "scratch"),
        ];
        update(&mut model, Msg::WindowListLoaded(before));

        // Cursor can drift for any reason (e.g. async refresh timing), so keep a stale cursor.
        model.set_cursor(1);
        model.mode = Mode::Renaming {
            window_id: "@1".to_string(),
        };

        let after = vec![
            wi("@1", 1, "new:new"),
            wi("@2", 2, "scratch"),
        ];
        update(&mut model, Msg::WindowListLoaded(after));

        assert_eq!(model.selected_window_info().map(|w| w.id.as_str()), Some("@1"));
        assert_eq!(model.cursor(), 1);
        assert_eq!(model.mode, Mode::Normal);
    }

    #[test]
    fn window_list_loaded_prefers_pending_rename_id_over_cursor() {
        let mut model = test_model();
        let before = vec![
            wi("@1", 1, "dev"),
            wi("@2", 2, "scratch"),
        ];
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

        let after = vec![
            wi("@1", 1, "new:new"),
            wi("@2", 2, "scratch"),
        ];
        update(&mut model, Msg::WindowListLoaded(after));

        assert_eq!(model.selected_window_info().map(|w| w.id.as_str()), Some("@1"));
        assert_eq!(model.cursor(), 1);
        assert!(model.pending_renames.contains_key("@1"));
    }

    #[test]
    fn new_window_name_uses_pending_rename_folder_when_context_collapses() {
        let mut model = test_model();
        let windows = vec![
            wi("@1", 1, "new:old"),
            wi("@2", 2, "dev"),
        ];
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
        assert_new_window_name(&cmds, "new:new");
    }

    #[test]
    fn window_list_loaded_keeps_pending_rename_if_target_missing_once() {
        let mut model = test_model();
        let initial = vec![
            wi("@1", 1, "new:new"),
            wi("@2", 2, "dev"),
        ];
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
        let initial = vec![
            wi("@1", 1, "new:new"),
            wi("@2", 2, "dev"),
        ];
        update(&mut model, Msg::WindowListLoaded(initial));
        model.pending_renames.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 5,
            },
        );

        let stable = vec![
            wi("@1", 1, "new:new"),
            wi("@2", 2, "dev"),
        ];
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
