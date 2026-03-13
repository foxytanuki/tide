use crate::cmd::Cmd;
use crate::model::{Mode, Model};
use crate::tree::{
    find_parent_folder, get_node_mut, next_visible_item, prev_visible_item, toggle_expand,
    FlatNodeKind, TreeNode, WindowInfo,
};

use super::input::{clear_input, set_input};
use super::naming::{reconstruct_folder_full_name, reconstruct_full_name};

pub(super) fn reset_to_normal_mode(model: &mut Model) {
    model.mode = Mode::Normal;
    clear_input(model);
}

pub(super) fn exit_to_normal_mode(model: &mut Model) -> Vec<Cmd> {
    reset_to_normal_mode(model);
    vec![Cmd::Render]
}

pub(super) fn clear_mode_if_missing_target(model: &mut Model, windows: &[WindowInfo]) {
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

pub(super) fn handle_cursor_up(model: &mut Model) -> Vec<Cmd> {
    if let Some(prev) = prev_visible_item(model.flat_items(), model.cursor()) {
        model.set_cursor(prev);
        preview_current_item(model)
    } else {
        vec![]
    }
}

pub(super) fn handle_cursor_down(model: &mut Model) -> Vec<Cmd> {
    if let Some(next) = next_visible_item(model.flat_items(), model.cursor()) {
        model.set_cursor(next);
        preview_current_item(model)
    } else {
        vec![]
    }
}

pub(super) fn preview_current_item(model: &mut Model) -> Vec<Cmd> {
    let window_id = model.selected_window_info().map(|info| info.id.clone());
    if let Some(id) = window_id {
        model.sidebar.pending_preview_id = Some(id.clone());
        vec![Cmd::PreviewWindow { id }, Cmd::Render]
    } else {
        model.sidebar.pending_preview_id = None;
        vec![Cmd::Render]
    }
}

pub(super) fn handle_select_item(model: &mut Model) -> Vec<Cmd> {
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

pub(super) fn handle_mouse_click(model: &mut Model, index: usize) -> Vec<Cmd> {
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
                handle_select_item(model)
            } else {
                model.set_cursor(index);
                preview_current_item(model)
            }
        }
    }
}

pub(super) fn handle_collapse_or_parent(model: &mut Model) -> Vec<Cmd> {
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

pub(super) fn handle_toggle_folder(model: &mut Model) -> Vec<Cmd> {
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

pub(super) fn handle_rename_window(model: &mut Model) -> Vec<Cmd> {
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

pub(super) fn handle_close_window(model: &mut Model) -> Vec<Cmd> {
    let window_id = match model.selected_window_info() {
        Some(info) => info.id.clone(),
        None => return vec![],
    };
    model.mode = Mode::ConfirmClose { window_id };
    vec![Cmd::Render]
}
