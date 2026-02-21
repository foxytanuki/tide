use std::collections::VecDeque;
use std::fmt::Write as _;
use std::future::Future;
use std::io::Stdout;
use std::pin::Pin;

use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tracing::{debug, info, warn};

use crate::cmd::Cmd;
use crate::model::{Model, PendingRename, PreviewState};
use crate::msg::Msg;
use crate::tmux::{quote_tmux, TmuxControl, WindowInfo};
use crate::update::update;
use crate::view::render;

pub type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

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

                let source_window = model.sidebar_window_id.clone();
                // suppress 2 events: join-pane + select-window
                model.ignore_window_changes = 2;
                model.pending_internal_focus_window = Some(target_window_id.clone());

                // Query phase: save target layout and find join target
                remember_window_layout_without_sidebar(model, tmux, &target_window_id).await;
                let target_home =
                    choose_home_pane_in_window(tmux, &target_window_id, &model.sidebar_pane_id)
                        .await;
                let join_target = if target_home.is_empty() {
                    target_window_id.clone()
                } else {
                    target_home.clone()
                };

                // Action phase: batch all visual tmux operations into a single
                // command so tmux processes them in one server tick (no flicker).
                // -l 30 sets sidebar width at join time to avoid intermediate resize.
                let mut batch = format!(
                    "join-pane -dfhb -l 30 -s {} -t {} ; select-window -t {} ; select-pane -t {}",
                    model.sidebar_pane_id, join_target,
                    target_window_id,
                    model.sidebar_pane_id,
                );
                if let Some(layout) = model.layout_without_sidebar_by_window.get(&source_window) {
                    write!(batch, " ; select-layout -t {} {}", source_window, quote_tmux(layout)).unwrap();
                }

                if let Err(err) = send_batch_with_reconcile(model, tmux, &batch).await {
                    warn!(%err, "preview batch failed");
                    clear_internal_window_change_suppression(model);
                    model.error_message = Some(format!("preview: {err}"));
                    queue.push_front(Cmd::Render);
                    continue;
                }

                // Resolve home pane if needed (query after move)
                let new_home = if !target_home.is_empty() {
                    target_home
                } else {
                    choose_home_pane_in_window(tmux, &target_window_id, &model.sidebar_pane_id)
                        .await
                };

                if !new_home.is_empty() {
                    model.home_pane_id = new_home;
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
                    model.pending_internal_focus_window = Some(orig_window.clone());
                    let source_window = model.sidebar_window_id.clone();
                    remember_window_layout_without_sidebar(model, tmux, &orig_window).await;

                    // Batch: join sidebar back + switch + focus + restore source
                    let mut batch = format!(
                        "join-pane -dfhb -l 30 -s {} -t {} ; select-window -t {} ; select-pane -t {}",
                        model.sidebar_pane_id, orig_home,
                        orig_window,
                        model.sidebar_pane_id,
                    );
                    if let Some(layout) = model.layout_without_sidebar_by_window.get(&source_window) {
                        write!(batch, " ; select-layout -t {} {}", source_window, quote_tmux(layout)).unwrap();
                    }

                    let restored = if let Err(err) = send_batch_with_reconcile(model, tmux, &batch).await {
                        warn!(%err, "restore batch failed, trying fallback");
                        // Fallback: join to window instead of home pane
                        let mut fallback = format!(
                            "join-pane -dfhb -l 30 -s {} -t {} ; select-window -t {} ; select-pane -t {}",
                            model.sidebar_pane_id, orig_window,
                            orig_window,
                            model.sidebar_pane_id,
                        );
                        if let Some(layout) = model.layout_without_sidebar_by_window.get(&source_window) {
                            write!(fallback, " ; select-layout -t {} {}", source_window, quote_tmux(layout)).unwrap();
                        }
                        if let Err(err) = send_batch_with_reconcile(model, tmux, &fallback).await {
                            warn!(%err, "restore fallback batch also failed");
                            false
                        } else {
                            true
                        }
                    } else {
                        true
                    };

                    if restored {
                        model.sidebar_window_id = orig_window;
                        model.home_pane_id = orig_home;
                        model.preview = PreviewState::Home;
                    } else {
                        // Keep reconciled runtime state from failed batch attempts.
                        clear_internal_window_change_suppression(model);
                        if model.sidebar_window_id == orig_window {
                            model.preview = PreviewState::Home;
                        }
                        model.error_message =
                            Some("restore preview failed; state reconciled".to_string());
                    }
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
                let new_cmd = format!(
                    "new-window -d -n {} -P -F '#{{window_id}}'",
                    quote_tmux(&name)
                );
                match tmux.send_command(&new_cmd).await {
                    Ok(output) => {
                        let window_id = output.trim();
                        if !window_id.is_empty() {
                            let disable_rename = format!(
                                "set-window-option -t {} automatic-rename off ; set-window-option -t {} allow-rename off",
                                window_id, window_id
                            );
                            if let Err(err) = tmux.send_command(&disable_rename).await {
                                warn!(
                                    id = %window_id,
                                    name = %name,
                                    %err,
                                    "failed to disable rename updates for new window"
                                );
                            } else {
                                debug!(
                                    id = %window_id,
                                    "disabled automatic/allow-rename for new window"
                                );
                            }

                            model.pending_renames.insert(
                                window_id.to_string(),
                                PendingRename {
                                    target_name: name.clone(),
                                    observed_count: 0,
                                },
                            );
                            model.pending_rename_last_window_id = Some(window_id.to_string());
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
                        warn!(%err, "new-window failed");
                        model.error_message = Some(format!("new-window: {err}"));
                        queue.push_front(Cmd::Render);
                    }
                }
            }
            Cmd::RenameWindow { id, name } => {
                debug!(id, name, "renaming window");
                let disable_rename = format!(
                    "set-window-option -t {} automatic-rename off ; set-window-option -t {} allow-rename off",
                    id, id
                );
                if let Err(err) = tmux.send_command(&disable_rename).await {
                    warn!(%id, %err, "failed to disable automatic-rename before rename");
                } else {
                    debug!(%id, "disabled rename options before rename");
                }
                let cmd_str = format!("rename-window -t {} {}", id, quote_tmux(&name));
                if let Err(err) = tmux.send_command(&cmd_str).await {
                    warn!(%err, "rename-window failed");
                    model.error_message = Some(format!("rename-window: {err}"));
                    queue.push_front(Cmd::Render);
                } else {
                    if let Err(err) = tmux
                        .send_command(&disable_rename)
                        .await
                    {
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

                let source_window = model.sidebar_window_id.clone();
                // suppress 1 event: join-pane only (no select-window in follow path)
                model.ignore_window_changes = 1;
                model.pending_internal_focus_window = Some(target_window_id.clone());

                // Query phase
                remember_window_layout_without_sidebar(model, tmux, &target_window_id).await;
                let target_home =
                    choose_home_pane_in_window(tmux, &target_window_id, &model.sidebar_pane_id)
                        .await;
                let join_target = if target_home.is_empty() {
                    target_window_id.clone()
                } else {
                    target_home.clone()
                };

                // Action phase: batch join (+ optional source restore)
                let mut batch = format!(
                    "join-pane -dfhb -l 30 -s {} -t {}",
                    model.sidebar_pane_id, join_target,
                );
                if let Some(layout) = model.layout_without_sidebar_by_window.get(&source_window) {
                    write!(batch, " ; select-layout -t {} {}", source_window, quote_tmux(layout)).unwrap();
                }

                if let Err(err) = send_batch_with_reconcile(model, tmux, &batch).await {
                    warn!(%err, "follow batch failed");
                    clear_internal_window_change_suppression(model);
                    model.error_message = Some(format!("follow: {err}"));
                    queue.push_front(Cmd::Render);
                    continue;
                }

                // Resolve home pane
                let new_home = if !target_home.is_empty() {
                    target_home
                } else {
                    choose_home_pane_in_window(tmux, &target_window_id, &model.sidebar_pane_id)
                        .await
                };

                if !new_home.is_empty() {
                    model.home_pane_id = new_home;
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
            Cmd::Restart => return false,
            Cmd::Quit => return false,
        }
    }

    true
}

fn clear_internal_window_change_suppression(model: &mut Model) {
    model.ignore_window_changes = 0;
    model.pending_internal_focus_window = None;
}

async fn send_batch_with_reconcile<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    batch: &str,
) -> Result<(), String> {
    if let Err(err) = tmux.send_command(batch).await {
        let old_window = model.sidebar_window_id.clone();
        let old_home = model.home_pane_id.clone();
        warn!(
            %err,
            old_window = %old_window,
            old_home = %old_home,
            batch = %batch,
            "reconcile: batch failed; syncing state"
        );
        // A semicolon-joined tmux command can fail after partially applying earlier
        // subcommands. Re-sync model with tmux before returning the error.
        reconcile_sidebar_state(model, tmux).await;
        warn!(
            new_window = %model.sidebar_window_id,
            new_home = %model.home_pane_id,
            "reconcile: state synced after batch failure"
        );
        return Err(err);
    }
    Ok(())
}

async fn reconcile_sidebar_state<T: TmuxApi>(model: &mut Model, tmux: &mut T) {
    let mut window_updated = false;
    let current_window = tmux
        .send_command(&format!(
            "display-message -t {} -p '#{{window_id}}'",
            model.sidebar_pane_id
        ))
        .await
        .ok()
        .map(|out| out.trim().to_string())
        .filter(|v| !v.is_empty());

    if let Some(window_id) = current_window {
        if model.sidebar_window_id != window_id {
            info!(
                old = %model.sidebar_window_id,
                new = %window_id,
                "reconcile: sidebar window id updated"
            );
        }
        model.sidebar_window_id = window_id;
        window_updated = true;
    }

    let new_home =
        choose_home_pane_in_window(tmux, &model.sidebar_window_id, &model.sidebar_pane_id).await;
    if !new_home.is_empty() {
        if model.home_pane_id != new_home {
            info!(
                old = %model.home_pane_id,
                new = %new_home,
                window = %model.sidebar_window_id,
                "reconcile: home pane id updated"
            );
        }
        model.home_pane_id = new_home;
    } else if window_updated {
        warn!(
            window = %model.sidebar_window_id,
            "reconcile: could not determine non-sidebar home pane"
        );
    }
}

async fn choose_home_pane_in_window<T: TmuxApi>(
    tmux: &mut T,
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

async fn read_window_layout<T: TmuxApi>(tmux: &mut T, window_id: &str) -> Option<String> {
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

async fn remember_window_layout_without_sidebar<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    window_id: &str,
) {
    if let Some(layout) = read_window_layout(tmux, window_id).await {
        model
            .layout_without_sidebar_by_window
            .insert(window_id.to_string(), layout);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    struct FakeTmux {
        responses: VecDeque<Result<String, String>>,
        commands: Vec<String>,
    }

    impl FakeTmux {
        fn new(responses: Vec<Result<String, String>>) -> Self {
            Self {
                responses: responses.into(),
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
            Box::pin(async move { Err("not used in this test".to_string()) })
        }
    }

    #[tokio::test]
    async fn batch_failure_reconciles_sidebar_state() {
        let mut model = test_model();

        let mut tmux = FakeTmux::new(vec![
            Err("batch failed".to_string()),
            Ok("@new\n".to_string()),
            Ok("%sidebar\t1\n%home_new\t0\n".to_string()),
        ]);

        let err = send_batch_with_reconcile(&mut model, &mut tmux, "join-pane ; resize-pane")
            .await
            .expect_err("batch should fail");

        assert_eq!(err, "batch failed");
        assert_eq!(model.sidebar_window_id, "@new");
        assert_eq!(model.home_pane_id, "%home_new");
        assert_eq!(tmux.commands.len(), 3);
        assert!(tmux.commands[1].contains("display-message -t %sidebar -p '#{window_id}'"));
        assert!(tmux.commands[2].contains("list-panes -t @new"));
    }
}
