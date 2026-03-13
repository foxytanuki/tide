use super::quote_tmux;

pub fn select_pane(pane_id: &str) -> String {
    format!("select-pane -t {pane_id}")
}

pub fn select_right_pane() -> &'static str {
    "select-pane -R"
}

pub fn pane_width_query(pane_id: &str) -> String {
    format!("display-message -t {pane_id} -p '#{{pane_width}}'")
}

pub fn resize_pane_width(pane_id: &str, width: u16) -> String {
    format!("resize-pane -t {pane_id} -x {width}")
}

pub fn new_window(name: &str) -> String {
    format!(
        "new-window -d -n {} -P -F '#{{window_id}}'",
        quote_tmux(name)
    )
}

pub fn rename_window(window_id: &str, name: &str) -> String {
    format!("rename-window -t {window_id} {}", quote_tmux(name))
}

pub fn kill_window(window_id: &str) -> String {
    format!("kill-window -t {window_id}")
}

pub fn disable_window_rename(window_id: &str) -> String {
    format!(
        "set-window-option -t {} automatic-rename off ; set-window-option -t {} allow-rename off",
        window_id, window_id
    )
}
