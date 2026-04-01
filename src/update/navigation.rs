use crate::cmd::Cmd;
use crate::model::{Mode, Model, MoveSubject, SelectionTarget};
use crate::tree::{
    find_folder_flat_index_by_path, find_parent_folder, find_window_flat_index_by_id, get_node_mut,
    move_node_to_position, next_visible_item, prev_visible_item, toggle_expand,
    visible_item_indices, window_ids_in_order, FlatNodeKind, TreeNode, WindowInfo,
};

use super::input::{clear_input, set_input};
use super::naming::{reconstruct_folder_full_name, reconstruct_full_name};

pub(super) fn reset_to_normal_mode(model: &mut Model) {
    model.mode = Mode::Normal;
    clear_input(model);
    model.clear_reorder_preview();
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
        Mode::Moving { subject } => match subject {
            MoveSubject::Item(SelectionTarget::Window(window_id)) => {
                !windows.iter().any(|w| w.id == *window_id)
            }
            MoveSubject::Item(SelectionTarget::Folder(folder_name))
            | MoveSubject::Project { folder_name } => {
                let folder_prefix = format!("{folder_name}:");
                windows.iter().all(|w| !w.name.starts_with(&folder_prefix))
            }
        },
        Mode::Normal | Mode::CreatingProject => false,
    };

    if should_reset {
        reset_to_normal_mode(model);
    }
}

pub(super) fn handle_move_item(model: &mut Model) -> Vec<Cmd> {
    let subject = match model.selected_selection_target() {
        Some(target) => MoveSubject::Item(target),
        None => return vec![],
    };
    model.begin_reorder_preview();
    model.mode = Mode::Moving { subject };
    vec![Cmd::Render]
}

pub(super) fn handle_move_project(model: &mut Model) -> Vec<Cmd> {
    let item = match model.flat_items().get(model.cursor()) {
        Some(item) => item.clone(),
        None => return vec![],
    };

    let folder_name = match item.kind {
        FlatNodeKind::Folder => reconstruct_folder_full_name(model, &item.path),
        FlatNodeKind::Window => {
            let info = match model.selected_window_info() {
                Some(info) => info,
                None => return vec![],
            };
            let full_name = reconstruct_full_name(model, model.cursor(), &info.name);
            match full_name.split_once(':') {
                Some((folder, _)) => folder.to_string(),
                None => return vec![],
            }
        }
    };

    let root_folder = folder_name
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string();
    if root_folder.is_empty() {
        return vec![];
    }

    model.begin_reorder_preview();
    model.mode = Mode::Moving {
        subject: MoveSubject::Project {
            folder_name: root_folder.clone(),
        },
    };
    if let Some(index) =
        find_folder_flat_index_by_path(model.flat_items(), model.tree(), &root_folder)
    {
        model.set_cursor(index);
    }
    vec![Cmd::Render]
}

pub(super) fn cancel_move(model: &mut Model) -> Vec<Cmd> {
    model.cancel_reorder_preview();
    model.mode = Mode::Normal;
    clear_input(model);
    vec![Cmd::Render]
}

pub(super) fn handle_move_enter(model: &mut Model) -> Vec<Cmd> {
    let subject = match model.mode.clone() {
        Mode::Moving { subject } => subject,
        _ => return vec![],
    };

    let selection = match &subject {
        MoveSubject::Item(target) => target.clone(),
        MoveSubject::Project { folder_name } => SelectionTarget::Folder(folder_name.clone()),
    };

    let order = window_ids_in_order(model.tree());

    let original_order = model
        .reorder
        .snapshot
        .as_ref()
        .map(|snapshot| window_ids_in_order(&snapshot.tree));

    if original_order.as_ref() == Some(&order) {
        model.mode = Mode::Normal;
        model.clear_reorder_preview();
        model.info_message = Some("move: already in that position".to_string());
        return vec![Cmd::Render];
    }

    model.mode = Mode::Normal;
    model.clear_reorder_preview();
    model.reorder.pending_selection = Some(selection.clone());
    vec![Cmd::ReorderWindows { order, selection }]
}

pub(super) fn handle_move_prev(model: &mut Model) -> Vec<Cmd> {
    shift_move(model, -1)
}

pub(super) fn handle_move_next(model: &mut Model) -> Vec<Cmd> {
    shift_move(model, 1)
}

fn shift_move(model: &mut Model, delta: isize) -> Vec<Cmd> {
    let subject = match model.mode.clone() {
        Mode::Moving { subject } => subject,
        _ => return vec![],
    };

    let Some((path, sibling_count, current_position)) = move_subject_context(model, &subject)
    else {
        return vec![];
    };

    let next_position = if delta < 0 {
        current_position.checked_sub(1)
    } else {
        let candidate = current_position + 1;
        (candidate <= sibling_count).then_some(candidate)
    };

    let Some(next_position) = next_position else {
        return vec![];
    };

    model.mutate_tree(|tree| {
        let _ = move_node_to_position(tree, &path, next_position);
    });

    if let Some(target_index) = selection_flat_index(model, &subject) {
        model.set_cursor(target_index);
    }

    vec![Cmd::Render]
}

fn move_subject_context(
    model: &Model,
    subject: &MoveSubject,
) -> Option<(Vec<usize>, usize, usize)> {
    let path = selection_path(model, subject)?;
    let current_position = path.last().copied()? + 1;
    let sibling_count = siblings_len(model.tree(), &path)?;
    Some((path, sibling_count, current_position))
}

fn selection_path(model: &Model, subject: &MoveSubject) -> Option<Vec<usize>> {
    match subject {
        MoveSubject::Item(SelectionTarget::Window(window_id)) => {
            let index = find_window_flat_index_by_id(model.flat_items(), model.tree(), window_id)?;
            Some(model.flat_items().get(index)?.path.clone())
        }
        MoveSubject::Item(SelectionTarget::Folder(folder_name))
        | MoveSubject::Project { folder_name } => {
            let index =
                find_folder_flat_index_by_path(model.flat_items(), model.tree(), folder_name)?;
            Some(model.flat_items().get(index)?.path.clone())
        }
    }
}

fn selection_flat_index(model: &Model, subject: &MoveSubject) -> Option<usize> {
    match subject {
        MoveSubject::Item(SelectionTarget::Window(window_id)) => {
            find_window_flat_index_by_id(model.flat_items(), model.tree(), window_id)
        }
        MoveSubject::Item(SelectionTarget::Folder(folder_name))
        | MoveSubject::Project { folder_name } => {
            find_folder_flat_index_by_path(model.flat_items(), model.tree(), folder_name)
        }
    }
}

fn siblings_len(nodes: &[TreeNode], path: &[usize]) -> Option<usize> {
    let (_index, parent_path) = path.split_last()?;
    if parent_path.is_empty() {
        return Some(nodes.len());
    }

    match crate::tree::get_node(nodes, parent_path).ok()? {
        TreeNode::Folder { children, .. } => Some(children.len()),
        TreeNode::Window { .. } => None,
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

pub(super) fn handle_jump_to_index(model: &mut Model, n: usize) -> Vec<Cmd> {
    if n == 0 {
        return vec![];
    }

    let visible = visible_item_indices(model.flat_items());
    if let Some(&target) = visible.get(n - 1) {
        model.set_cursor(target);
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
