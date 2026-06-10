use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use unicode_width::UnicodeWidthChar;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::model::{Mode, Model};
use crate::tree::{FlatItem, TreeNode, WindowInfo};

pub fn render(model: &Model, frame: &mut Frame) {
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", model.session_name));
    let area = frame.area();
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let footer_height: u16 = match model.mode {
        Mode::Renaming { .. } | Mode::RenamingFolder { .. } | Mode::CreatingProject => 2,
        _ => 1,
    };

    let chunks =
        Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)]).split(inner);

    // Tree area
    let tree_items = build_tree_items(model, chunks[0].width as usize);

    frame.render_widget(List::new(tree_items), chunks[0]);

    // Footer area
    let footer_text = build_footer_text(model, chunks[1].width as usize);
    let footer_style = if model.error_message.is_some() {
        Style::default().fg(Color::Red)
    } else if model.info_message.is_some() {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let footer = Paragraph::new(footer_text).style(footer_style);
    frame.render_widget(footer, chunks[1]);

    // Show terminal block cursor in input modes
    if let Some(prefix_len) = input_prefix_len(&model.mode) {
        let display_width: u16 = model
            .input_buffer
            .chars()
            .take(model.input_cursor)
            .map(|c| c.width().unwrap_or(0) as u16)
            .sum();
        let cursor_x = chunks[1].x + prefix_len + display_width;
        let cursor_y = chunks[1].y;
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

pub fn build_tree_items(model: &Model, width: usize) -> Vec<ListItem<'static>> {
    let ctx = RenderTreeContext {
        flat_items: model.flat_items(),
        width,
        cursor: model.cursor(),
        active_window_id: model
            .sidebar
            .pending_preview_id
            .as_deref()
            .unwrap_or(&model.sidebar.window_id),
        ai_windows: &model.ai.windows,
        recently_finished: &model.ai.recently_finished,
    };
    let mut rendered = Vec::with_capacity(ctx.flat_items.len());
    let mut flat_index = 0;

    ctx.collect_tree_items(model.tree(), &mut flat_index, &mut rendered);

    debug_assert_eq!(flat_index, ctx.flat_items.len());
    rendered
}

/// Badge state for a tree item's AI activity indicator.
enum AiBadge {
    /// No badge
    None,
    /// ● Active AI (yellow)
    Active,
    /// ○ Recently finished AI (dark gray)
    Finished,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SubtreeAiState {
    has_active: bool,
    has_finished: bool,
}

impl SubtreeAiState {
    fn merge(&mut self, other: Self) {
        self.has_active |= other.has_active;
        self.has_finished |= other.has_finished;
    }

    fn badge_for_collapsed_folder(self) -> AiBadge {
        if self.has_active {
            AiBadge::Active
        } else if self.has_finished {
            AiBadge::Finished
        } else {
            AiBadge::None
        }
    }
}

struct RenderTreeContext<'a> {
    flat_items: &'a [FlatItem],
    width: usize,
    cursor: usize,
    active_window_id: &'a str,
    ai_windows: &'a HashSet<String>,
    recently_finished: &'a HashMap<String, Instant>,
}

impl RenderTreeContext<'_> {
    fn collect_tree_items(
        &self,
        nodes: &[TreeNode],
        flat_index: &mut usize,
        out: &mut Vec<ListItem<'static>>,
    ) -> SubtreeAiState {
        let mut subtree = SubtreeAiState::default();

        for node in nodes {
            subtree.merge(self.collect_tree_item(node, flat_index, out));
        }

        subtree
    }

    fn collect_tree_item(
        &self,
        node: &TreeNode,
        flat_index: &mut usize,
        out: &mut Vec<ListItem<'static>>,
    ) -> SubtreeAiState {
        match node {
            TreeNode::Folder {
                name,
                expanded,
                children,
            } => {
                let Some(item) = self.flat_items.get(*flat_index) else {
                    debug_assert!(false, "flat/tree mismatch on folder row");
                    return subtree_ai_state(children, self.ai_windows, self.recently_finished);
                };
                let is_selected = *flat_index == self.cursor;
                *flat_index += 1;

                let rendered_index = out.len();
                out.push(ListItem::new(String::new()));

                let child_state = if *expanded {
                    self.collect_tree_items(children, flat_index, out)
                } else {
                    subtree_ai_state(children, self.ai_windows, self.recently_finished)
                };

                out[rendered_index] = render_folder_item(
                    item,
                    name,
                    *expanded,
                    children.is_empty(),
                    self.width,
                    is_selected,
                    if *expanded {
                        AiBadge::None
                    } else {
                        child_state.badge_for_collapsed_folder()
                    },
                );

                child_state
            }
            TreeNode::Window { info } => {
                let Some(item) = self.flat_items.get(*flat_index) else {
                    debug_assert!(false, "flat/tree mismatch on window row");
                    return window_ai_state(
                        info.id.as_str(),
                        self.ai_windows,
                        self.recently_finished,
                    );
                };
                let is_selected = *flat_index == self.cursor;
                *flat_index += 1;
                let ai_state =
                    window_ai_state(info.id.as_str(), self.ai_windows, self.recently_finished);
                let badge = ai_state.badge_for_collapsed_folder();
                out.push(render_window_item(
                    item,
                    info,
                    self.active_window_id,
                    self.width,
                    is_selected,
                    badge,
                ));
                ai_state
            }
        }
    }
}

fn subtree_ai_state(
    children: &[TreeNode],
    ai_windows: &HashSet<String>,
    recently_finished: &HashMap<String, Instant>,
) -> SubtreeAiState {
    let mut state = SubtreeAiState::default();

    for child in children {
        match child {
            TreeNode::Window { info } => {
                state.merge(window_ai_state(
                    info.id.as_str(),
                    ai_windows,
                    recently_finished,
                ));
            }
            TreeNode::Folder { children, .. } => {
                state.merge(subtree_ai_state(children, ai_windows, recently_finished));
            }
        }
    }

    state
}

fn window_ai_state(
    window_id: &str,
    ai_windows: &HashSet<String>,
    recently_finished: &HashMap<String, Instant>,
) -> SubtreeAiState {
    SubtreeAiState {
        has_active: ai_windows.contains(window_id),
        has_finished: recently_finished.contains_key(window_id),
    }
}

fn render_folder_item(
    item: &FlatItem,
    name: &str,
    expanded: bool,
    is_empty: bool,
    width: usize,
    selected: bool,
    badge: AiBadge,
) -> ListItem<'static> {
    let indent = " ".repeat(item.depth * 2);
    let marker = if expanded { "v" } else { ">" };
    let content = format!("{}{} {}", visible_prefix(item, &indent), marker, name);
    let mut style = Style::default();
    style = style.add_modifier(Modifier::BOLD);
    if is_empty {
        style = style.fg(Color::DarkGray);
    }
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }

    render_line_with_badge(&content, style, width, badge)
}

fn render_window_item(
    item: &FlatItem,
    info: &WindowInfo,
    active_window_id: &str,
    width: usize,
    selected: bool,
    badge: AiBadge,
) -> ListItem<'static> {
    let indent = " ".repeat(item.depth * 2);
    let prefix = window_prefix(item, &indent);
    let mut style = Style::default();
    let content = if info.id == active_window_id {
        style = style.fg(Color::Yellow);
        format!("{prefix}* {}", info.name)
    } else {
        format!("{prefix}{}", info.name)
    };
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }

    render_line_with_badge(&content, style, width, badge)
}

fn render_line_with_badge(
    content: &str,
    style: Style,
    width: usize,
    badge: AiBadge,
) -> ListItem<'static> {
    let truncated = truncate(content, width);

    match badge {
        AiBadge::Active if width > 1 => {
            let truncated = truncate(content, width.saturating_sub(2));
            let text_width = display_width(&truncated);
            let padding = width.saturating_sub(text_width).saturating_sub(1);
            let line = Line::from(vec![
                Span::styled(truncated, style),
                Span::styled(" ".repeat(padding), style),
                Span::styled("●", style.fg(Color::Yellow)),
            ]);
            ListItem::new(line)
        }
        AiBadge::Finished if width > 1 => {
            let truncated = truncate(content, width.saturating_sub(2));
            let text_width = display_width(&truncated);
            let padding = width.saturating_sub(text_width).saturating_sub(1);
            let line = Line::from(vec![
                Span::styled(truncated, style),
                Span::styled(" ".repeat(padding), style),
                Span::styled("○", style.fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        }
        _ => ListItem::new(truncated).style(style),
    }
}

fn visible_prefix(item: &FlatItem, indent: &str) -> String {
    match item.visible_number {
        Some(n) => format!("{indent}{n}:"),
        None => indent.to_string(),
    }
}

fn window_prefix(item: &FlatItem, indent: &str) -> String {
    let connector = window_connector(item.is_last_sibling);

    match item.visible_number {
        Some(n) => format!("{indent}{connector} {n}: "),
        None => format!("{indent}{connector} "),
    }
}

fn window_connector(is_last_sibling: bool) -> &'static str {
    if is_last_sibling {
        "└─"
    } else {
        "├─"
    }
}

fn build_footer_text(model: &Model, width: usize) -> String {
    if let Some(err) = &model.error_message {
        return truncate(err, width);
    }
    if let Some(info) = &model.info_message {
        return truncate(info, width);
    }

    match &model.mode {
        Mode::Normal => build_normal_footer_text(width),
        Mode::Renaming { .. } | Mode::RenamingFolder { .. } => {
            let line1 = truncate(&format!("Rename: {}", model.input_buffer), width);
            let line2 = truncate("[enter] ok [esc] cancel", width);
            format!("{}\n{}", line1, line2)
        }
        Mode::CreatingProject => {
            let line1 = truncate(&format!("Project: {}", model.input_buffer), width);
            let line2 = truncate("[enter] ok [esc] cancel", width);
            format!("{}\n{}", line1, line2)
        }
        Mode::Moving { .. } => truncate("Move: [↑/↓] place [enter] ok [esc] cancel", width),
        Mode::ConfirmClose { .. } => truncate("Close window? [y/n]", width),
    }
}

fn build_normal_footer_text(width: usize) -> String {
    let actions = [
        "[r]ename",
        "[x]close",
        "[c]new",
        "[m]ove",
        "[M]proj",
        "[L]ayout",
        "[C]proj",
        "[R]estart",
    ];
    fit_footer_actions(&actions, width)
}

fn fit_footer_actions(actions: &[&str], width: usize) -> String {
    if width == 0 || actions.is_empty() {
        return String::new();
    }

    let mut shown = Vec::new();
    let mut used = 0;

    for action in actions {
        let sep = usize::from(!shown.is_empty());
        let action_width = display_width(action);
        if used + sep + action_width > width {
            break;
        }
        if sep == 1 {
            used += 1;
        }
        shown.push(*action);
        used += action_width;
    }

    if shown.is_empty() {
        return truncate(actions[0], width);
    }

    let mut result = shown.join(" ");
    if shown.len() < actions.len() && display_width(&result) + 2 <= width {
        result.push_str(" …");
    }
    result
}

/// Returns the prefix length before the input text for cursor positioning.
fn input_prefix_len(mode: &Mode) -> Option<u16> {
    match mode {
        Mode::Renaming { .. } | Mode::RenamingFolder { .. } => Some("Rename: ".len() as u16),
        Mode::CreatingProject => Some("Project: ".len() as u16),
        _ => None,
    }
}

/// Truncate a string to fit within `max_cols` terminal display columns.
fn truncate(input: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let mut cols = 0;
    let mut result = String::new();
    for ch in input.chars() {
        let w = ch.width().unwrap_or(0);
        if cols + w > max_cols {
            break;
        }
        result.push(ch);
        cols += w;
    }
    result
}

fn display_width(input: &str) -> usize {
    input.chars().map(|ch| ch.width().unwrap_or(0)).sum()
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::time::Instant;

    use super::{
        build_normal_footer_text, build_tree_items, fit_footer_actions, subtree_ai_state,
        window_connector,
    };
    use crate::model::Model;
    use crate::tree::{build_tree, get_node, toggle_expand, TreeNode, WindowInfo};

    fn test_model() -> Model {
        Model::new(
            "s".to_string(),
            "$1".to_string(),
            "%sidebar".to_string(),
            "%home".to_string(),
            "@home".to_string(),
        )
    }

    fn window(id: &str, index: usize, name: &str) -> WindowInfo {
        WindowInfo {
            id: id.to_string(),
            index,
            name: name.to_string(),
            active: false,
        }
    }

    #[test]
    fn window_connector_uses_box_drawing_glyphs() {
        assert_eq!(window_connector(false), "├─");
        assert_eq!(window_connector(true), "└─");
    }

    #[test]
    fn normal_footer_prioritizes_primary_actions_on_narrow_width() {
        assert_eq!(build_normal_footer_text(25), "[r]ename [x]close [c]new");
    }

    #[test]
    fn normal_footer_shows_overflow_marker_when_it_fits() {
        assert_eq!(
            fit_footer_actions(&["[r]ename", "[x]close", "[c]new"], 20),
            "[r]ename [x]close …"
        );
    }

    #[test]
    fn normal_footer_includes_layout_action() {
        assert!(build_normal_footer_text(64).contains("[L]ayout"));
    }

    #[test]
    fn normal_footer_shows_all_actions_when_width_allows() {
        assert_eq!(
            build_normal_footer_text(96),
            "[r]ename [x]close [c]new [m]ove [M]proj [L]ayout [C]proj [R]estart"
        );
    }

    #[test]
    fn subtree_ai_state_bubbles_nested_window_activity() {
        let tree = build_tree(&[
            window("@1", 1, "proj:api:tab1"),
            window("@2", 2, "proj:api:tab2"),
            window("@3", 3, "proj:web"),
        ]);
        let mut ai_windows = HashSet::new();
        ai_windows.insert("@2".to_string());
        let mut recently_finished = HashMap::new();
        recently_finished.insert("@3".to_string(), Instant::now());

        let state = subtree_ai_state(&tree, &ai_windows, &recently_finished);

        assert!(state.has_active);
        assert!(state.has_finished);
    }

    #[test]
    fn build_tree_items_matches_visible_flat_rows() {
        let mut model = test_model();
        let mut tree = build_tree(&[
            window("@1", 1, "proj:api:tab1"),
            window("@2", 2, "proj:api:tab2"),
            window("@3", 3, "scratch"),
        ]);
        let node = get_node(&tree, &[0, 0]).expect("nested folder exists");
        assert!(matches!(node, TreeNode::Folder { .. }));
        let folder = crate::tree::get_node_mut(&mut tree, &[0, 0]).expect("nested folder exists");
        toggle_expand(folder);
        model.replace_tree_preserve_selection(tree, None);

        let items = build_tree_items(&model, 25);

        assert_eq!(items.len(), model.flat_items().len());
    }

    #[test]
    fn collapsed_folder_badge_moves_to_parent_only_when_collapsed() {
        let mut model = test_model();
        let expanded_tree =
            build_tree(&[window("@1", 1, "proj:tab1"), window("@2", 2, "proj:tab2")]);
        model.ai.windows.insert("@2".to_string());

        let mut tree = expanded_tree.clone();
        let folder = crate::tree::get_node_mut(&mut tree, &[0]).expect("folder exists");
        toggle_expand(folder);
        model.replace_tree_preserve_selection(tree, None);

        let collapsed = build_tree_items(&model, 25);
        assert!(format!("{:?}", collapsed[0]).contains('●'));

        model.replace_tree_preserve_selection(expanded_tree, None);
        let expanded = build_tree_items(&model, 25);
        assert!(!format!("{:?}", expanded[0]).contains('●'));
        assert!(expanded
            .iter()
            .skip(1)
            .any(|item| format!("{:?}", item).contains('●')));
    }
}
