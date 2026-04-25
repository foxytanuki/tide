use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::io::Stdout;
use std::pin::Pin;

use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tracing::{debug, info, warn};

use crate::cmd::Cmd;
use crate::metrics;
use crate::model::{Model, PendingRename, PreviewState, SelectionTarget};
use crate::msg::Msg;
use crate::tmux::commands;
use crate::tmux::{TmuxControl, WindowInfo};
use crate::update::update;
use crate::view::render;

mod ai;
mod layout;

use self::ai::{check_border, poll_ai_processes, reset_all_borders};
use self::layout::{
    apply_layout_helper, choose_home_pane_in_window, choose_leftmost_pane_in_window,
    cleanup_helper_managed_windows, ensure_sidebar_width, query_window_pane_targets,
    reapply_helper_layout_if_needed, reconcile_sidebar_state, resolve_new_window_cwd,
    restore_window_layout, save_window_layout, save_window_layout_without_pane,
    validate_sidebar_panes,
};

#[cfg(test)]
use std::collections::HashMap;

#[cfg(test)]
use self::ai::{
    classify_active_panes, find_ai_pane_candidates, is_ai_process_name, read_cpu_ticks,
    read_cpu_ticks_tree, AiPaneCandidate, AI_CPU_GRACE_POLLS, MIN_OUTPUT_BURST,
};

#[cfg(test)]
use self::layout::{
    build_cd_send_keys_cmd, build_sidebar_main_3x2_layout, build_split_window_cmd,
    content_pane_ids, layout_without_pane, parse_layout_pane_line, query_layout_root, LayoutKind,
    LayoutNode, PaneGeom,
};

pub type AppTerminal = Terminal<CrosstermBackend<Stdout>>;
const SIDEBAR_WIDTH_CHARS: u16 = 30;
const LAYOUT_CHANGE_SUPPRESSION_MS: u64 = 500;

pub trait TmuxApi {
    fn send_command<'a>(
        &'a mut self,
        cmd: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
    fn list_windows<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<WindowInfo>, String>> + Send + 'a>>;
}

impl TmuxApi for TmuxControl {
    fn send_command<'a>(
        &'a mut self,
        cmd: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            TmuxControl::send_command(self, cmd)
                .await
                .map_err(|err| err.to_string())
        })
    }

    fn list_windows<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<WindowInfo>, String>> + Send + 'a>> {
        Box::pin(async move {
            TmuxControl::list_windows(self)
                .await
                .map_err(|err| err.to_string())
        })
    }
}

pub async fn execute_commands<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    terminal: &mut AppTerminal,
    cmds: Vec<Cmd>,
) -> bool {
    let mut queue: VecDeque<Cmd> = cmds.into();

    while let Some(cmd) = queue.pop_front() {
        match cmd {
            Cmd::PreviewWindow {
                id: target_window_id,
            } => handle_preview_window(model, tmux, &mut queue, target_window_id).await,
            Cmd::RestorePreview => handle_restore_preview(model, tmux, &mut queue).await,
            Cmd::FocusRightPane => {
                handle_focus_right_pane(model, tmux, &mut queue).await;
            }
            Cmd::NewWindow { name } => handle_new_window(model, tmux, &mut queue, name).await,
            Cmd::RenameWindow { id, name } => {
                handle_rename_window(model, tmux, &mut queue, id, name).await;
            }
            Cmd::ReorderWindows { order, selection } => {
                handle_reorder_windows(model, tmux, &mut queue, order, selection).await;
            }
            Cmd::CloseWindow { id } => handle_close_window(model, tmux, &mut queue, id).await,
            Cmd::FollowToWindow {
                window_id: target_window_id,
            } => handle_follow_to_window(model, tmux, &mut queue, target_window_id).await,
            Cmd::EnsureSidebarWidth => ensure_sidebar_width(model, tmux).await,
            Cmd::ValidateSidebarPanes => validate_sidebar_panes(model, tmux, &mut queue).await,
            Cmd::ListWindows => handle_list_windows(model, tmux, &mut queue).await,
            Cmd::PollAiProcesses => poll_ai_processes(model, tmux, &mut queue).await,
            Cmd::ApplyLayoutHelper => apply_layout_helper(model, tmux, &mut queue).await,
            Cmd::CheckBorder => check_border(model, tmux).await,
            Cmd::ResetAllBorders => reset_all_borders(model, tmux).await,
            Cmd::Render => render_model(model, terminal),
            Cmd::Restart => return false,
            Cmd::Quit => return false,
        }
    }

    true
}

fn join_batch_with_window_select(
    sidebar_pane_id: &str,
    join_target: &str,
    window_id: &str,
) -> String {
    format!(
        "join-pane -dfhb -l {} -s {} -t {} ; select-window -t {} ; select-pane -t {}",
        SIDEBAR_WIDTH_CHARS, sidebar_pane_id, join_target, window_id, sidebar_pane_id,
    )
}

fn join_batch_without_window_select(sidebar_pane_id: &str, join_target: &str) -> String {
    format!(
        "join-pane -dfhb -l {} -s {} -t {}",
        SIDEBAR_WIDTH_CHARS, sidebar_pane_id, join_target,
    )
}

async fn restore_or_seed_leaving_layout<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    leaving_window: &str,
) {
    let had_saved_layout = model.sidebar.pane_layouts.contains_key(leaving_window);
    restore_window_layout(model, tmux, leaving_window).await;
    if !had_saved_layout {
        // No exact snapshot yet (typically startup window).
        // Persist current post-move layout as baseline for next switch.
        save_window_layout(model, tmux, leaving_window).await;
    }
}

async fn snapshot_leaving_window_layout<T: TmuxApi>(model: &mut Model, tmux: &mut T) -> String {
    let leaving_window = model.sidebar.window_id.clone();
    let sidebar_pane_id = model.sidebar.pane_id.clone();
    save_window_layout_without_pane(model, tmux, &leaving_window, &sidebar_pane_id).await;
    leaving_window
}

async fn handle_preview_window<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    queue: &mut VecDeque<Cmd>,
    target_window_id: String,
) {
    // If previewing and target is the original window, restore.
    if let PreviewState::Previewing {
        ref original_window_id,
        ..
    } = model.sidebar.preview
    {
        if target_window_id == *original_window_id {
            debug!(target = %target_window_id, "preview target is original, restoring");
            queue.push_front(Cmd::Render);
            queue.push_front(Cmd::RestorePreview);
            return;
        }
    }

    // Already on this window, no-op.
    if target_window_id == model.sidebar.window_id {
        return;
    }

    debug!(target = %target_window_id, "previewing window");

    // Record original state if starting fresh preview.
    let (orig_window, orig_home) = match &model.sidebar.preview {
        PreviewState::Home => (
            model.sidebar.window_id.clone(),
            model.sidebar.home_pane_id.clone(),
        ),
        PreviewState::Previewing {
            original_window_id,
            original_home_pane_id,
        } => (original_window_id.clone(), original_home_pane_id.clone()),
    };

    // Query phase: find join target.
    // Always target the leftmost pane so sidebar stays at far left.
    let target_panes =
        query_window_pane_targets(tmux, &target_window_id, &model.sidebar.pane_id).await;
    let join_target = if target_panes.leftmost.is_empty() {
        model.error_message = Some("preview: could not resolve leftmost pane".to_string());
        queue.push_front(Cmd::Render);
        return;
    } else {
        target_panes.leftmost
    };

    // Save target window's layout before sidebar joins (for future restoration).
    save_window_layout(model, tmux, &target_window_id).await;
    let leaving_window = snapshot_leaving_window_layout(model, tmux).await;

    // Action phase: batch all visual tmux operations into a single
    // command so tmux processes them in one server tick (no flicker).
    // -l {SIDEBAR_WIDTH_CHARS} sets sidebar width at join time to avoid intermediate resize.
    let batch =
        join_batch_with_window_select(&model.sidebar.pane_id, &join_target, &target_window_id);

    // Suppress the next expected focus notification from this internal move.
    model.sidebar.ignore_window_changes = 1;
    model.sidebar.pending_internal_focus_window = Some(target_window_id.clone());

    if let Err(err) = send_batch_with_reconcile(model, tmux, &batch).await {
        warn!(%err, "preview batch failed");
        clear_internal_window_change_suppression(model);
        model.error_message = Some(format!("preview: {err}"));
        queue.push_front(Cmd::Render);
        return;
    }

    // Restore leaving window's pane layout when we have an exact
    // "without sidebar" snapshot.
    restore_or_seed_leaving_layout(model, tmux, &leaving_window).await;
    reapply_helper_layout_if_needed(model, tmux, &target_window_id).await;

    // Resolve home pane if needed (query after move).
    let new_home = if !target_panes.home.is_empty() {
        target_panes.home
    } else {
        choose_home_pane_in_window(tmux, &target_window_id, &model.sidebar.pane_id).await
    };

    if !new_home.is_empty() {
        model.sidebar.home_pane_id = new_home;
    }

    debug!(
        sidebar_window = %target_window_id,
        home_pane = %model.sidebar.home_pane_id,
        orig_window = %orig_window,
        "preview active"
    );

    model.sidebar.window_id = target_window_id;
    model.sidebar.pending_preview_id = None;
    model.sidebar.preview = PreviewState::Previewing {
        original_window_id: orig_window,
        original_home_pane_id: orig_home,
    };
    // Suppress %output for next 2 polls to discard pane redraw noise.
    model.ai.output_suppress = 2;
    queue.push_front(Cmd::CheckBorder);
}

async fn handle_restore_preview<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    queue: &mut VecDeque<Cmd>,
) {
    if let PreviewState::Previewing {
        ref original_window_id,
        ref original_home_pane_id,
    } = model.sidebar.preview
    {
        let orig_window = original_window_id.clone();
        let orig_home = original_home_pane_id.clone();

        debug!(orig_window = %orig_window, "restoring preview");

        // Resolve both the join target and fresh home pane in one pane list query.
        let orig_panes =
            query_window_pane_targets(tmux, &orig_window, &model.sidebar.pane_id).await;

        // Save orig_window's layout before sidebar re-joins it.
        save_window_layout(model, tmux, &orig_window).await;
        let leaving_window = snapshot_leaving_window_layout(model, tmux).await;

        // Batch: join sidebar back + switch + focus.
        if orig_panes.leftmost.is_empty() {
            model.error_message =
                Some("restore preview: could not resolve leftmost pane".to_string());
            queue.push_front(Cmd::Render);
            return;
        }
        let batch = join_batch_with_window_select(
            &model.sidebar.pane_id,
            &orig_panes.leftmost,
            &orig_window,
        );

        // Suppress the next expected focus notification from this internal move.
        model.sidebar.ignore_window_changes = 1;
        model.sidebar.pending_internal_focus_window = Some(orig_window.clone());

        let restored = if let Err(err) = send_batch_with_reconcile(model, tmux, &batch).await {
            warn!(%err, "restore batch failed, trying fallback");
            // Fallback: retry join to leftmost pane (re-query, because pane IDs may have changed).
            let fallback_leftmost =
                choose_leftmost_pane_in_window(tmux, &orig_window, &model.sidebar.pane_id).await;
            if fallback_leftmost.is_empty() {
                warn!("restore fallback: no leftmost pane available");
                false
            } else {
                model.sidebar.ignore_window_changes = 1;
                model.sidebar.pending_internal_focus_window = Some(orig_window.clone());
                let fallback = join_batch_with_window_select(
                    &model.sidebar.pane_id,
                    &fallback_leftmost,
                    &orig_window,
                );
                if let Err(err) = send_batch_with_reconcile(model, tmux, &fallback).await {
                    warn!(%err, "restore fallback batch also failed");
                    false
                } else {
                    true
                }
            }
        } else {
            true
        };

        // Restore leaving window's pane layout after batch.
        if restored {
            restore_or_seed_leaving_layout(model, tmux, &leaving_window).await;
            reapply_helper_layout_if_needed(model, tmux, &orig_window).await;
        }

        if restored {
            model.sidebar.window_id = orig_window.clone();
            // Reuse the same pane-list result; fallback to the original saved
            // home only if the active pane disappeared.
            model.sidebar.home_pane_id = if orig_panes.home.is_empty() {
                orig_home
            } else {
                orig_panes.home
            };
            model.sidebar.preview = PreviewState::Home;
        } else {
            // Keep reconciled runtime state from failed batch attempts.
            clear_internal_window_change_suppression(model);
            if model.sidebar.window_id == orig_window {
                model.sidebar.preview = PreviewState::Home;
            }
            model.error_message = Some("restore preview failed; state reconciled".to_string());
        }
        // Discard %output events from pane redraw during window switch.
        model.ai.output_counts.clear();
        queue.push_front(Cmd::CheckBorder);
    }
}

async fn handle_focus_right_pane<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    queue: &mut VecDeque<Cmd>,
) {
    debug!("focusing right pane");
    if let Err(err) = tmux.send_command(commands::select_right_pane()).await {
        warn!(%err, "select-pane -R failed");
        model.error_message = Some(format!("select-pane: {err}"));
        queue.push_front(Cmd::Render);
    }
}

async fn handle_new_window<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    queue: &mut VecDeque<Cmd>,
    name: String,
) {
    debug!(name, "creating new window");
    let cwd = resolve_new_window_cwd(model, tmux).await;
    let new_cmd = commands::new_window(&model.session_name, &name, cwd.as_deref());
    match tmux.send_command(&new_cmd).await {
        Ok(output) => {
            let window_id = output.trim();
            if !window_id.is_empty() {
                let disable_rename = commands::disable_window_rename(window_id);
                if let Err(err) = tmux.send_command(&disable_rename).await {
                    warn!(
                        id = %window_id,
                        name = %name,
                        %err,
                        "failed to disable rename updates for new window"
                    );
                } else {
                    debug!(id = %window_id, "disabled automatic/allow-rename for new window");
                }

                model.renames.pending.insert(
                    window_id.to_string(),
                    PendingRename {
                        target_name: name.clone(),
                        observed_count: 0,
                    },
                );
                model.renames.last_window_id = Some(window_id.to_string());
                debug!(
                    id = %window_id,
                    name = %name,
                    "tracking new window for rename stabilization"
                );
                queue.push_front(Cmd::Render);
                queue.push_front(Cmd::PreviewWindow {
                    id: window_id.to_string(),
                });
                queue.push_front(Cmd::ListWindows);
            } else {
                warn!("new-window returned empty window id");
            }
        }
        Err(err) => {
            reconcile_after_command_failure(model, tmux, queue, "new-window", &err, true).await;
        }
    }
}

async fn handle_rename_window<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    queue: &mut VecDeque<Cmd>,
    id: String,
    name: String,
) {
    debug!(id, name, "renaming window");
    let disable_rename = commands::disable_window_rename(&id);
    if let Err(err) = tmux.send_command(&disable_rename).await {
        warn!(%id, %err, "failed to disable automatic-rename before rename");
    } else {
        debug!(%id, "disabled rename options before rename");
    }
    let cmd_str = commands::rename_window(&id, &name);
    if let Err(err) = tmux.send_command(&cmd_str).await {
        reconcile_after_command_failure(model, tmux, queue, "rename-window", &err, true).await;
    } else {
        if let Err(err) = tmux.send_command(&disable_rename).await {
            warn!(
                %id,
                %err,
                "failed to keep rename updates disabled after rename"
            );
        } else {
            debug!(%id, "kept rename options disabled after rename");
        }
        queue.push_front(Cmd::ListWindows);
    }
}

async fn handle_reorder_windows<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    queue: &mut VecDeque<Cmd>,
    mut desired_order: Vec<String>,
    selection: SelectionTarget,
) {
    debug!(count = desired_order.len(), target = ?selection, "reordering windows");
    desired_order.retain(|window_id| window_id != &model.sidebar.window_id);

    let mut current_windows = match tmux.list_windows().await {
        Ok(windows) => windows,
        Err(err) => {
            warn!(%err, "reorder: list-windows failed");
            model.reorder.pending_selection = None;
            model.error_message = Some(format!("reorder: list-windows: {err}"));
            queue.push_front(Cmd::Render);
            return;
        }
    };
    current_windows.retain(|window| window.id != model.sidebar.window_id);
    current_windows.sort_by_key(|window| window.index);
    let current_ids: Vec<String> = current_windows
        .iter()
        .map(|window| window.id.clone())
        .collect();

    let mut desired_iter = desired_order.iter();
    let desired_set: HashSet<&str> = desired_order.iter().map(String::as_str).collect();
    let mut final_order = Vec::with_capacity(current_ids.len());
    for current_id in &current_ids {
        if desired_set.contains(current_id.as_str()) {
            if let Some(next_id) = desired_iter.next() {
                final_order.push(next_id.clone());
            }
        } else {
            final_order.push(current_id.clone());
        }
    }

    if final_order.len() != current_ids.len() || desired_iter.next().is_some() {
        warn!(
            current = current_ids.len(),
            desired = desired_order.len(),
            "reorder: inconsistent window set"
        );
        model.reorder.pending_selection = None;
        model.error_message = Some("reorder: window set changed".to_string());
        queue.push_front(Cmd::Render);
        return;
    }

    let mut working = current_ids.clone();
    for idx in 0..final_order.len() {
        if working[idx] == final_order[idx] {
            continue;
        }

        let Some(found_idx) = working.iter().position(|id| id == &final_order[idx]) else {
            warn!(wanted = %final_order[idx], "reorder: target window missing during swap");
            model.reorder.pending_selection = None;
            model.error_message = Some("reorder: target window missing".to_string());
            queue.push_front(Cmd::Render);
            return;
        };

        let cmd_str = commands::swap_window(&working[found_idx], &working[idx]);
        if let Err(err) = tmux.send_command(&cmd_str).await {
            model.reorder.pending_selection = None;
            reconcile_after_command_failure(model, tmux, queue, "swap-window", &err, true).await;
            return;
        }
        working.swap(idx, found_idx);
    }

    queue.push_front(Cmd::ListWindows);
}

async fn handle_close_window<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    queue: &mut VecDeque<Cmd>,
    id: String,
) {
    // Never kill the window hosting the sidebar.
    if model.sidebar.window_id == id {
        match &model.sidebar.preview {
            PreviewState::Previewing { .. } => {
                if model.sidebar.close_restore_attempted {
                    // Circuit breaker: already tried restore once, don't loop.
                    model.sidebar.close_restore_attempted = false;
                    warn!(
                        id,
                        "close-after-restore failed, refusing to close sidebar window"
                    );
                    model.error_message = Some("cannot close: sidebar stuck in window".to_string());
                    queue.push_front(Cmd::Render);
                    return;
                }
                model.sidebar.close_restore_attempted = true;
                debug!(id, "closing previewed window, restoring first");
                queue.push_front(Cmd::CloseWindow { id });
                queue.push_front(Cmd::RestorePreview);
                return;
            }
            PreviewState::Home => {
                if model.sidebar.close_restore_attempted {
                    model.sidebar.close_restore_attempted = false;
                    warn!(
                        id,
                        "close-after-evacuate failed, refusing to close sidebar window"
                    );
                    model.error_message = Some("cannot close: sidebar stuck in window".to_string());
                    queue.push_front(Cmd::Render);
                    return;
                }
                // Find another window to evacuate sidebar to.
                if let Some(other_id) = model.find_another_window_id(&id) {
                    model.sidebar.close_restore_attempted = true;
                    debug!(id, other = %other_id, "evacuating sidebar before close");
                    queue.push_front(Cmd::CloseWindow { id });
                    queue.push_front(Cmd::FollowToWindow {
                        window_id: other_id,
                    });
                    return;
                } else {
                    warn!(id, "no other window to evacuate sidebar to");
                    model.error_message = Some("cannot close last window".to_string());
                    queue.push_front(Cmd::Render);
                    return;
                }
            }
        }
    }

    model.sidebar.close_restore_attempted = false;
    debug!(id, "closing window");
    let cmd_str = commands::kill_window(&id);
    if let Err(err) = tmux.send_command(&cmd_str).await {
        reconcile_after_command_failure(model, tmux, queue, "kill-window", &err, true).await;
    } else {
        model.sidebar.pane_layouts.remove(&id);
        model.sidebar.helper_managed_windows.remove(&id);
        // Proactively refresh — don't rely solely on %window-close notification.
        queue.push_front(Cmd::ListWindows);
    }
}

async fn handle_follow_to_window<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    queue: &mut VecDeque<Cmd>,
    target_window_id: String,
) {
    if target_window_id == model.sidebar.window_id {
        return;
    }

    info!(target = %target_window_id, "following to window");

    // Query phase.
    // Always target the leftmost pane so sidebar stays at far left.
    let target_panes =
        query_window_pane_targets(tmux, &target_window_id, &model.sidebar.pane_id).await;
    let join_target = if target_panes.leftmost.is_empty() {
        model.error_message = Some("follow: could not resolve leftmost pane".to_string());
        queue.push_front(Cmd::Render);
        return;
    } else {
        target_panes.leftmost
    };

    // Save target window's layout before sidebar joins.
    save_window_layout(model, tmux, &target_window_id).await;
    let leaving_window = snapshot_leaving_window_layout(model, tmux).await;

    // Action phase: batch join.
    let batch = join_batch_without_window_select(&model.sidebar.pane_id, &join_target);

    // Suppress the next expected focus notification from this internal move.
    model.sidebar.ignore_window_changes = 1;
    model.sidebar.pending_internal_focus_window = Some(target_window_id.clone());

    if let Err(err) = send_batch_with_reconcile(model, tmux, &batch).await {
        warn!(%err, "follow batch failed");
        clear_internal_window_change_suppression(model);
        model.error_message = Some(format!("follow: {err}"));
        queue.push_front(Cmd::Render);
        return;
    }

    // Restore leaving window's pane layout.
    restore_or_seed_leaving_layout(model, tmux, &leaving_window).await;
    reapply_helper_layout_if_needed(model, tmux, &target_window_id).await;

    // Resolve home pane.
    let new_home = if !target_panes.home.is_empty() {
        target_panes.home
    } else {
        choose_home_pane_in_window(tmux, &target_window_id, &model.sidebar.pane_id).await
    };

    if !new_home.is_empty() {
        model.sidebar.home_pane_id = new_home;
    }

    debug!(
        sidebar_window = %target_window_id,
        home_pane = %model.sidebar.home_pane_id,
        "follow complete"
    );
    model.sidebar.window_id = target_window_id;
    // Suppress %output for next 2 polls to discard pane redraw noise.
    model.ai.output_suppress = 2;
    queue.push_front(Cmd::CheckBorder);
}

async fn handle_list_windows<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    queue: &mut VecDeque<Cmd>,
) {
    match tmux.list_windows().await {
        Ok(windows) => {
            debug!(count = windows.len(), "refreshed window list");
            for window in &windows {
                if window.id == model.sidebar.window_id {
                    continue;
                }
                if !model.sidebar.pane_layouts.contains_key(&window.id) {
                    save_window_layout(model, tmux, &window.id).await;
                }
            }
            model
                .sidebar
                .pane_layouts
                .retain(|id, _| windows.iter().any(|w| w.id == *id));
            cleanup_helper_managed_windows(model, &windows);
            enqueue_follow_up(queue, update(model, Msg::WindowListLoaded(windows)));
        }
        Err(err) => {
            warn!(%err, "list-windows failed");
            model.error_message = Some(format!("list-windows: {err}"));
            queue.push_front(Cmd::Render);
        }
    }
}

fn enqueue_follow_up(queue: &mut VecDeque<Cmd>, follow_up: Vec<Cmd>) {
    for cmd in follow_up.into_iter().rev() {
        queue.push_front(cmd);
    }
}

fn render_model(model: &mut Model, terminal: &mut AppTerminal) {
    let started_at = std::time::Instant::now();
    if let Err(err) = terminal.draw(|f| render(model, f)) {
        model.error_message = Some(format!("render: {err}"));
    }
    tracing::trace!(
        flat_items = model.flat_items().len(),
        elapsed_us = started_at.elapsed().as_micros() as u64,
        "render completed"
    );
}

fn clear_internal_window_change_suppression(model: &mut Model) {
    model.sidebar.ignore_window_changes = 0;
    model.sidebar.pending_internal_focus_window = None;
}

async fn reconcile_after_command_failure<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    queue: &mut VecDeque<Cmd>,
    command_name: &str,
    err: &str,
    refresh_windows: bool,
) {
    warn!(%err, command = command_name, "tmux command failed; reconciling state");
    model.error_message = Some(format!("{command_name}: {err}"));
    reconcile_sidebar_state(model, tmux).await;

    if refresh_windows {
        handle_list_windows(model, tmux, queue).await;
    } else {
        queue.push_front(Cmd::Render);
    }
}

async fn send_batch_with_reconcile<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    batch: &str,
) -> Result<(), String> {
    if let Err(err) = tmux.send_command(batch).await {
        let reconciles = metrics::record_batch_reconcile();
        let old_window = model.sidebar.window_id.clone();
        let old_home = model.sidebar.home_pane_id.clone();
        warn!(
            %err,
            reconciles,
            old_window = %old_window,
            old_home = %old_home,
            batch = %batch,
            "reconcile: batch failed; syncing state"
        );
        // A semicolon-joined tmux command can fail after partially applying earlier
        // subcommands. Re-sync model with tmux before returning the error.
        reconcile_sidebar_state(model, tmux).await;
        warn!(
            new_window = %model.sidebar.window_id,
            new_home = %model.sidebar.home_pane_id,
            "reconcile: state synced after batch failure"
        );
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    struct FakeTmux {
        responses: VecDeque<Result<String, String>>,
        window_lists: VecDeque<Result<Vec<WindowInfo>, String>>,
        commands: Vec<String>,
    }

    impl FakeTmux {
        fn new(responses: Vec<Result<String, String>>) -> Self {
            Self {
                responses: responses.into(),
                window_lists: VecDeque::new(),
                commands: Vec::new(),
            }
        }

        fn with_window_lists(
            responses: Vec<Result<String, String>>,
            window_lists: Vec<Result<Vec<WindowInfo>, String>>,
        ) -> Self {
            Self {
                responses: responses.into(),
                window_lists: window_lists.into(),
                commands: Vec::new(),
            }
        }
    }

    fn test_model() -> Model {
        Model::new(
            "s".to_string(),
            "$1".to_string(),
            "%sidebar".to_string(),
            "%home_old".to_string(),
            "@old".to_string(),
        )
    }

    fn wi(id: &str, index: usize, name: &str) -> WindowInfo {
        WindowInfo {
            id: id.to_string(),
            index,
            name: name.to_string(),
            active: false,
        }
    }

    impl TmuxApi for FakeTmux {
        fn send_command<'a>(
            &'a mut self,
            cmd: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async move {
                self.commands.push(cmd.to_string());
                self.responses
                    .pop_front()
                    .expect("missing fake response for command")
            })
        }

        fn list_windows<'a>(
            &'a mut self,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<WindowInfo>, String>> + Send + 'a>> {
            Box::pin(async move {
                self.window_lists
                    .pop_front()
                    .unwrap_or_else(|| Err("not used in this test".to_string()))
            })
        }
    }

    #[test]
    fn find_ai_pane_candidates_filters_correctly() {
        let output = "@1\t%10\tclaude\t1000\n@1\t%11\tzsh\t1001\n@2\t%20\tgemini\t1002\n@2\t%21\tcodex\t1003\n@3\t%30\topencode\t1004\n";
        let candidates = find_ai_pane_candidates(output, "%sidebar");
        assert_eq!(candidates.len(), 4);
        assert_eq!(candidates[0].pane_id, "%10");
        assert_eq!(candidates[0].window_id, "@1");
        assert_eq!(candidates[1].pane_id, "%20");
        assert_eq!(candidates[1].window_id, "@2");
        assert_eq!(candidates[2].pane_id, "%21");
        assert_eq!(candidates[3].pane_id, "%30");
    }

    #[test]
    fn is_ai_process_name_matches_opencode_and_existing_tools() {
        assert!(is_ai_process_name("opencode"));
        assert!(is_ai_process_name("OpenCode"));
        assert!(is_ai_process_name("claude-code"));
        assert!(!is_ai_process_name("bash"));
    }

    #[test]
    fn find_ai_pane_candidates_skips_sidebar() {
        let output = "@1\t%sidebar\tclaude\t1000\n@1\t%10\tclaude\t1001\n";
        let candidates = find_ai_pane_candidates(output, "%sidebar");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].pane_id, "%10");
    }

    #[test]
    fn layout_without_pane_restores_full_width_after_sidebar_removal() {
        let layout = "1086,315x81,0,0{157x81,0,0,27,78x81,158,0,25,78x81,237,0,26}";
        let trimmed = layout_without_pane(layout, "%27").expect("layout should trim sidebar");
        assert_eq!(
            trimmed.split_once(',').unwrap().1,
            "315x81,0,0{157x81,0,0,25,157x81,158,0,26}"
        );
    }

    #[test]
    fn layout_without_pane_preserves_vertical_stack_inside_main_area() {
        let layout = "9999,200x80,0,0{30x80,0,0,10,169x80,31,0[169x39,31,0,11,169x40,31,40,12]}";
        let trimmed = layout_without_pane(layout, "%10").expect("layout should trim sidebar");
        assert_eq!(
            trimmed.split_once(',').unwrap().1,
            "200x80,0,0[200x39,0,0,11,200x40,0,40,12]"
        );
    }

    #[test]
    fn parse_layout_pane_line_parses_geometry() {
        let (pane_id, geom) = parse_layout_pane_line("%12\t120\t4").expect("valid line");
        assert_eq!(pane_id, "%12");
        assert_eq!(geom, PaneGeom { left: 120, top: 4 });
    }

    #[test]
    fn content_pane_ids_excludes_sidebar_and_sorts_by_geometry() {
        let panes = vec![
            ("%sidebar".to_string(), PaneGeom { left: 0, top: 0 }),
            ("%2".to_string(), PaneGeom { left: 10, top: 20 }),
            ("%1".to_string(), PaneGeom { left: 10, top: 0 }),
            ("%3".to_string(), PaneGeom { left: 5, top: 0 }),
        ];
        assert_eq!(
            content_pane_ids(&panes, "%sidebar"),
            vec!["%3".to_string(), "%1".to_string(), "%2".to_string()]
        );
    }

    #[test]
    fn build_split_window_cmd_carries_current_path() {
        assert_eq!(
            build_split_window_cmd("%2", Some("/tmp/my dir")),
            "split-window -d -t %2 -h -c \"/tmp/my dir\""
        );
        assert_eq!(
            build_split_window_cmd("%2", None),
            "split-window -d -t %2 -h"
        );
    }

    #[test]
    fn build_cd_send_keys_cmd_shell_quotes_path() {
        let cmd = build_cd_send_keys_cmd("%2", "/tmp/it's here");
        assert!(cmd.starts_with("send-keys -t %2 \"cd -- "));
        assert!(cmd.contains("/tmp/it"));
        assert!(cmd.contains("s here'"));
        assert!(cmd.ends_with("\" C-m"));
    }

    #[tokio::test]
    async fn resolve_new_window_cwd_prefers_home_pane() {
        let model = test_model();
        let mut tmux = FakeTmux::new(vec![Ok("/work/project\n".to_string())]);

        let cwd = resolve_new_window_cwd(&model, &mut tmux).await;

        assert_eq!(cwd.as_deref(), Some("/work/project"));
        assert_eq!(
            tmux.commands,
            vec!["display-message -p -t %home_old '#{pane_current_path}'".to_string()]
        );
    }

    #[tokio::test]
    async fn resolve_new_window_cwd_falls_back_when_home_pane_is_stale() {
        let model = test_model();
        let mut tmux = FakeTmux::new(vec![
            Err("stale pane".to_string()),
            Ok("%sidebar\t0\n%fallback\t1\n".to_string()),
            Ok("/work/fallback\n".to_string()),
        ]);

        let cwd = resolve_new_window_cwd(&model, &mut tmux).await;

        assert_eq!(cwd.as_deref(), Some("/work/fallback"));
        assert_eq!(
            tmux.commands,
            vec![
                "display-message -p -t %home_old '#{pane_current_path}'".to_string(),
                "list-panes -t @old -F '#{pane_id}\t#{pane_active}'".to_string(),
                "display-message -p -t %fallback '#{pane_current_path}'".to_string(),
            ]
        );
    }

    #[test]
    fn build_sidebar_main_3x2_layout_places_right_column_last_two_panes() {
        let layout = build_sidebar_main_3x2_layout(120, 40, 1, &[2, 3, 4, 5, 6, 7])
            .expect("layout should build");
        let root = query_layout_root(&layout).expect("layout should parse");
        let LayoutNode::Split { kind, children, .. } = root else {
            panic!("expected root split");
        };
        assert_eq!(kind, LayoutKind::LeftRight);
        assert_eq!(children.len(), 2);
        match &children[1] {
            LayoutNode::Split { children, .. } => match &children[2] {
                LayoutNode::Split { children, .. } => {
                    assert!(matches!(children[0], LayoutNode::Pane { pane_id: 6, .. }));
                    assert!(matches!(children[1], LayoutNode::Pane { pane_id: 7, .. }));
                }
                _ => panic!("expected right column split"),
            },
            _ => panic!("expected main split"),
        }
    }

    #[test]
    fn classify_active_panes_first_seen_is_idle() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        let mut prev = HashMap::new();
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut HashMap::new());
        assert!(
            !panes.contains("%10"),
            "first-seen pane should be idle (baseline)"
        );
        assert!(prev.contains_key("%10"), "baseline should be recorded");
    }

    #[test]
    fn classify_active_panes_high_cpu_alone_does_not_activate() {
        // Key test: CPU alone should NEVER activate a pane.
        // This prevents warm idle sessions from showing as active.
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // polls=0 (never active), even with high CPU — should stay idle
        let mut prev = HashMap::new();
        prev.insert("%10".to_string(), (99999u32, 50000u64, 0u16, 0u8));

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut HashMap::new());
        assert!(
            !panes.contains("%10"),
            "CPU alone should never activate (output-primary)"
        );
    }

    #[test]
    fn classify_active_panes_single_burst_does_not_activate_first_seen() {
        // Single burst should NOT activate — needs consecutive bursts to
        // filter out typing noise in TUI apps like Claude Code.
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        let mut prev = HashMap::new();
        let mut counts = HashMap::new();
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(
            !panes.contains("%10"),
            "single burst should not activate first-seen pane"
        );
        assert!(counts.is_empty(), "counts should be reset after poll");
        let &(_, _, _, bursts) = prev.get("%10").unwrap();
        assert_eq!(bursts, 1, "should have recorded 1 consecutive burst");
    }

    #[test]
    fn classify_active_panes_consecutive_bursts_activate() {
        // Consecutive bursts (sustained streaming) should activate.
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        let mut prev = HashMap::new();
        let mut counts = HashMap::new();

        // First burst — not yet active
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(!panes.contains("%10"), "first burst should not activate");

        // Second burst — now activate
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(
            panes.contains("%10"),
            "consecutive bursts should activate pane"
        );
        let &(_, _, polls, _) = prev.get("%10").unwrap();
        assert_eq!(polls, 1, "polls should be 1 after activation");
    }

    #[test]
    fn classify_active_panes_single_burst_does_not_activate_existing() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // Existing pane with baseline (polls=0, never active)
        let mut prev = HashMap::new();
        prev.insert("%10".to_string(), (99999u32, 50000u64, 0u16, 0u8));

        let mut counts = HashMap::new();
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(
            !panes.contains("%10"),
            "single burst should not activate idle pane"
        );
        let &(_, _, polls, bursts) = prev.get("%10").unwrap();
        assert_eq!(polls, 0, "should stay idle");
        assert_eq!(bursts, 1, "should record 1 consecutive burst");
    }

    #[test]
    fn classify_active_panes_burst_gap_resets_counter() {
        // Burst → no burst → burst should NOT activate (counter resets)
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        let mut prev = HashMap::new();
        let mut counts = HashMap::new();

        // First burst
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(!panes.contains("%10"));

        // Gap (no burst)
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(!panes.contains("%10"));
        let &(_, _, _, bursts) = prev.get("%10").unwrap();
        assert_eq!(bursts, 0, "burst counter should reset on gap");

        // Another burst — starts counting from 1 again
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(!panes.contains("%10"), "should not activate after gap");
    }

    #[test]
    fn classify_active_panes_output_burst_resets_grace() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // Was active (polls >= 1), grace is about to expire
        let mut prev = HashMap::new();
        prev.insert(
            "%10".to_string(),
            (99999u32, 50000u64, AI_CPU_GRACE_POLLS, 0u8),
        );

        // Output burst on already-active pane should reset grace (no consecutive requirement)
        let mut counts = HashMap::new();
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST + 10);

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(
            panes.contains("%10"),
            "output burst should reset grace timer"
        );
        let &(_, _, polls, _) = prev.get("%10").unwrap();
        assert_eq!(polls, 1, "polls should reset to 1");
    }

    #[test]
    fn classify_active_panes_grace_with_cpu_stays_active() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // polls=1 means just activated by streaming, now no output but CPU still working
        // Since fake PID won't have /proc entry, cpu_delta=0, so grace counts down
        let mut prev = HashMap::new();
        prev.insert("%10".to_string(), (99999u32, 50000u64, 1u16, 0u8));

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut HashMap::new());
        // With fake PID, cpu_delta=0, so grace counts down (polls 1→2)
        // But still within grace period
        assert!(
            panes.contains("%10"),
            "should stay active within grace period"
        );

        let &(_, _, polls, _) = prev.get("%10").unwrap();
        assert_eq!(
            polls, 2,
            "polls counter should increment (no CPU activity from fake pid)"
        );
    }

    #[test]
    fn classify_active_panes_grace_expires_to_idle() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // polls at grace limit — next poll should demote to idle
        let mut prev = HashMap::new();
        prev.insert(
            "%10".to_string(),
            (99999u32, 50000u64, AI_CPU_GRACE_POLLS, 0u8),
        );

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut HashMap::new());
        assert!(
            !panes.contains("%10"),
            "should be idle after grace period expires"
        );
    }

    #[test]
    fn classify_active_panes_stays_active_during_thinking() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // polls=2 means was active, been quiet for 2 polls — still within 3s grace
        let mut prev = HashMap::new();
        prev.insert("%10".to_string(), (99999u32, 50000u64, 2u16, 0u8));

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut HashMap::new());
        assert!(
            panes.contains("%10"),
            "should stay active during thinking (within grace)"
        );
    }

    #[test]
    fn classify_active_panes_cleans_stale_entries() {
        let candidates = vec![]; // no AI panes
        let mut prev = HashMap::new();
        prev.insert("%gone".to_string(), (123u32, 50000u64, 0u16, 0u8));

        let _ = classify_active_panes(&candidates, &mut prev, &mut HashMap::new());
        assert!(prev.is_empty(), "stale entries should be cleaned");
    }

    #[test]
    fn classify_active_panes_low_output_not_streaming() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        let mut prev = HashMap::new();
        // Low output count (idle terminal noise) should NOT activate
        let mut counts = HashMap::new();
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST - 1);

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(
            !panes.contains("%10"),
            "low output count should not activate"
        );
    }

    #[test]
    fn classify_active_panes_grace_expired_needs_consecutive_to_reactivate() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // Grace fully expired (polls > GRACE)
        let mut prev = HashMap::new();
        prev.insert(
            "%10".to_string(),
            (99999u32, 50000u64, AI_CPU_GRACE_POLLS + 1, 0u8),
        );

        // No streaming — should demote to idle
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut HashMap::new());
        assert!(
            !panes.contains("%10"),
            "grace-expired pane should demote to idle"
        );

        // Single burst — starts counting but doesn't activate yet
        let mut counts = HashMap::new();
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(
            !panes.contains("%10"),
            "single burst after grace expiry should not reactivate"
        );

        // Second consecutive burst — now reactivate
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(
            panes.contains("%10"),
            "consecutive bursts should reactivate grace-expired pane"
        );
    }

    #[test]
    fn classify_active_panes_any_output_sustains_grace() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // polls=2 means active, been quiet for 2 polls — within grace
        let mut prev = HashMap::new();
        prev.insert("%10".to_string(), (99999u32, 50000u64, 2u16, 0u8));

        // Even 1 output event (spinner) should sustain grace
        let mut counts = HashMap::new();
        counts.insert("%10".to_string(), 1u32);

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(
            panes.contains("%10"),
            "any output should sustain grace period"
        );

        let &(_, _, polls, _) = prev.get("%10").unwrap();
        assert_eq!(
            polls, 1,
            "polls should reset to 1 when output sustains grace"
        );
    }

    #[test]
    fn classify_active_panes_no_output_no_cpu_grace_expires() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // Start at grace limit — no output, no CPU → should expire
        let mut prev = HashMap::new();
        prev.insert(
            "%10".to_string(),
            (99999u32, 50000u64, AI_CPU_GRACE_POLLS, 0u8),
        );

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut HashMap::new());
        assert!(
            !panes.contains("%10"),
            "no output + no CPU should let grace expire"
        );
    }

    #[tokio::test]
    async fn poll_ai_processes_backs_off_after_idle_polls() {
        let mut model = test_model();
        model.ai.poll_skip_ticks = 2;
        model.ai.idle_polls = 6;
        let mut tmux = FakeTmux::new(vec![]);
        let mut queue = VecDeque::new();

        poll_ai_processes(&mut model, &mut tmux, &mut queue).await;

        assert!(tmux.commands.is_empty(), "poll should be skipped");
        assert_eq!(model.ai.poll_skip_ticks, 1);
        assert_eq!(model.ai.idle_polls, 6);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn poll_ai_processes_resets_backoff_when_candidates_appear() {
        let mut model = test_model();
        model.ai.idle_polls = 9;
        let output = "@1\t%10\tclaude\t1000\n";
        let mut tmux = FakeTmux::new(vec![Ok(output.to_string())]);
        let mut queue = VecDeque::new();

        poll_ai_processes(&mut model, &mut tmux, &mut queue).await;

        assert_eq!(model.ai.poll_skip_ticks, 0);
        assert_eq!(model.ai.idle_polls, 0);
        assert_eq!(tmux.commands.len(), 1);
    }

    #[test]
    fn read_cpu_ticks_parses_stat() {
        // Test with a synthetic /proc/pid/stat line
        // We can't easily unit-test read_cpu_ticks since it reads /proc,
        // but we can verify it works on our own process
        let pid = std::process::id();
        let result = read_cpu_ticks(pid);
        assert!(
            result.is_some(),
            "should be able to read own process CPU ticks"
        );
    }

    #[test]
    fn read_cpu_ticks_tree_includes_own_process() {
        let pid = std::process::id();
        let single = read_cpu_ticks(pid).unwrap();
        let tree = read_cpu_ticks_tree(pid).unwrap();
        assert!(
            tree >= single,
            "tree CPU ({tree}) should be >= single process CPU ({single})"
        );
    }

    #[test]
    fn read_cpu_ticks_tree_nonexistent_pid() {
        // PID 4_000_000 should not exist
        let result = read_cpu_ticks_tree(4_000_000);
        assert!(result.is_none(), "nonexistent PID should return None");
    }

    #[test]
    fn read_cpu_ticks_tree_with_children() {
        use std::process::Command;
        // Spawn a child process that does some work
        let child = Command::new("sleep").arg("0.1").spawn();
        if let Ok(mut child) = child {
            let pid = std::process::id();
            let single = read_cpu_ticks(pid).unwrap();
            let tree = read_cpu_ticks_tree(pid).unwrap();
            assert!(
                tree >= single,
                "tree CPU ({tree}) with child should be >= single ({single})"
            );
            let _ = child.wait();
        }
    }

    #[tokio::test]
    async fn batch_failure_reconciles_sidebar_state() {
        let mut model = test_model();

        let mut tmux = FakeTmux::new(vec![
            Err("batch failed".to_string()),
            Ok("@new\t%sidebar\t0\n@new\t%home_new\t1\n".to_string()),
        ]);

        let err = send_batch_with_reconcile(&mut model, &mut tmux, "join-pane ; resize-pane")
            .await
            .expect_err("batch should fail");

        assert_eq!(err, "batch failed");
        assert_eq!(model.sidebar.window_id, "@new");
        assert_eq!(model.sidebar.home_pane_id, "%home_new");
        assert_eq!(tmux.commands.len(), 2);
        assert!(tmux.commands[1].contains("list-panes -s -t s"));
    }

    #[tokio::test]
    async fn batch_failure_reconcile_falls_back_when_session_scan_misses_sidebar() {
        let mut model = test_model();

        let mut tmux = FakeTmux::new(vec![
            Err("batch failed".to_string()),
            Ok("@else\t%other\t1\n".to_string()),
            Ok("@new\n".to_string()),
            Ok("%sidebar\t0\n%home_new\t1\n".to_string()),
        ]);

        let err = send_batch_with_reconcile(&mut model, &mut tmux, "join-pane ; resize-pane")
            .await
            .expect_err("batch should fail");

        assert_eq!(err, "batch failed");
        assert_eq!(model.sidebar.window_id, "@new");
        assert_eq!(model.sidebar.home_pane_id, "%home_new");
        assert_eq!(tmux.commands.len(), 4);
        assert!(tmux.commands[1].contains("list-panes -s -t s"));
        assert!(tmux.commands[2].contains("display-message -t %sidebar -p '#{window_id}'"));
        assert!(tmux.commands[3].contains("list-panes -t @new"));
    }

    #[tokio::test]
    async fn new_window_failure_reconciles_sidebar_and_refreshes_windows() {
        let mut model = test_model();
        let mut tmux = FakeTmux::with_window_lists(
            vec![
                Ok("/work/project\n".to_string()),
                Err("boom".to_string()),
                Ok("@new\t%sidebar\t0\n@new\t%home_new\t1\n".to_string()),
            ],
            vec![Ok(vec![wi("@new", 1, "scratch")])],
        );
        let mut queue = VecDeque::new();

        handle_new_window(&mut model, &mut tmux, &mut queue, "proj:tab3".to_string()).await;

        assert_eq!(model.error_message.as_deref(), Some("new-window: boom"));
        assert_eq!(model.sidebar.window_id, "@new");
        assert_eq!(model.sidebar.home_pane_id, "%home_new");
        assert_eq!(tmux.commands.len(), 3);
        assert!(matches!(queue.pop_front(), Some(Cmd::Render)));
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn rename_window_failure_reconciles_sidebar_and_refreshes_windows() {
        let mut model = test_model();
        let mut tmux = FakeTmux::with_window_lists(
            vec![
                Ok(String::new()),
                Err("rename failed".to_string()),
                Ok("@renamed\t%sidebar\t0\n@renamed\t%home_new\t1\n".to_string()),
            ],
            vec![Ok(vec![wi("@renamed", 1, "proj:tab3")])],
        );
        let mut queue = VecDeque::new();

        handle_rename_window(
            &mut model,
            &mut tmux,
            &mut queue,
            "@1".to_string(),
            "proj:tab3".to_string(),
        )
        .await;

        assert_eq!(
            model.error_message.as_deref(),
            Some("rename-window: rename failed")
        );
        assert_eq!(model.sidebar.window_id, "@renamed");
        assert_eq!(model.sidebar.home_pane_id, "%home_new");
        assert_eq!(tmux.commands.len(), 3);
        assert!(matches!(queue.pop_front(), Some(Cmd::Render)));
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn reorder_failure_reconciles_sidebar_and_refreshes_windows() {
        let mut model = test_model();
        let mut tmux = FakeTmux::with_window_lists(
            vec![
                Err("swap failed".to_string()),
                Ok("@new\t%sidebar\t0\n@new\t%home_new\t1\n".to_string()),
            ],
            vec![
                Ok(vec![wi("@1", 1, "proj:edit"), wi("@2", 2, "proj:term")]),
                Ok(vec![wi("@new", 1, "scratch")]),
            ],
        );
        let mut queue = VecDeque::new();

        handle_reorder_windows(
            &mut model,
            &mut tmux,
            &mut queue,
            vec!["@2".to_string(), "@1".to_string()],
            SelectionTarget::Window("@1".to_string()),
        )
        .await;

        assert_eq!(
            model.error_message.as_deref(),
            Some("swap-window: swap failed")
        );
        assert_eq!(model.sidebar.window_id, "@new");
        assert_eq!(model.sidebar.home_pane_id, "%home_new");
        assert_eq!(model.reorder.pending_selection, None);
        assert_eq!(tmux.commands.len(), 2);
        assert!(matches!(queue.pop_front(), Some(Cmd::Render)));
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn close_window_failure_reconciles_sidebar_and_refreshes_windows() {
        let mut model = test_model();
        let mut tmux = FakeTmux::with_window_lists(
            vec![
                Err("kill failed".to_string()),
                Ok("@new\t%sidebar\t0\n@new\t%home_new\t1\n".to_string()),
            ],
            vec![Ok(vec![wi("@new", 1, "scratch")])],
        );
        let mut queue = VecDeque::new();

        handle_close_window(&mut model, &mut tmux, &mut queue, "@1".to_string()).await;

        assert_eq!(
            model.error_message.as_deref(),
            Some("kill-window: kill failed")
        );
        assert_eq!(model.sidebar.window_id, "@new");
        assert_eq!(model.sidebar.home_pane_id, "%home_new");
        assert_eq!(tmux.commands.len(), 2);
        assert!(matches!(queue.pop_front(), Some(Cmd::Render)));
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn new_window_uses_working_pane_cwd() {
        let mut model = test_model();
        let mut tmux = FakeTmux::new(vec![
            Ok("/work/project\n".to_string()),
            Ok("@new\n".to_string()),
            Ok(String::new()),
        ]);
        let mut queue = VecDeque::new();

        handle_new_window(&mut model, &mut tmux, &mut queue, "proj:tab3".to_string()).await;

        assert_eq!(
            tmux.commands[1],
            "new-window -d -a -t \"=s:{end}\" -c \"/work/project\" -n \"proj:tab3\" -P -F '#{window_id}'"
        );
        assert!(matches!(queue.pop_front(), Some(Cmd::ListWindows)));
        assert!(matches!(
            queue.pop_front(),
            Some(Cmd::PreviewWindow { id }) if id == "@new"
        ));
        assert!(matches!(queue.pop_front(), Some(Cmd::Render)));
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn new_window_omits_cwd_when_no_working_pane_path_is_available() {
        let mut model = test_model();
        let mut tmux = FakeTmux::new(vec![
            Err("stale pane".to_string()),
            Ok("%sidebar\t1\n".to_string()),
            Ok("@new\n".to_string()),
            Ok(String::new()),
        ]);
        let mut queue = VecDeque::new();

        handle_new_window(&mut model, &mut tmux, &mut queue, "proj:tab3".to_string()).await;

        assert_eq!(
            tmux.commands[2],
            "new-window -d -a -t \"=s:{end}\" -n \"proj:tab3\" -P -F '#{window_id}'"
        );
        assert!(matches!(queue.pop_front(), Some(Cmd::ListWindows)));
        assert!(matches!(
            queue.pop_front(),
            Some(Cmd::PreviewWindow { id }) if id == "@new"
        ));
        assert!(matches!(queue.pop_front(), Some(Cmd::Render)));
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn reorder_windows_swaps_into_requested_order() {
        let mut model = test_model();
        let mut tmux = FakeTmux::with_window_lists(
            vec![Ok(String::new()), Ok(String::new())],
            vec![Ok(vec![
                wi("@1", 1, "proj:edit"),
                wi("@2", 2, "scratch"),
                wi("@3", 3, "proj:term"),
            ])],
        );
        let mut queue = VecDeque::new();

        handle_reorder_windows(
            &mut model,
            &mut tmux,
            &mut queue,
            vec!["@3".to_string(), "@1".to_string(), "@2".to_string()],
            SelectionTarget::Window("@1".to_string()),
        )
        .await;

        assert_eq!(
            tmux.commands,
            vec!["swap-window -s @3 -t @1", "swap-window -s @1 -t @2",]
        );
        assert!(matches!(queue.front(), Some(Cmd::ListWindows)));
    }

    #[tokio::test]
    async fn reorder_windows_never_swaps_sidebar_window() {
        let mut model = test_model();
        let mut tmux = FakeTmux::with_window_lists(
            vec![Ok(String::new())],
            vec![Ok(vec![
                wi("@1", 1, "proj:edit"),
                wi("@old", 2, "sidebar"),
                wi("@2", 3, "proj:term"),
            ])],
        );
        let mut queue = VecDeque::new();

        handle_reorder_windows(
            &mut model,
            &mut tmux,
            &mut queue,
            vec!["@2".to_string(), "@1".to_string()],
            SelectionTarget::Window("@1".to_string()),
        )
        .await;

        assert_eq!(tmux.commands, vec!["swap-window -s @2 -t @1"]);
        assert!(matches!(queue.front(), Some(Cmd::ListWindows)));
    }

    #[tokio::test]
    async fn apply_layout_helper_launches_helper_apps_on_first_apply() {
        let mut model = test_model();
        model.sidebar.pane_id = "%9".to_string();
        model.set_window_list_snapshot(vec![wi("@old", 1, "proj:main")]);

        let layout = build_sidebar_main_3x2_layout(120, 40, 9, &[2, 3, 4, 5, 6, 7])
            .expect("layout should build");

        let mut tmux = FakeTmux::new(vec![
            Ok(
                "%9\t0\t0\n%2\t31\t0\n%3\t50\t0\n%4\t69\t0\n%5\t31\t20\n%6\t50\t20\n%7\t69\t20\n"
                    .to_string(),
            ),
            Ok("/work\n".to_string()),
            Ok(format!("1234,{}", layout.split_once(',').unwrap().1)),
            Ok(String::new()),
            Ok(String::new()),
            Ok("%2\tbash\n%3\tbash\n%4\tbash\n%5\tbash\n%6\tbash\n%7\tbash\n".to_string()),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
        ]);

        let mut queue = VecDeque::new();
        apply_layout_helper(&mut model, &mut tmux, &mut queue).await;

        assert!(tmux
            .commands
            .iter()
            .any(|cmd| cmd.contains("pane_current_command")));
        let disable_idx = tmux
            .commands
            .iter()
            .position(|cmd| cmd == "set-window-option -t @old automatic-rename off ; set-window-option -t @old allow-rename off")
            .expect("layout helper should disable window rename");
        let lazygit_idx = tmux
            .commands
            .iter()
            .position(|cmd| cmd == "send-keys -t %4 lazygit C-m")
            .expect("layout helper should launch lazygit");
        assert!(disable_idx < lazygit_idx);
        assert!(tmux
            .commands
            .iter()
            .any(|cmd| cmd == "send-keys -t %4 lazygit C-m"));
        assert!(tmux
            .commands
            .iter()
            .any(|cmd| cmd == "send-keys -t %7 yazi C-m"));
        assert!(tmux
            .commands
            .iter()
            .any(|cmd| cmd.starts_with("select-layout -t @old ")));
        assert!(tmux.commands.iter().any(|cmd| cmd == "select-pane -t %9"));
        assert!(model
            .sidebar
            .helper_managed_windows
            .contains(&model.sidebar.window_id));
        assert_eq!(
            model
                .renames
                .pending
                .get("@old")
                .map(|pending| pending.target_name.as_str()),
            Some("proj:main")
        );
        assert_eq!(model.renames.last_window_id.as_deref(), Some("@old"));
        assert_eq!(model.info_message.as_deref(), Some("layout helper applied"));
    }

    #[tokio::test]
    async fn apply_layout_helper_reapply_skips_launch_on_helper_managed_window() {
        let mut model = test_model();
        model.sidebar.pane_id = "%9".to_string();
        model
            .sidebar
            .helper_managed_windows
            .insert("@old".to_string());

        let layout = build_sidebar_main_3x2_layout(120, 40, 9, &[2, 3, 4, 5, 6, 7])
            .expect("layout should build");

        let mut tmux = FakeTmux::new(vec![
            Ok(
                "%9\t0\t0\n%2\t31\t0\n%3\t50\t0\n%4\t69\t0\n%5\t31\t20\n%6\t50\t20\n%7\t69\t20\n"
                    .to_string(),
            ),
            Ok("/work\n".to_string()),
            Ok(format!("1234,{}", layout.split_once(',').unwrap().1)),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
        ]);

        let mut queue = VecDeque::new();
        apply_layout_helper(&mut model, &mut tmux, &mut queue).await;

        assert!(!tmux
            .commands
            .iter()
            .any(|cmd| cmd.contains("pane_current_command")));
        assert!(tmux
            .commands
            .iter()
            .any(|cmd| cmd.starts_with("select-layout -t @old ")));
        assert!(tmux
            .commands
            .iter()
            .any(|cmd| cmd == "set-window-option -t @old automatic-rename off ; set-window-option -t @old allow-rename off"));
        assert!(tmux.commands.iter().any(|cmd| cmd == "select-pane -t %9"));
        assert!(!tmux.commands.iter().any(|cmd| cmd.contains(" lazygit C-m")));
        assert!(!tmux.commands.iter().any(|cmd| cmd.contains(" yazi C-m")));
        assert!(!tmux.commands.iter().any(|cmd| cmd.contains("cd -- ")));
        assert_eq!(
            model.info_message.as_deref(),
            Some("layout helper refreshed")
        );
    }

    #[tokio::test]
    async fn reapply_helper_layout_runs_for_helper_managed_windows() {
        let mut model = test_model();
        model.sidebar.pane_id = "%9".to_string();
        model
            .sidebar
            .helper_managed_windows
            .insert("@1".to_string());

        let layout = build_sidebar_main_3x2_layout(120, 40, 9, &[2, 3, 4, 5, 6, 7])
            .expect("layout should build");

        let mut tmux = FakeTmux::new(vec![
            Ok(
                "%9\t0\t0\n%2\t31\t0\n%3\t50\t0\n%4\t69\t0\n%5\t31\t20\n%6\t50\t20\n%7\t69\t20\n"
                    .to_string(),
            ),
            Ok(format!("1234,{}", layout.split_once(',').unwrap().1)),
            Ok(String::new()),
        ]);

        reapply_helper_layout_if_needed(&mut model, &mut tmux, "@1").await;

        assert_eq!(tmux.commands.len(), 3);
        assert!(tmux.commands[0].starts_with("list-panes -t @1"));
        assert!(tmux.commands[2].starts_with("select-layout -t @1 "));
    }

    #[tokio::test]
    async fn reapply_helper_layout_skips_non_applicable_windows() {
        let mut model = test_model();
        model
            .sidebar
            .helper_managed_windows
            .insert("@1".to_string());

        let mut tmux = FakeTmux::new(vec![Ok(
            "%sidebar\t0\t0\n%1\t31\t0\n%2\t50\t0\n%3\t69\t0\n%4\t31\t20\n".to_string(),
        )]);

        reapply_helper_layout_if_needed(&mut model, &mut tmux, "@1").await;

        assert_eq!(tmux.commands.len(), 1);
        assert!(tmux.commands[0].starts_with("list-panes -t @1"));
    }

    #[tokio::test]
    async fn follow_to_window_reapplies_helper_layout_for_helper_managed_windows() {
        let mut model = test_model();
        model.sidebar.pane_id = "%9".to_string();
        model
            .sidebar
            .helper_managed_windows
            .insert("@new".to_string());

        let mut queue = VecDeque::new();
        let explicit_layout = build_sidebar_main_3x2_layout(120, 40, 9, &[2, 3, 4, 5, 6, 7])
            .expect("layout should build");

        let mut tmux = FakeTmux::new(vec![
            Ok("%2\t0\t0\t1\n%3\t39\t0\t0\n%4\t78\t0\t0\n%5\t0\t20\t0\n%6\t39\t20\t0\n%7\t78\t20\t0\n".to_string()),
            Ok("aaaa,119x40,0,0{39x40,0,0[39x19,0,0,2,39x20,0,20,5],38x40,40,0[38x19,40,0,3,38x20,40,20,6],39x40,79,0[39x19,79,0,4,39x20,79,20,7]}".to_string()),
            Ok("9999,200x80,0,0{30x80,0,0,9,169x80,31,0[169x39,31,0,11,169x40,31,40,12]}".to_string()),
            Ok(String::new()),
            Ok(String::new()),
            Ok("%9\t0\t0\n%2\t31\t0\n%5\t31\t20\n%3\t61\t0\n%6\t61\t20\n%4\t91\t0\n%7\t91\t20\n".to_string()),
            Ok(format!("1234,{}", explicit_layout.split_once(',').unwrap().1)),
            Ok(String::new()),
        ]);

        handle_follow_to_window(&mut model, &mut tmux, &mut queue, "@new".to_string()).await;

        assert_eq!(model.sidebar.window_id, "@new");
        assert_eq!(model.sidebar.home_pane_id, "%2");
        assert!(matches!(queue.front(), Some(Cmd::CheckBorder)));
        assert!(tmux
            .commands
            .iter()
            .any(|cmd| cmd.starts_with("select-layout -t @new ")));
    }

    #[tokio::test]
    async fn follow_to_window_query_failure_does_not_set_window_change_suppression() {
        let mut model = test_model();
        let mut queue = VecDeque::new();
        let mut tmux = FakeTmux::new(vec![Ok("%sidebar\t0\t0\t1\n".to_string())]);

        handle_follow_to_window(&mut model, &mut tmux, &mut queue, "@new".to_string()).await;

        assert_eq!(model.sidebar.ignore_window_changes, 0);
        assert_eq!(model.sidebar.pending_internal_focus_window, None);
        assert_eq!(
            model.error_message.as_deref(),
            Some("follow: could not resolve leftmost pane")
        );
        assert!(matches!(queue.pop_front(), Some(Cmd::Render)));
        assert!(queue.is_empty());
        assert_eq!(tmux.commands.len(), 1);
    }

    #[tokio::test]
    async fn follow_to_window_batch_failure_clears_window_change_suppression() {
        let mut model = test_model();
        let mut queue = VecDeque::new();
        let mut tmux = FakeTmux::new(vec![
            Ok("%2\t0\t0\t1\n%sidebar\t31\t0\t0\n".to_string()),
            Ok("9999,200x80,0,0{30x80,0,0,2,70x80,31,0[70x39,31,0,3,70x40,31,39,4]}".to_string()),
            Ok("9999,200x80,0,0{30x80,0,0,2,70x80,31,0[70x39,31,0,3,70x40,31,39,4]}".to_string()),
            Err("join failed".to_string()),
            Ok("@old\t%old\t1\n@old\t%1\t0\n%sidebar\t0\t0\n".to_string()),
            Ok("@old\t0\t0\n".to_string()),
            Ok("@old\t%old\t1\n@old\t%1\t0\n%sidebar\t0\t0\n".to_string()),
            Ok("@old\n".to_string()),
        ]);

        handle_follow_to_window(&mut model, &mut tmux, &mut queue, "@new".to_string()).await;

        assert_eq!(model.sidebar.ignore_window_changes, 0);
        assert_eq!(model.sidebar.pending_internal_focus_window, None);
        assert_eq!(model.error_message.as_deref(), Some("follow: join failed"));
        assert!(matches!(queue.pop_front(), Some(Cmd::Render)));
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn preview_window_query_failure_does_not_set_window_change_suppression() {
        let mut model = test_model();
        let mut queue = VecDeque::new();
        let mut tmux = FakeTmux::new(vec![Ok("%sidebar\t0\t0\t1\n".to_string())]);

        handle_preview_window(&mut model, &mut tmux, &mut queue, "@new".to_string()).await;

        assert_eq!(model.sidebar.ignore_window_changes, 0);
        assert_eq!(model.sidebar.pending_internal_focus_window, None);
        assert_eq!(
            model.error_message.as_deref(),
            Some("preview: could not resolve leftmost pane")
        );
        assert!(matches!(queue.pop_front(), Some(Cmd::Render)));
        assert!(queue.is_empty());
        assert_eq!(tmux.commands.len(), 1);
    }

    #[tokio::test]
    async fn preview_window_batch_failure_clears_window_change_suppression() {
        let mut model = test_model();
        let mut queue = VecDeque::new();
        let mut tmux = FakeTmux::new(vec![
            Ok("%2\t0\t0\t1\n%sidebar\t31\t0\t0\n".to_string()),
            Ok("9999,200x80,0,0{30x80,0,0,2,70x80,31,0[70x39,31,0,3,70x40,31,39,4]}".to_string()),
            Ok("9999,200x80,0,0{30x80,0,0,2,70x80,31,0[70x39,31,0,3,70x40,31,39,4]}".to_string()),
            Err("preview join failed".to_string()),
            Ok("@old\t%old\t1\n@old\t%1\t0\n%sidebar\t0\t0\n".to_string()),
            Ok("@old\t0\t0\n".to_string()),
            Ok("@old\t%old\t1\n@old\t%1\t0\n%sidebar\t0\t0\n".to_string()),
            Ok("@old\n".to_string()),
        ]);

        handle_preview_window(&mut model, &mut tmux, &mut queue, "@new".to_string()).await;

        assert_eq!(model.sidebar.ignore_window_changes, 0);
        assert_eq!(model.sidebar.pending_internal_focus_window, None);
        assert_eq!(
            model.error_message.as_deref(),
            Some("preview: preview join failed")
        );
        assert!(matches!(queue.pop_front(), Some(Cmd::Render)));
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn restore_preview_uses_one_pane_query_for_target_and_home() {
        let mut model = test_model();
        model.terminal_size = (120, 40);
        model.sidebar.window_id = "@cur".to_string();
        model.sidebar.home_pane_id = "%cur_home".to_string();
        model.sidebar.preview = PreviewState::Previewing {
            original_window_id: "@orig".to_string(),
            original_home_pane_id: "%orig_home".to_string(),
        };
        model
            .sidebar
            .pane_layouts
            .insert("@cur".to_string(), (120, "layout-cur".to_string()));
        model
            .sidebar
            .pane_layouts
            .insert("@orig".to_string(), (120, "layout-orig".to_string()));

        let mut tmux = FakeTmux::new(vec![
            Ok("%orig_a\t0\t0\t0\n%orig_b\t40\t0\t1\n%sidebar\t80\t0\t0\n".to_string()),
            Ok("layout-orig\n".to_string()),
            Ok("layout-cur\n".to_string()),
            Ok(String::new()),
            Ok(String::new()),
        ]);
        let mut queue = VecDeque::new();

        handle_restore_preview(&mut model, &mut tmux, &mut queue).await;

        assert_eq!(tmux.commands.len(), 5);
        assert!(tmux.commands[0].starts_with("list-panes -t @orig"));
        assert!(tmux.commands[3].starts_with("join-pane -dfhb -l 30 -s %sidebar -t %orig_a"));
        assert_eq!(model.sidebar.window_id, "@orig");
        assert_eq!(model.sidebar.home_pane_id, "%orig_b");
        assert!(matches!(queue.front(), Some(Cmd::CheckBorder)));
    }
}
