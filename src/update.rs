use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::cmd::Cmd;
use crate::model::{Mode, Model, PreviewState};
use crate::msg::Msg;
use crate::tree::{
    build_tree, find_parent_folder, get_node_mut, next_visible_item, prev_visible_item,
    toggle_expand, FlatNodeKind, TreeNode,
};

pub fn update(model: &mut Model, msg: Msg) -> Vec<Cmd> {
    // Clear error on any user action
    if !matches!(
        msg,
        Msg::Tick
            | Msg::WindowAdded(_)
            | Msg::WindowClosed(_)
            | Msg::WindowRenamed(_, _)
            | Msg::WindowListLoaded(_)
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
        Msg::NewWindow => vec![Cmd::NewWindow {
            name: "new-window".to_string(),
        }],
        Msg::RenameWindow => handle_rename_window(model),
        Msg::CloseWindow => handle_close_window(model),
        Msg::WindowFocusChanged(window_id) => {
            if model.ignore_window_changes > 0 {
                model.ignore_window_changes -= 1;
                return vec![Cmd::EnsureSidebarWidth];
            }
            if window_id == model.sidebar_window_id {
                return vec![Cmd::EnsureSidebarWidth];
            }
            // User explicitly switched windows; clear preview state
            model.preview = PreviewState::Home;
            vec![
                Cmd::FollowToWindow { window_id },
                Cmd::EnsureSidebarWidth,
                Cmd::ListWindows,
            ]
        }
        Msg::WindowAdded(_) | Msg::WindowClosed(_) | Msg::WindowRenamed(_, _) => {
            vec![Cmd::ListWindows]
        }
        Msg::WindowListLoaded(windows) => {
            // Save folder expanded state before rebuilding
            let expanded_state = collect_folder_expanded(&model.tree);
            model.tree = build_tree(&windows);
            restore_folder_expanded(&mut model.tree, &expanded_state);

            // If in Renaming/ConfirmClose mode, check target still exists
            match &model.mode {
                Mode::Renaming { window_id } | Mode::ConfirmClose { window_id } => {
                    let still_exists = windows.iter().any(|w| w.id == *window_id);
                    if !still_exists {
                        model.mode = Mode::Normal;
                        model.input_buffer.clear();
                    }
                }
                Mode::Normal => {}
            }

            model.rebuild_flat();
            vec![Cmd::Render]
        }
        Msg::Key(event) => handle_key(model, event),
        Msg::Tick => vec![],
        Msg::Quit => {
            model.should_quit = true;
            vec![Cmd::RestorePreview, Cmd::Quit]
        }
    }
}

fn handle_cursor_up(model: &mut Model) -> Vec<Cmd> {
    if let Some(prev) = prev_visible_item(&model.flat_items, model.cursor) {
        model.cursor = prev;
        preview_current_item(model)
    } else {
        vec![]
    }
}

fn handle_cursor_down(model: &mut Model) -> Vec<Cmd> {
    if let Some(next) = next_visible_item(&model.flat_items, model.cursor) {
        model.cursor = next;
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
    let item = match model.flat_items.get(model.cursor) {
        Some(item) => item.clone(),
        None => return vec![],
    };

    match item.kind {
        FlatNodeKind::Folder => {
            if let Ok(node) = get_node_mut(&mut model.tree, &item.path) {
                toggle_expand(node);
            }
            model.rebuild_flat();
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
    let item = match model.flat_items.get(model.cursor) {
        Some(item) => item.clone(),
        None => return vec![],
    };

    match item.kind {
        FlatNodeKind::Folder => {
            if let Ok(TreeNode::Folder { expanded, .. }) = get_node_mut(&mut model.tree, &item.path)
            {
                if *expanded {
                    *expanded = false;
                    model.rebuild_flat();
                    return vec![Cmd::Render];
                }
            }
            if let Some(parent_idx) = find_parent_folder(&model.flat_items, model.cursor) {
                model.cursor = parent_idx;
                vec![Cmd::Render]
            } else {
                vec![]
            }
        }
        FlatNodeKind::Window => {
            if let Some(parent_idx) = find_parent_folder(&model.flat_items, model.cursor) {
                model.cursor = parent_idx;
                vec![Cmd::Render]
            } else {
                vec![]
            }
        }
    }
}

fn handle_toggle_folder(model: &mut Model) -> Vec<Cmd> {
    let item = match model.flat_items.get(model.cursor) {
        Some(item) => item.clone(),
        None => return vec![],
    };

    if item.kind == FlatNodeKind::Folder {
        if let Ok(node) = get_node_mut(&mut model.tree, &item.path) {
            toggle_expand(node);
        }
        model.rebuild_flat();
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
                let new_name = model.input_buffer.clone();
                model.mode = Mode::Normal;
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
