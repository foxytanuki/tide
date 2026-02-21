use std::collections::VecDeque;
use std::io::Stdout;

use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tracing::{debug, info, warn};

use crate::cmd::Cmd;
use crate::model::{Model, PreviewState};
use crate::msg::Msg;
use crate::tmux::{quote_tmux, TmuxControl};
use crate::update::update;
use crate::view::render;

pub type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

pub async fn execute_commands(
    model: &mut Model,
    tmux: &mut TmuxControl,
    terminal: &mut AppTerminal,
    cmds: Vec<Cmd>,
) -> bool {
    let mut queue: VecDeque<Cmd> = cmds.into();

    while let Some(cmd) = queue.pop_front() {
        match cmd {
            Cmd::PreviewWindow {
                id: target_window_id,
            } => {
                // If previewing and target is the original window, restore
                if let PreviewState::Previewing {
                    ref original_window_id,
                    ..
                } = model.preview
                {
                    if target_window_id == *original_window_id {
                        debug!(target = %target_window_id, "preview target is original, restoring");
                        queue.push_front(Cmd::Render);
                        queue.push_front(Cmd::RestorePreview);
                        continue;
                    }
                }

                // Already on this window, no-op
                if target_window_id == model.sidebar_window_id {
                    continue;
                }

                debug!(target = %target_window_id, "previewing window");

                // Record original state if starting fresh preview
                let (orig_window, orig_home) = match &model.preview {
                    PreviewState::Home => {
                        (model.sidebar_window_id.clone(), model.home_pane_id.clone())
                    }
                    PreviewState::Previewing {
                        original_window_id,
                        original_home_pane_id,
                    } => (original_window_id.clone(), original_home_pane_id.clone()),
                };

                let new_home =
                    match move_sidebar_to_window(model, tmux, &target_window_id).await {
                        Ok(h) => h,
                        Err(err) => {
                            warn!(%err, "preview move failed");
                            model.error_message = Some(format!("preview {err}"));
                            queue.push_front(Cmd::Render);
                            continue;
                        }
                    };

                if let Err(err) =
                    focus_sidebar_in_window(tmux, &model.sidebar_pane_id, &target_window_id).await
                {
                    warn!(%err, "preview focus failed");
                    model.error_message = Some(format!("preview {err}"));
                    queue.push_front(Cmd::Render);
                    continue;
                }

                if !new_home.is_empty() {
                    model.home_pane_id = new_home.clone();
                }

                debug!(
                    sidebar_window = %target_window_id,
                    home_pane = %model.home_pane_id,
                    orig_window = %orig_window,
                    "preview active"
                );

                model.sidebar_window_id = target_window_id;
                model.preview = PreviewState::Previewing {
                    original_window_id: orig_window,
                    original_home_pane_id: orig_home,
                };
            }
            Cmd::RestorePreview => {
                if let PreviewState::Previewing {
                    ref original_window_id,
                    ref original_home_pane_id,
                } = model.preview
                {
                    let orig_window = original_window_id.clone();
                    let orig_home = original_home_pane_id.clone();

                    debug!(orig_window = %orig_window, "restoring preview");

                    // Suppress session-window-changed events from join-pane + select-window
                    model.ignore_window_changes = 2;
                    let source_window = model.sidebar_window_id.clone();
                    remember_window_layout_without_sidebar(model, tmux, &orig_window).await;

                    // Move sidebar back to original window
                    let join_cmd_by_home = format!(
                        "join-pane -dfhb -s {} -t {}",
                        model.sidebar_pane_id, orig_home
                    );
                    if tmux.send_command(&join_cmd_by_home).await.is_err() {
                        let join_cmd_by_window = format!(
                            "join-pane -dfhb -s {} -t {}",
                            model.sidebar_pane_id, orig_window
                        );
                        let _ = tmux.send_command(&join_cmd_by_window).await;
                    }
                    restore_window_layout_without_sidebar(model, tmux, &source_window).await;

                    // Switch back to original window
                    let _ = tmux
                        .send_command(&format!("select-window -t {}", orig_window))
                        .await;

                    // Focus sidebar
                    let _ = tmux
                        .send_command(&format!("select-pane -t {}", model.sidebar_pane_id))
                        .await;

                    // Force sidebar to exactly 30 columns LAST — select-window
                    // triggers layout recalculation that can override earlier resize
                    if let Err(err) = tmux
                        .send_command(&format!(
                            "resize-pane -t {} -x 30",
                            model.sidebar_pane_id
                        ))
                        .await
                    {
                        warn!(%err, "restore resize-pane failed");
                    }

                    model.sidebar_window_id = orig_window;
                    model.home_pane_id = orig_home;
                    model.preview = PreviewState::Home;
                }
            }
            Cmd::FocusRightPane => {
                debug!("focusing right pane");
                if let Err(err) = tmux.send_command("select-pane -R").await {
                    warn!(%err, "select-pane -R failed");
                    model.error_message = Some(format!("select-pane: {err}"));
                    queue.push_front(Cmd::Render);
                }
            }
            Cmd::NewWindow { name } => {
                debug!(name, "creating new window");
                let new_cmd = format!("new-window -d -n {}", quote_tmux(&name));
                if let Err(err) = tmux.send_command(&new_cmd).await {
                    warn!(%err, "new-window failed");
                    model.error_message = Some(format!("new-window: {err}"));
                    queue.push_front(Cmd::Render);
                }
            }
            Cmd::RenameWindow { id, name } => {
                debug!(id, name, "renaming window");
                let cmd_str = format!("rename-window -t {} {}", id, quote_tmux(&name));
                if let Err(err) = tmux.send_command(&cmd_str).await {
                    warn!(%err, "rename-window failed");
                    model.error_message = Some(format!("rename-window: {err}"));
                    queue.push_front(Cmd::Render);
                }
            }
            Cmd::CloseWindow { id } => {
                // If sidebar is in the window being closed, restore preview first
                if let PreviewState::Previewing { .. } = &model.preview {
                    if model.sidebar_window_id == id {
                        debug!(id, "closing previewed window, restoring first");
                        queue.push_front(Cmd::CloseWindow { id });
                        queue.push_front(Cmd::RestorePreview);
                        continue;
                    }
                }
                debug!(id, "closing window");
                let cmd_str = format!("kill-window -t {id}");
                if let Err(err) = tmux.send_command(&cmd_str).await {
                    warn!(%err, "kill-window failed");
                    model.error_message = Some(format!("kill-window: {err}"));
                    queue.push_front(Cmd::Render);
                } else {
                    // Proactively refresh — don't rely solely on %window-close notification
                    queue.push_front(Cmd::ListWindows);
                }
            }
            Cmd::FollowToWindow {
                window_id: target_window_id,
            } => {
                if target_window_id == model.sidebar_window_id {
                    continue;
                }

                info!(target = %target_window_id, "following to window");

                let new_home =
                    match move_sidebar_to_window(model, tmux, &target_window_id).await {
                        Ok(h) => h,
                        Err(err) => {
                            warn!(%err, "follow move failed");
                            model.error_message = Some(format!("follow {err}"));
                            queue.push_front(Cmd::Render);
                            continue;
                        }
                    };

                if !new_home.is_empty() {
                    model.home_pane_id = new_home;
                }

                // Force sidebar to exactly 30 columns — don't select-window/select-pane
                // because the user initiated this window switch themselves
                if let Err(err) = tmux
                    .send_command(&format!(
                        "resize-pane -t {} -x 30",
                        model.sidebar_pane_id
                    ))
                    .await
                {
                    warn!(%err, "follow resize-pane failed");
                }

                debug!(
                    sidebar_window = %target_window_id,
                    home_pane = %model.home_pane_id,
                    "follow complete"
                );
                model.sidebar_window_id = target_window_id;
            }
            Cmd::EnsureSidebarWidth => {
                if let Err(err) = tmux
                    .send_command(&format!(
                        "resize-pane -t {} -x 30",
                        model.sidebar_pane_id
                    ))
                    .await
                {
                    warn!(%err, "ensure resize-pane failed");
                }
            }
            Cmd::ListWindows => match tmux.list_windows().await {
                Ok(windows) => {
                    debug!(count = windows.len(), "refreshed window list");
                    let follow_up = update(model, Msg::WindowListLoaded(windows));
                    for c in follow_up.into_iter().rev() {
                        queue.push_front(c);
                    }
                }
                Err(err) => {
                    warn!(%err, "list-windows failed");
                    model.error_message = Some(format!("list-windows: {err}"));
                    queue.push_front(Cmd::Render);
                }
            },
            Cmd::Render => {
                if let Err(err) = terminal.draw(|f| render(model, f)) {
                    model.error_message = Some(format!("render: {err}"));
                }
            }
            Cmd::Quit => return false,
            Cmd::Batch(batch) => {
                for c in batch.into_iter().rev() {
                    queue.push_front(c);
                }
            }
        }
    }

    true
}

/// Move sidebar pane to the target window, preserving layouts.
/// Sets `ignore_window_changes = 2` and handles layout save/restore.
/// Returns the new home pane id in the target window (may be empty).
async fn move_sidebar_to_window(
    model: &mut Model,
    tmux: &mut TmuxControl,
    target_window_id: &str,
) -> Result<String, String> {
    model.ignore_window_changes = 2;
    let source_window = model.sidebar_window_id.clone();
    remember_window_layout_without_sidebar(model, tmux, target_window_id).await;

    let target_home =
        choose_home_pane_in_window(tmux, target_window_id, &model.sidebar_pane_id).await;
    let join_target = if target_home.is_empty() {
        target_window_id.to_string()
    } else {
        target_home.clone()
    };

    let join_cmd = format!(
        "join-pane -dfhb -s {} -t {}",
        model.sidebar_pane_id, join_target
    );
    if let Err(err) = tmux.send_command(&join_cmd).await {
        return Err(format!("join-pane: {err}"));
    }
    restore_window_layout_without_sidebar(model, tmux, &source_window).await;

    // Resolve new home pane — re-query only if first attempt found nothing
    let new_home = if !target_home.is_empty() {
        target_home
    } else {
        choose_home_pane_in_window(tmux, target_window_id, &model.sidebar_pane_id).await
    };

    Ok(new_home)
}

/// Switch to the target window, focus sidebar pane, and force 30-column width.
async fn focus_sidebar_in_window(
    tmux: &mut TmuxControl,
    sidebar_pane_id: &str,
    target_window_id: &str,
) -> Result<(), String> {
    if let Err(err) = tmux
        .send_command(&format!("select-window -t {}", target_window_id))
        .await
    {
        return Err(format!("select-window: {err}"));
    }
    let _ = tmux
        .send_command(&format!("select-pane -t {}", sidebar_pane_id))
        .await;
    if let Err(err) = tmux
        .send_command(&format!("resize-pane -t {} -x 30", sidebar_pane_id))
        .await
    {
        warn!(%err, "resize-pane failed");
    }
    Ok(())
}

async fn choose_home_pane_in_window(
    tmux: &mut TmuxControl,
    window_id: &str,
    sidebar_pane_id: &str,
) -> String {
    let pane_list = tmux
        .send_command(&format!(
            "list-panes -t {} -F '#{{pane_id}}\t#{{pane_active}}'",
            window_id
        ))
        .await
        .unwrap_or_default();

    let mut first_non_sidebar = String::new();
    for line in pane_list.lines() {
        let mut parts = line.split('\t');
        let pane_id = parts.next().unwrap_or("").trim();
        let active = parts.next().unwrap_or("").trim();
        if pane_id.is_empty() || pane_id == sidebar_pane_id {
            continue;
        }
        if first_non_sidebar.is_empty() {
            first_non_sidebar = pane_id.to_string();
        }
        if active == "1" {
            return pane_id.to_string();
        }
    }

    first_non_sidebar
}

async fn read_window_layout(tmux: &mut TmuxControl, window_id: &str) -> Option<String> {
    if window_id.is_empty() {
        return None;
    }
    let out = tmux
        .send_command(&format!(
            "display-message -t {} -p '#{{window_layout}}'",
            window_id
        ))
        .await
        .ok()?;
    let layout = out.trim();
    if layout.is_empty() {
        None
    } else {
        Some(layout.to_string())
    }
}

async fn remember_window_layout_without_sidebar(
    model: &mut Model,
    tmux: &mut TmuxControl,
    window_id: &str,
) {
    if let Some(layout) = read_window_layout(tmux, window_id).await {
        model
            .layout_without_sidebar_by_window
            .insert(window_id.to_string(), layout);
    }
}

async fn restore_window_layout_without_sidebar(
    model: &Model,
    tmux: &mut TmuxControl,
    window_id: &str,
) {
    let Some(layout) = model.layout_without_sidebar_by_window.get(window_id) else {
        return;
    };
    if let Err(err) = tmux
        .send_command(&format!(
            "select-layout -t {} {}",
            window_id,
            quote_tmux(layout)
        ))
        .await
    {
        warn!(%err, window = window_id, layout = %layout, "restore window layout failed");
    }
}
