use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
use std::cmp::Reverse;

use crate::cmd::Cmd;
use crate::model::{Mode, Model, SearchMatch, SearchState};
use crate::tree::{expand_to_window_by_id, TreeNode};

use super::input::{clear_input, handle_input_key};

pub(super) fn enter_search_mode(model: &mut Model) -> Vec<Cmd> {
    let pre_id = model
        .sidebar
        .pending_preview_id
        .clone()
        .or_else(|| model.selected_window_info().map(|i| i.id.clone()));
    model.mode = Mode::Searching;
    clear_input(model);
    model.search_state = Some(SearchState {
        matches: Vec::new(),
        selection: 0,
        pre_search_window_id: pre_id,
    });
    recompute_matches(model);
    preview_search_selection(model)
}

pub(super) fn handle_searching_key(model: &mut Model, event: KeyEvent) -> Vec<Cmd> {
    if event.modifiers == KeyModifiers::CONTROL {
        match event.code {
            KeyCode::Char('n') => return move_selection(model, 1),
            KeyCode::Char('p') => return move_selection(model, -1),
            _ => return vec![],
        }
    }

    if event.modifiers != KeyModifiers::NONE && event.modifiers != KeyModifiers::SHIFT {
        return vec![];
    }

    match event.code {
        KeyCode::Down => move_selection(model, 1),
        KeyCode::Up => move_selection(model, -1),
        KeyCode::Enter => follow_selected(model),
        KeyCode::Esc => cancel_search(model),
        _ => {
            if handle_input_key(model, event.code).is_some() {
                recompute_matches(model);
                if let Some(state) = &mut model.search_state {
                    state.selection = 0;
                }
                preview_search_selection(model)
            } else {
                vec![]
            }
        }
    }
}

fn move_selection(model: &mut Model, delta: i32) -> Vec<Cmd> {
    let Some(state) = &mut model.search_state else {
        return vec![];
    };
    let len = state.matches.len();
    if len == 0 {
        return vec![Cmd::Render];
    }
    if delta > 0 {
        if state.selection + 1 < len {
            state.selection += 1;
        }
    } else if state.selection > 0 {
        state.selection -= 1;
    }
    preview_search_selection(model)
}

fn cancel_search(model: &mut Model) -> Vec<Cmd> {
    let pre_id = model
        .search_state
        .as_ref()
        .and_then(|s| s.pre_search_window_id.clone());

    model.mode = Mode::Normal;
    model.search_state = None;
    clear_input(model);

    if let Some(id) = pre_id {
        model.sidebar.pending_preview_id = Some(id.clone());
        vec![Cmd::PreviewWindow { id }, Cmd::Render]
    } else {
        vec![Cmd::Render]
    }
}

fn recompute_matches(model: &mut Model) {
    let candidates = collect_search_candidates(model.tree());
    let query = model.input_buffer.clone();

    let matches = if query.is_empty() {
        candidates
            .into_iter()
            .map(|(window_id, display)| SearchMatch {
                window_id,
                display,
                score: 0,
                indices: Vec::new(),
            })
            .collect()
    } else {
        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let pattern = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
        let mut scored: Vec<SearchMatch> = candidates
            .into_iter()
            .filter_map(|(window_id, display)| {
                let mut buf = Vec::new();
                let haystack = Utf32Str::new(&display, &mut buf);
                let mut indices = Vec::new();
                let score = pattern.indices(haystack, &mut matcher, &mut indices)?;
                Some(SearchMatch {
                    window_id,
                    display,
                    score,
                    indices,
                })
            })
            .collect();
        scored.sort_by_key(|m| Reverse(m.score));
        scored
    };

    let selection = model
        .search_state
        .as_ref()
        .map(|s| s.selection.min(matches.len().saturating_sub(1)))
        .unwrap_or(0);

    let pre_id = model
        .search_state
        .as_ref()
        .and_then(|s| s.pre_search_window_id.clone());

    model.search_state = Some(SearchState {
        matches,
        selection,
        pre_search_window_id: pre_id,
    });
}

fn collect_search_candidates(nodes: &[TreeNode]) -> Vec<(String, String)> {
    fn visit(nodes: &[TreeNode], prefix: Option<&str>, out: &mut Vec<(String, String)>) {
        for node in nodes {
            match node {
                TreeNode::Window { info } => {
                    let display = match prefix {
                        Some(p) => format!("{}:{}", p, info.name),
                        None => info.name.clone(),
                    };
                    out.push((info.id.clone(), display));
                }
                TreeNode::Folder { name, children, .. } => {
                    let next_prefix = match prefix {
                        Some(p) => format!("{}:{}", p, name),
                        None => name.clone(),
                    };
                    visit(children, Some(&next_prefix), out);
                }
            }
        }
    }
    let mut out = Vec::new();
    visit(nodes, None, &mut out);
    out
}

fn preview_search_selection(model: &mut Model) -> Vec<Cmd> {
    let id = model
        .search_state
        .as_ref()
        .and_then(|s| s.matches.get(s.selection))
        .map(|m| m.window_id.clone());

    if let Some(id) = id {
        model.sidebar.pending_preview_id = Some(id.clone());
        vec![Cmd::PreviewWindow { id }, Cmd::Render]
    } else {
        vec![Cmd::Render]
    }
}

fn follow_selected(model: &mut Model) -> Vec<Cmd> {
    let window_id = model
        .search_state
        .as_ref()
        .and_then(|s| s.matches.get(s.selection))
        .map(|m| m.window_id.clone());

    let Some(window_id) = window_id else {
        return vec![];
    };

    model.mode = Mode::Normal;
    model.search_state = None;
    clear_input(model);

    model.mutate_tree(|tree| {
        expand_to_window_by_id(tree, &window_id);
    });

    let Some(idx) = model.flat_index_for_window_id(&window_id) else {
        return vec![Cmd::Render];
    };
    model.set_cursor(idx);

    let mut cmds = super::navigation::handle_select_item(model);
    cmds.push(Cmd::Render);
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Mode, Model};
    use crate::msg::Msg;
    use crate::tree::WindowInfo;
    use crate::update::update;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn test_model() -> Model {
        Model::new(
            "s".to_string(),
            "$1".to_string(),
            "%sidebar".to_string(),
            "%home".to_string(),
            "@home".to_string(),
        )
    }

    fn wi(id: &str, index: usize, name: &str) -> WindowInfo {
        WindowInfo::new(id, index, name, false)
    }

    #[test]
    fn enter_search_populates_all_windows() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@1", 1, "scratch"),
                wi("@2", 2, "myapp:editor"),
                wi("@3", 3, "myapp:term"),
            ]),
        );

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        );

        assert!(matches!(model.mode, Mode::Searching));
        let state = model.search_state.as_ref().expect("search_state set");
        assert_eq!(state.matches.len(), 3);
    }

    #[test]
    fn typing_filters_matches() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@1", 1, "scratch"),
                wi("@2", 2, "myapp:editor"),
                wi("@3", 3, "myapp:term"),
            ]),
        );

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        );
        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)),
        );
        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
        );

        let state = model.search_state.as_ref().expect("search_state set");
        assert_eq!(state.matches.len(), 1);
        assert_eq!(state.matches[0].window_id, "@2");
    }

    #[test]
    fn selection_movement_clamps() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@1", 1, "alpha"),
                wi("@2", 2, "beta"),
            ]),
        );

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        );

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        {
            let state = model.search_state.as_ref().expect("search_state");
            assert_eq!(state.selection, 1);
        }

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        );
        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        );
        {
            let state = model.search_state.as_ref().expect("search_state");
            assert_eq!(state.selection, 0);
        }
    }

    #[test]
    fn enter_in_search_follows_window() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@1", 1, "alpha"),
                wi("@2", 2, "beta"),
            ]),
        );

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        );

        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert_eq!(model.mode, Mode::Normal);
        assert!(model.search_state.is_none());
        assert!(cmds.iter().any(|c| matches!(c, Cmd::PreviewWindow { .. })));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::FocusRightPane)));
    }

    #[test]
    fn typing_j_edits_query_not_selection() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@1", 1, "jobs"), wi("@2", 2, "alpha")]),
        );

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        );
        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
        );

        assert_eq!(model.input_buffer, "j");
        let state = model.search_state.as_ref().expect("search_state");
        assert_eq!(state.selection, 0);
        assert_eq!(state.matches.len(), 1);
        assert_eq!(state.matches[0].window_id, "@1");
    }

    #[test]
    fn esc_restores_normal_mode() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@1", 1, "dev")]),
        );

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        );
        assert!(matches!(model.mode, Mode::Searching));

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );

        assert_eq!(model.mode, Mode::Normal);
        assert!(model.search_state.is_none());
    }
}
