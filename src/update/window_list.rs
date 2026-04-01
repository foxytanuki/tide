use tracing::debug;

use crate::cmd::Cmd;
use crate::model::{Mode, Model, PreviewState, SelectionTarget};
use crate::tree::{build_tree, WindowInfo};

use super::naming::{collect_folder_expanded, restore_folder_expanded};
use super::navigation::clear_mode_if_missing_target;

pub(super) fn handle_window_focus_changed(model: &mut Model, window_id: String) -> Vec<Cmd> {
    if model.sidebar.ignore_window_changes > 0 {
        let expected = model.sidebar.pending_internal_focus_window.as_deref();
        if expected == Some(window_id.as_str()) {
            model.sidebar.ignore_window_changes -= 1;
            if model.sidebar.ignore_window_changes == 0 {
                model.sidebar.pending_internal_focus_window = None;
            }
            return vec![Cmd::EnsureSidebarWidth];
        }
    }

    if window_id == model.sidebar.window_id {
        return vec![Cmd::EnsureSidebarWidth];
    }

    model.sidebar.preview = PreviewState::Home;
    model.sidebar.ignore_window_changes = 0;
    model.sidebar.pending_internal_focus_window = None;
    vec![
        Cmd::FollowToWindow { window_id },
        Cmd::EnsureSidebarWidth,
        Cmd::ListWindows,
    ]
}

pub(super) fn handle_window_renamed(
    model: &mut Model,
    window_id: String,
    name: String,
) -> Vec<Cmd> {
    if let Some(pending) = model.renames.pending.get_mut(&window_id) {
        let expected_name = pending.target_name.clone();
        if name != expected_name {
            debug!(
                id = window_id.as_str(),
                current = name.as_str(),
                expected = expected_name.as_str(),
                "pending rename mismatch from event, correcting immediately"
            );
            pending.observed_count = 0;
            model.renames.last_window_id = Some(window_id.clone());
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

pub(super) fn handle_window_list_loaded(
    model: &mut Model,
    mut windows: Vec<WindowInfo>,
) -> Vec<Cmd> {
    let expanded_state = collect_folder_expanded(model.tree());
    let mut selected_target = derive_selected_target(model);

    let selected_exists = match selected_target.as_ref() {
        Some(SelectionTarget::Window(id)) => windows.iter().any(|w| w.id == *id),
        Some(SelectionTarget::Folder(folder)) => {
            let folder_prefix = format!("{folder}:");
            windows.iter().any(|w| w.name.starts_with(&folder_prefix))
        }
        None => false,
    };
    debug!(
        pending_count = model.renames.pending.len(),
        pending_last = model.renames.last_window_id.as_deref(),
        mode = ?model.mode,
        selected = ?selected_target,
        exists = selected_exists,
        cursor = model.cursor(),
        "window list loaded"
    );

    bump_last_pending_if_missing(model, &windows, &mut selected_target);
    windows.sort_by_key(|window| window.index);

    let mut new_tree = build_tree(&windows);
    restore_folder_expanded(&mut new_tree, &expanded_state);
    let selected_ref = selected_target.as_ref();
    model.replace_tree_preserve_selection(new_tree, selected_ref);
    model.reorder.pending_selection = None;

    let (mut followup_cmds, stale_pending_ids) = reconcile_pending_renames(model, &windows);
    clear_stale_pending_renames(model, stale_pending_ids);
    clear_orphaned_last_pending_rename(model);

    debug!(
        cursor = model.cursor(),
        selected = ?selected_ref,
        "window list selection restored"
    );

    clear_mode_if_missing_target(model, &windows);
    followup_cmds.push(Cmd::Render);
    followup_cmds
}

fn derive_selected_target(model: &Model) -> Option<SelectionTarget> {
    model
        .reorder
        .pending_selection
        .clone()
        .or_else(|| {
            model
                .renames
                .last_window_id
                .as_ref()
                .filter(|id| model.renames.pending.contains_key(*id))
                .cloned()
                .map(SelectionTarget::Window)
        })
        .or_else(|| match &model.mode {
            Mode::Renaming { window_id } => Some(SelectionTarget::Window(window_id.clone())),
            _ => model.selected_selection_target(),
        })
}

fn bump_last_pending_if_missing(
    model: &mut Model,
    windows: &[WindowInfo],
    selected_target: &mut Option<SelectionTarget>,
) {
    model
        .renames
        .last_window_id
        .clone()
        .into_iter()
        .for_each(|last_id| {
            let last_exists = windows.iter().any(|w| w.id == last_id);
            if !last_exists {
                if let Some(pending) = model.renames.pending.get_mut(&last_id) {
                    pending.observed_count = pending.observed_count.saturating_add(1);
                    debug!(
                        id = last_id.as_str(),
                        observed = pending.observed_count,
                        "latest pending rename target missing from window list"
                    );
                }
                *selected_target = None;
            }
        });
}

fn reconcile_pending_renames(model: &mut Model, windows: &[WindowInfo]) -> (Vec<Cmd>, Vec<String>) {
    let mut followup_cmds = Vec::new();
    let mut stale_pending_ids = Vec::new();

    for (id, pending) in &mut model.renames.pending {
        if model.renames.last_window_id.as_deref() == Some(id.as_str())
            && !windows.iter().any(|w| w.id == *id)
        {
            if pending.observed_count >= super::MISSING_PENDING_RENAME_THRESHOLD {
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
            if pending.observed_count >= super::MISSING_PENDING_RENAME_THRESHOLD {
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
        model.renames.pending.remove(&stale_id);
        if model.renames.last_window_id.as_deref() == Some(stale_id.as_str()) {
            model.renames.last_window_id = None;
        }
    }
}

fn clear_orphaned_last_pending_rename(model: &mut Model) {
    if let Some(last_id) = model.renames.last_window_id.as_deref() {
        if !model.renames.pending.contains_key(last_id) {
            model.renames.last_window_id = None;
        }
    }
}
