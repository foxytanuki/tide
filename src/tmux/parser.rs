#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxEvent {
    WindowAdd(String),
    WindowClose(String),
    WindowRenamed(String, String),
    SessionChanged(String, String),
    SessionWindowChanged(String, String), // session_id, window_id
    PaneOutput(String),                   // pane_id (data discarded)
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlMarker {
    Begin,
    End,
    ErrorEnd,
}

pub(crate) fn parse_control_marker(line: &str) -> Option<ControlMarker> {
    if line.starts_with("%begin ") {
        return Some(ControlMarker::Begin);
    }
    if line.starts_with("%end ") {
        return Some(ControlMarker::End);
    }
    if line.starts_with("%error ") {
        return Some(ControlMarker::ErrorEnd);
    }
    None
}

pub fn parse_line(line: &str) -> Option<TmuxEvent> {
    if !line.starts_with('%') {
        return None;
    }

    if parse_control_marker(line).is_some() {
        return None;
    }

    if let Some(rest) = line.strip_prefix("%window-add ") {
        let id = rest.split_whitespace().next()?.to_string();
        return Some(TmuxEvent::WindowAdd(id));
    }

    if let Some(rest) = line.strip_prefix("%window-close ") {
        let id = rest.split_whitespace().next()?.to_string();
        return Some(TmuxEvent::WindowClose(id));
    }

    if let Some(rest) = line.strip_prefix("%window-renamed ") {
        let (id, name) = match rest.split_once(' ') {
            Some((id, name)) => (id.to_string(), name.trim().to_string()),
            None => (rest.trim().to_string(), String::new()),
        };
        return Some(TmuxEvent::WindowRenamed(id, name));
    }

    if let Some(rest) = line.strip_prefix("%session-window-changed ") {
        let (session_id, window_id) = match rest.split_once(' ') {
            Some((sid, wid)) => (sid.to_string(), wid.trim().to_string()),
            None => return None,
        };
        return Some(TmuxEvent::SessionWindowChanged(session_id, window_id));
    }

    if let Some(rest) = line.strip_prefix("%session-changed ") {
        let (id, name) = match rest.split_once(' ') {
            Some((id, name)) => (id.to_string(), name.trim().to_string()),
            None => (rest.trim().to_string(), String::new()),
        };
        return Some(TmuxEvent::SessionChanged(id, name));
    }

    // %output %<pane_id> <data> — pane produced output (data discarded)
    if let Some(rest) = line.strip_prefix("%output ") {
        let pane_id = rest.split_whitespace().next()?.to_string();
        return Some(TmuxEvent::PaneOutput(pane_id));
    }

    // %extended-output %<pane_id> <age> ... : <data> — same as %output but
    // emitted when pause-after is configured. We only need the pane_id.
    if let Some(rest) = line.strip_prefix("%extended-output ") {
        let pane_id = rest.split_whitespace().next()?.to_string();
        return Some(TmuxEvent::PaneOutput(pane_id));
    }

    // Unknown % notifications are silently ignored
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_window_add() {
        assert_eq!(
            parse_line("%window-add @3"),
            Some(TmuxEvent::WindowAdd("@3".to_string()))
        );
    }

    #[test]
    fn parse_window_close() {
        assert_eq!(
            parse_line("%window-close @5"),
            Some(TmuxEvent::WindowClose("@5".to_string()))
        );
    }

    #[test]
    fn parse_window_renamed() {
        assert_eq!(
            parse_line("%window-renamed @2 my-window"),
            Some(TmuxEvent::WindowRenamed(
                "@2".to_string(),
                "my-window".to_string()
            ))
        );
    }

    #[test]
    fn parse_session_changed() {
        assert_eq!(
            parse_line("%session-changed $1 main"),
            Some(TmuxEvent::SessionChanged(
                "$1".to_string(),
                "main".to_string()
            ))
        );
    }

    #[test]
    fn parse_begin_end_error_markers() {
        assert_eq!(
            parse_control_marker("%begin 1234567890 42 0"),
            Some(ControlMarker::Begin)
        );
        assert_eq!(
            parse_control_marker("%end 1234567890 42 0"),
            Some(ControlMarker::End)
        );
        assert_eq!(
            parse_control_marker("%error 1234567890 42 1"),
            Some(ControlMarker::ErrorEnd)
        );
    }

    #[test]
    fn markers_not_parsed_as_events() {
        assert_eq!(parse_line("%begin 1234567890 42 0"), None);
        assert_eq!(parse_line("%end 1234567890 42 0"), None);
        assert_eq!(parse_line("%error 1234567890 42 1"), None);
    }

    #[test]
    fn non_control_lines_ignored() {
        assert_eq!(parse_line("some random output"), None);
    }

    #[test]
    fn unknown_percent_lines_ignored() {
        assert_eq!(parse_line("%layout-change some data"), None);
        assert_eq!(parse_line("%pane-mode-changed @1"), None);
    }

    #[test]
    fn parse_pane_output() {
        assert_eq!(
            parse_line("%output %5 some escaped output data"),
            Some(TmuxEvent::PaneOutput("%5".to_string()))
        );
    }

    #[test]
    fn parse_pane_output_no_data() {
        // Edge case: %output with pane_id but no data
        assert_eq!(
            parse_line("%output %0"),
            Some(TmuxEvent::PaneOutput("%0".to_string()))
        );
    }

    #[test]
    fn parse_extended_output() {
        assert_eq!(
            parse_line("%extended-output %3 500 : some data here"),
            Some(TmuxEvent::PaneOutput("%3".to_string()))
        );
    }
}
