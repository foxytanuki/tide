use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::cmd::Cmd;
use crate::model::Model;

const RECENTLY_FINISHED_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub(super) fn handle_ai_process_poll_result(
    model: &mut Model,
    panes: HashSet<String>,
    windows: HashSet<String>,
) -> Vec<Cmd> {
    let now = Instant::now();
    let mut recently_changed = false;

    // Detect windows that just finished AI (were active, now gone).
    for old_win in &model.ai.windows {
        if !windows.contains(old_win) {
            model.ai.recently_finished.insert(old_win.clone(), now);
            recently_changed = true;
        }
    }

    for new_win in &windows {
        if model.ai.recently_finished.remove(new_win).is_some() {
            recently_changed = true;
        }
    }

    let before_len = model.ai.recently_finished.len();
    model
        .ai
        .recently_finished
        .retain(|_, finished_at| now.duration_since(*finished_at) < RECENTLY_FINISHED_TIMEOUT);
    if model.ai.recently_finished.len() != before_len {
        recently_changed = true;
    }

    let ai_changed = panes != model.ai.panes || windows != model.ai.windows;
    model.ai.panes = panes;
    model.ai.windows = windows;

    if ai_changed {
        vec![Cmd::CheckBorder, Cmd::Render]
    } else if recently_changed {
        vec![Cmd::Render]
    } else {
        vec![]
    }
}
