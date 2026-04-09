use std::collections::HashMap;

use tracing::debug;

use crate::cmd::Cmd;
use crate::model::{Mode, Model, PreviewState, SelectionTarget};
use crate::tree::{build_tree, WindowInfo};

use super::naming::restore_folder_expanded;
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
    let was_moving = matches!(model.mode, Mode::Moving { .. });
    if was_moving {
        model.mode = Mode::Normal;
        model.clear_reorder_preview();
    }

    let mut selected_target = derive_selected_target(model);
    windows.sort_by_key(|window| window.index);
    let windows_by_id: HashMap<&str, &WindowInfo> = windows
        .iter()
        .map(|window| (window.id.as_str(), window))
        .collect();

    let selected_exists = match selected_target.as_ref() {
        Some(SelectionTarget::Window(id)) => windows_by_id.contains_key(id.as_str()),
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

    bump_last_pending_if_missing(model, &windows_by_id, &mut selected_target);

    let should_skip_rebuild = !was_moving
        && model
            .window_list_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot == &windows);

    if should_skip_rebuild {
        model.set_window_list_snapshot(windows.clone());
        let (mut followup_cmds, stale_pending_ids) =
            reconcile_pending_renames(model, &windows_by_id);
        clear_stale_pending_renames(model, stale_pending_ids);
        clear_orphaned_last_pending_rename(model);
        clear_mode_if_missing_target(model, &windows);
        followup_cmds.push(Cmd::Render);
        return followup_cmds;
    }

    if !was_moving {
        if let Some(cmds) = try_incremental_root_level_add_remove(model, &windows, &selected_target)
        {
            return cmds;
        }

        if let Some(leaf_names_by_id) = rename_only_leaf_updates(model, &windows) {
            model.rename_window_leaves(&leaf_names_by_id);
            model.reorder.pending_selection = None;
            model.set_window_list_snapshot(windows.clone());
            let (mut followup_cmds, stale_pending_ids) =
                reconcile_pending_renames(model, &windows_by_id);
            clear_stale_pending_renames(model, stale_pending_ids);
            clear_orphaned_last_pending_rename(model);
            clear_mode_if_missing_target(model, &windows);
            followup_cmds.push(Cmd::Render);
            return followup_cmds;
        }
    }

    let expanded_state = model.folder_expanded_snapshot().clone();
    let mut new_tree = build_tree(&windows);
    restore_folder_expanded(&mut new_tree, &expanded_state);
    let selected_ref = selected_target.as_ref();
    model.replace_tree_preserve_selection(new_tree, selected_ref);
    model.reorder.pending_selection = None;
    model.set_window_list_snapshot(windows.clone());

    let (mut followup_cmds, stale_pending_ids) = reconcile_pending_renames(model, &windows_by_id);
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
    windows_by_id: &HashMap<&str, &WindowInfo>,
    selected_target: &mut Option<SelectionTarget>,
) {
    model
        .renames
        .last_window_id
        .clone()
        .into_iter()
        .for_each(|last_id| {
            let last_exists = windows_by_id.contains_key(last_id.as_str());
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

fn reconcile_pending_renames(
    model: &mut Model,
    windows_by_id: &HashMap<&str, &WindowInfo>,
) -> (Vec<Cmd>, Vec<String>) {
    let mut followup_cmds = Vec::new();
    let mut stale_pending_ids = Vec::new();

    for (id, pending) in &mut model.renames.pending {
        if model.renames.last_window_id.as_deref() == Some(id.as_str())
            && !windows_by_id.contains_key(id.as_str())
        {
            if pending.observed_count >= super::MISSING_PENDING_RENAME_THRESHOLD {
                stale_pending_ids.push(id.clone());
            }
            continue;
        }

        if let Some(current) = windows_by_id.get(id.as_str()) {
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

fn rename_only_leaf_updates(
    model: &Model,
    windows: &[WindowInfo],
) -> Option<HashMap<String, String>> {
    let snapshot = model.window_list_snapshot.as_ref()?;
    if snapshot.len() != windows.len() {
        return None;
    }

    let mut leaf_names_by_id = HashMap::new();

    for (old, new) in snapshot.iter().zip(windows) {
        if old.id != new.id || old.index != new.index {
            return None;
        }

        if folder_prefix(old.name.as_str()) != folder_prefix(new.name.as_str()) {
            return None;
        }

        if old.name != new.name {
            leaf_names_by_id.insert(new.id.clone(), leaf_name(new.name.as_str()).to_string());
        }
    }

    if leaf_names_by_id.is_empty() {
        None
    } else {
        Some(leaf_names_by_id)
    }
}

fn folder_prefix(name: &str) -> Option<&str> {
    name.rsplit_once(':').map(|(prefix, _)| prefix)
}

fn leaf_name(name: &str) -> &str {
    name.rsplit_once(':').map(|(_, leaf)| leaf).unwrap_or(name)
}

fn try_incremental_root_level_add_remove(
    model: &mut Model,
    windows: &[WindowInfo],
    selected_target: &Option<SelectionTarget>,
) -> Option<Vec<Cmd>> {
    let snapshot = model.window_list_snapshot.as_ref()?;
    if snapshot.iter().any(|w| w.name.contains(':')) || windows.iter().any(|w| w.name.contains(':'))
    {
        return None;
    }

    let snapshot_ids: Vec<&str> = snapshot.iter().map(|w| w.id.as_str()).collect();
    let window_ids: Vec<&str> = windows.iter().map(|w| w.id.as_str()).collect();

    let removed_ids: Vec<&str> = snapshot_ids
        .iter()
        .copied()
        .filter(|id| !window_ids.contains(id))
        .collect();
    let added_ids: Vec<&str> = window_ids
        .iter()
        .copied()
        .filter(|id| !snapshot_ids.contains(id))
        .collect();

    let single_change = match (added_ids.as_slice(), removed_ids.as_slice()) {
        ([added], []) => Some((Some(*added), None)),
        ([], [removed]) => Some((None, Some(*removed))),
        _ => None,
    }?;

    if !snapshot_ids
        .iter()
        .zip(window_ids.iter())
        .all(|(old, new)| {
            old == new || single_change.0 == Some(*new) || single_change.1 == Some(*old)
        })
    {
        return None;
    }

    let mut tree = model.tree().to_vec();
    match single_change {
        (Some(added_id), None) => {
            let added = windows.iter().find(|w| w.id == added_id)?;
            let insert_at = windows
                .iter()
                .take_while(|w| w.id != added.id)
                .count()
                .min(tree.len());
            tree.insert(
                insert_at,
                crate::tree::TreeNode::Window {
                    info: added.clone(),
                },
            );
        }
        (None, Some(removed_id)) => {
            let remove_at = tree.iter().position(|node| match node {
                crate::tree::TreeNode::Window { info } => info.id == removed_id,
                _ => false,
            })?;
            tree.remove(remove_at);
        }
        _ => return None,
    }

    let expanded_state = model.folder_expanded_snapshot().clone();
    restore_folder_expanded(&mut tree, &expanded_state);
    model.replace_tree_preserve_selection(tree, selected_target.as_ref());
    model.reorder.pending_selection = None;
    model.set_window_list_snapshot(windows.to_vec());

    let windows_by_id: HashMap<&str, &WindowInfo> =
        windows.iter().map(|w| (w.id.as_str(), w)).collect();
    let (mut followup_cmds, stale_pending_ids) = reconcile_pending_renames(model, &windows_by_id);
    clear_stale_pending_renames(model, stale_pending_ids);
    clear_orphaned_last_pending_rename(model);
    clear_mode_if_missing_target(model, windows);
    followup_cmds.push(Cmd::Render);
    Some(followup_cmds)
}
