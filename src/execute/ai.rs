use std::collections::{HashMap, HashSet, VecDeque};

use tracing::{debug, warn};

use super::{enqueue_follow_up, TmuxApi};
use crate::cmd::Cmd;
use crate::model::Model;
use crate::msg::Msg;
use crate::update::update;

pub(super) async fn poll_ai_processes<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    queue: &mut VecDeque<Cmd>,
) {
    if model.ai.output_suppress > 0 {
        model.ai.output_suppress -= 1;
        model.ai.output_counts.clear();
    }
    if model.ai.poll_skip_ticks > 0 {
        let poll_skip_ticks = model.ai.poll_skip_ticks;
        model.ai.poll_skip_ticks -= 1;
        tracing::trace!(
            idle_polls = model.ai.idle_polls,
            poll_skip_ticks,
            next_poll_skip_ticks = model.ai.poll_skip_ticks,
            "ai poll skipped by backoff"
        );
        return;
    }
    let list_cmd = format!(
        "list-panes -s -t {} -F '#{{window_id}}\t#{{pane_id}}\t#{{pane_current_command}}\t#{{pane_pid}}'",
        model.session_name
    );
    match tmux.send_command(&list_cmd).await {
        Ok(output) => {
            let candidates = find_ai_pane_candidates(&output, &model.sidebar.pane_id);
            if candidates.is_empty() {
                model.ai.cpu_tracker.clear();
                model.ai.output_counts.clear();
                schedule_ai_poll_backoff(model);
                enqueue_follow_up(
                    queue,
                    update(
                        model,
                        Msg::AiProcessPollResult {
                            panes: HashSet::new(),
                            windows: HashSet::new(),
                        },
                    ),
                );
                return;
            }

            let prev_cpu = std::mem::take(&mut model.ai.cpu_tracker);
            let output_counts = std::mem::take(&mut model.ai.output_counts);
            let candidate_count = candidates.len();
            if model.ai.idle_polls > 0 || model.ai.poll_skip_ticks > 0 {
                tracing::trace!(
                    idle_polls = model.ai.idle_polls,
                    poll_skip_ticks = model.ai.poll_skip_ticks,
                    candidates = candidate_count,
                    "ai poll backoff reset"
                );
            }
            model.ai.idle_polls = 0;
            model.ai.poll_skip_ticks = 0;
            let classify_started_at = std::time::Instant::now();
            match tokio::task::spawn_blocking(move || {
                let mut prev_cpu = prev_cpu;
                let mut output_counts = output_counts;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    classify_active_panes(&candidates, &mut prev_cpu, &mut output_counts)
                }));
                match result {
                    Ok((panes, windows)) => Ok((panes, windows, prev_cpu, output_counts)),
                    Err(_) => Err((prev_cpu, output_counts)),
                }
            })
            .await
            {
                Ok(Ok((panes, windows, prev_cpu, output_counts))) => {
                    model.ai.cpu_tracker = prev_cpu;
                    model.ai.output_counts = output_counts;
                    tracing::trace!(
                        candidates = candidate_count,
                        elapsed_us = classify_started_at.elapsed().as_micros() as u64,
                        "ai classification completed"
                    );
                    enqueue_follow_up(
                        queue,
                        update(model, Msg::AiProcessPollResult { panes, windows }),
                    );
                }
                Ok(Err((prev_cpu, output_counts))) => {
                    model.ai.cpu_tracker = prev_cpu;
                    model.ai.output_counts = output_counts;
                    warn!("ai classification panicked; restored tracking state");
                }
                Err(err) => {
                    warn!(%err, "ai classification task failed");
                }
            }
        }
        Err(err) => {
            debug!(%err, "ai process poll failed");
        }
    }
}

fn schedule_ai_poll_backoff(model: &mut Model) {
    model.ai.idle_polls = model.ai.idle_polls.saturating_add(1);
    model.ai.poll_skip_ticks = match model.ai.idle_polls {
        0..=3 => 0,
        4..=7 => 1,
        8..=15 => 2,
        16..=31 => 4,
        _ => 8,
    };
    tracing::trace!(
        idle_polls = model.ai.idle_polls,
        poll_skip_ticks = model.ai.poll_skip_ticks,
        "ai poll backoff updated"
    );
}

pub(super) async fn check_border<T: TmuxApi>(model: &mut Model, tmux: &mut T) {
    let wanted: HashSet<String> = model.ai.panes.clone();
    let current = &model.ai.highlighted_panes;

    let to_remove: Vec<String> = current.difference(&wanted).cloned().collect();
    let to_add: Vec<String> = wanted.difference(current).cloned().collect();

    if !to_remove.is_empty() || !to_add.is_empty() {
        let mut parts: Vec<String> = Vec::new();
        for pane_id in &to_remove {
            parts.push(format!(
                "set-option -p -t {} -u pane-border-format",
                pane_id
            ));
            debug!(pane = %pane_id, "pane border format reset");
        }
        for pane_id in &to_add {
            parts.push(format!(
                "set-option -p -t {} pane-border-format \" #{{?pane_active,#[fg=yellow#,bold]● #P: #{{pane_current_command}} #{{pane_current_path}},#[fg=yellow]● #P: #{{pane_current_command}}}} \"",
                pane_id
            ));
            debug!(pane = %pane_id, "pane border format set to AI active");
        }
        let batch = parts.join(" ; ");
        let _ = tmux.send_command(&batch).await;
    }

    model.ai.highlighted_panes = wanted;
}

pub(super) async fn reset_all_borders<T: TmuxApi>(model: &mut Model, tmux: &mut T) {
    for pane_id in model.ai.highlighted_panes.drain() {
        let reset_cmd = format!("set-option -p -t {} -u pane-border-format", pane_id);
        let _ = tmux.send_command(&reset_cmd).await;
        debug!(pane = %pane_id, "pane border format reset on cleanup");
    }
}

const AI_PROCESS_NAMES: &[&str] = &["claude", "codex", "gemini", "opencode"];

pub(super) fn is_ai_process_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    AI_PROCESS_NAMES
        .iter()
        .any(|ai_name| lower.contains(ai_name))
}

#[derive(Clone)]
pub(super) struct AiPaneCandidate {
    pub pane_id: String,
    pub window_id: String,
    pub pane_pid: u32,
}

pub(super) fn find_ai_pane_candidates(output: &str, sidebar_pane_id: &str) -> Vec<AiPaneCandidate> {
    let mut candidates = Vec::new();
    for line in output.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 4 {
            continue;
        }
        let window_id = parts[0].trim();
        let pane_id = parts[1].trim();
        let command = parts[2].trim();
        let pid_str = parts[3].trim();

        if pane_id == sidebar_pane_id {
            continue;
        }

        if !is_ai_process_name(command) {
            continue;
        }

        if let Ok(pane_pid) = pid_str.parse::<u32>() {
            candidates.push(AiPaneCandidate {
                pane_id: pane_id.to_string(),
                window_id: window_id.to_string(),
                pane_pid,
            });
        }
    }
    candidates
}

pub(super) const AI_CPU_GRACE_POLLS: u16 = 6;
pub(super) const MIN_OUTPUT_BURST: u32 = 6;
const MIN_CONSECUTIVE_BURSTS: u8 = 2;
const MIN_CPU_DELTA_FOR_GRACE: u64 = 50;
const AI_DEBUG: bool = false;
const MAX_PROC_TREE_DEPTH: u8 = 8;

pub(super) fn classify_active_panes(
    candidates: &[AiPaneCandidate],
    prev_cpu: &mut HashMap<String, (u32, u64, u16, u8)>,
    output_counts: &mut HashMap<String, u32>,
) -> (HashSet<String>, HashSet<String>) {
    let mut active_panes = HashSet::new();
    let mut active_windows = HashSet::new();
    let mut seen_panes = HashSet::new();

    for candidate in candidates {
        seen_panes.insert(candidate.pane_id.clone());

        let ai_pid = find_ai_child_pid(candidate.pane_pid).unwrap_or(candidate.pane_pid);
        let current_cpu = read_cpu_ticks_tree(ai_pid).unwrap_or(0);

        let output_count = output_counts.get(&candidate.pane_id).copied().unwrap_or(0);
        let is_burst = output_count >= MIN_OUTPUT_BURST;

        let (is_active, new_polls, new_bursts) = if let Some(&(
            prev_pid,
            prev_ticks,
            polls,
            bursts,
        )) = prev_cpu.get(&candidate.pane_id)
        {
            if prev_pid != ai_pid {
                let bursts = if is_burst { 1 } else { 0 };
                let activated = bursts >= MIN_CONSECUTIVE_BURSTS;
                (activated, if activated { 1 } else { 0 }, bursts)
            } else if is_burst {
                let bursts = bursts.saturating_add(1);
                if polls >= 1 || bursts >= MIN_CONSECUTIVE_BURSTS {
                    (true, 1, bursts)
                } else {
                    (false, 0, bursts)
                }
            } else if polls == 0 {
                (false, 0, 0)
            } else if polls <= AI_CPU_GRACE_POLLS {
                if output_count > 0 {
                    (true, 1, 0)
                } else {
                    let cpu_delta = current_cpu.saturating_sub(prev_ticks);
                    if cpu_delta >= MIN_CPU_DELTA_FOR_GRACE {
                        (true, 1, 0)
                    } else {
                        let new_polls = polls.saturating_add(1);
                        (new_polls <= AI_CPU_GRACE_POLLS, new_polls, 0)
                    }
                }
            } else {
                (false, 0, 0)
            }
        } else {
            let bursts = if is_burst { 1 } else { 0 };
            (false, 0, bursts)
        };

        if AI_DEBUG {
            let prev_info = prev_cpu.get(&candidate.pane_id);
            let prev_ticks = prev_info.map(|&(_, ticks, _, _)| ticks).unwrap_or(0);
            let prev_polls = prev_info.map(|&(_, _, polls, _)| polls).unwrap_or(0);
            let prev_bursts = prev_info.map(|&(_, _, _, bursts)| bursts).unwrap_or(0);
            let cpu_delta = current_cpu.saturating_sub(prev_ticks);
            let single_cpu = read_cpu_ticks(ai_pid).unwrap_or(0);
            let single_delta = single_cpu.saturating_sub(prev_ticks);
            debug!(
                pane = %candidate.pane_id,
                window = %candidate.window_id,
                ai_pid,
                cpu_delta,
                single_delta,
                output_count,
                is_burst,
                prev_polls,
                new_polls,
                prev_bursts,
                new_bursts,
                is_active,
                "ai classify"
            );
        }

        prev_cpu.insert(
            candidate.pane_id.clone(),
            (ai_pid, current_cpu, new_polls, new_bursts),
        );

        if is_active {
            active_panes.insert(candidate.pane_id.clone());
            active_windows.insert(candidate.window_id.clone());
        }
    }

    prev_cpu.retain(|pane_id, _| seen_panes.contains(pane_id));
    output_counts.clear();

    (active_panes, active_windows)
}

fn find_ai_child_pid(pane_pid: u32) -> Option<u32> {
    let children_path = format!("/proc/{}/task/{}/children", pane_pid, pane_pid);
    let children = std::fs::read_to_string(&children_path).ok()?;
    for child_str in children.split_whitespace() {
        let child_pid: u32 = child_str.parse().ok()?;
        let comm_path = format!("/proc/{}/comm", child_pid);
        if let Ok(comm) = std::fs::read_to_string(&comm_path) {
            if is_ai_process_name(comm.trim()) {
                return Some(child_pid);
            }
        }
    }
    None
}

pub(super) fn read_cpu_ticks(pid: u32) -> Option<u64> {
    let path = format!("/proc/{}/stat", pid);
    let content = std::fs::read_to_string(&path).ok()?;
    let after_comm = content.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

pub(super) fn read_cpu_ticks_tree(pid: u32) -> Option<u64> {
    let root_cpu = read_cpu_ticks(pid)?;
    let mut total = root_cpu;

    let mut stack: Vec<(u32, u8)> = vec![(pid, 0)];
    while let Some((current_pid, depth)) = stack.pop() {
        if depth >= MAX_PROC_TREE_DEPTH {
            continue;
        }
        let task_dir = format!("/proc/{}/task", current_pid);
        let tasks = match std::fs::read_dir(&task_dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for task_entry in tasks {
            let tid = match task_entry {
                Ok(entry) => entry.file_name(),
                Err(_) => continue,
            };
            let children_path = format!(
                "/proc/{}/task/{}/children",
                current_pid,
                tid.to_string_lossy()
            );
            let children = match std::fs::read_to_string(&children_path) {
                Ok(children) => children,
                Err(_) => continue,
            };
            for child_str in children.split_whitespace() {
                if let Ok(child_pid) = child_str.parse::<u32>() {
                    if let Some(child_cpu) = read_cpu_ticks(child_pid) {
                        total += child_cpu;
                    }
                    stack.push((child_pid, depth + 1));
                }
            }
        }
    }
    Some(total)
}
