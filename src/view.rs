use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use unicode_width::UnicodeWidthChar;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::model::{Mode, Model};
use crate::tree::{get_node, visible_item_number, FlatItem, FlatNodeKind, TreeNode};

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
    let tree_items: Vec<ListItem> = model
        .flat_items()
        .iter()
        .enumerate()
        .map(|(idx, item)| render_tree_item(model, idx, item, chunks[0].width as usize))
        .collect();

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

/// Badge state for a tree item's AI activity indicator.
enum AiBadge {
    /// No badge
    None,
    /// ● Active AI (yellow)
    Active,
    /// ○ Recently finished AI (dark gray)
    Finished,
}

fn render_tree_item(
    model: &Model,
    index: usize,
    item: &FlatItem,
    width: usize,
) -> ListItem<'static> {
    let indent = " ".repeat(item.depth * 2);
    let mut content = String::new();
    let mut style = Style::default();
    let mut badge = AiBadge::None;

    match get_node(model.tree(), &item.path) {
        Ok(node) => match (&item.kind, node) {
            (
                FlatNodeKind::Folder,
                TreeNode::Folder {
                    name,
                    expanded,
                    children,
                },
            ) => {
                let marker = if *expanded { "v" } else { ">" };
                let prefix = visible_prefix(model, index, &indent);
                content = format!("{}{} {}", prefix, marker, name);
                style = style.add_modifier(Modifier::BOLD);
                if children.is_empty() {
                    style = style.fg(Color::DarkGray);
                }
                // Bubble up: show dot only when collapsed (expanded shows individual dots)
                // Active takes priority over recently-finished
                if !*expanded {
                    if folder_has_ai_window(children, &model.ai.windows) {
                        badge = AiBadge::Active;
                    } else if folder_has_recently_finished(children, &model.ai.recently_finished) {
                        badge = AiBadge::Finished;
                    }
                }
            }
            (FlatNodeKind::Window, TreeNode::Window { info }) => {
                let active_id = model
                    .sidebar
                    .pending_preview_id
                    .as_deref()
                    .unwrap_or(&model.sidebar.window_id);
                let prefix = window_prefix(model, index, item, &indent);
                if info.id == active_id {
                    content = format!("{prefix}* {}", info.name);
                    style = style.fg(Color::Yellow);
                } else {
                    content = format!("{prefix}{}", info.name);
                }
                if model.ai.windows.contains(&info.id) {
                    badge = AiBadge::Active;
                } else if model.ai.recently_finished.contains_key(&info.id) {
                    badge = AiBadge::Finished;
                }
            }
            _ => {
                content = format!("{}?", indent);
            }
        },
        Err(_) => {
            content.push('?');
        }
    }

    if index == model.cursor() {
        style = style.add_modifier(Modifier::REVERSED);
    }

    match badge {
        AiBadge::Active if width > 1 => {
            let truncated = truncate(&content, width.saturating_sub(2));
            let text_width = truncated
                .chars()
                .map(|c| c.width().unwrap_or(0))
                .sum::<usize>();
            let padding = width.saturating_sub(text_width).saturating_sub(1);
            let line = Line::from(vec![
                Span::styled(truncated, style),
                Span::styled(" ".repeat(padding), style),
                Span::styled("●", style.fg(Color::Yellow)),
            ]);
            ListItem::new(line)
        }
        AiBadge::Finished if width > 1 => {
            let truncated = truncate(&content, width.saturating_sub(2));
            let text_width = truncated
                .chars()
                .map(|c| c.width().unwrap_or(0))
                .sum::<usize>();
            let padding = width.saturating_sub(text_width).saturating_sub(1);
            let line = Line::from(vec![
                Span::styled(truncated, style),
                Span::styled(" ".repeat(padding), style),
                Span::styled("○", style.fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        }
        _ => ListItem::new(truncate(&content, width)).style(style),
    }
}

fn visible_prefix(model: &Model, index: usize, indent: &str) -> String {
    match visible_item_number(model.flat_items(), index) {
        Some(n) => format!("{indent}{n}:"),
        None => indent.to_string(),
    }
}

fn window_prefix(model: &Model, index: usize, item: &FlatItem, indent: &str) -> String {
    let connector = window_connector(item.is_last_sibling);

    match visible_item_number(model.flat_items(), index) {
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

/// Check if any window inside a folder (recursively) has AI activity.
fn folder_has_ai_window(children: &[TreeNode], ai_windows: &HashSet<String>) -> bool {
    for child in children {
        match child {
            TreeNode::Window { info } => {
                if ai_windows.contains(&info.id) {
                    return true;
                }
            }
            TreeNode::Folder { children, .. } => {
                if folder_has_ai_window(children, ai_windows) {
                    return true;
                }
            }
        }
    }
    false
}

/// Check if any window inside a folder (recursively) has recently finished AI.
fn folder_has_recently_finished(
    children: &[TreeNode],
    recently_finished: &HashMap<String, Instant>,
) -> bool {
    for child in children {
        match child {
            TreeNode::Window { info } => {
                if recently_finished.contains_key(&info.id) {
                    return true;
                }
            }
            TreeNode::Folder { children, .. } => {
                if folder_has_recently_finished(children, recently_finished) {
                    return true;
                }
            }
        }
    }
    false
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

/// Hit-test: map a mouse row to a flat_items index in the tree area.
pub fn tree_item_at(model: &Model, row: u16) -> Option<usize> {
    let (w, h) = model.terminal_size;
    let area = Rect::new(0, 0, w, h);
    let outer = Block::default().borders(Borders::ALL);
    let inner = outer.inner(area);

    let footer_height: u16 = match model.mode {
        Mode::Renaming { .. } | Mode::RenamingFolder { .. } | Mode::CreatingProject => 2,
        _ => 1,
    };
    let chunks =
        Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)]).split(inner);
    let tree_rect = chunks[0];

    if row < tree_rect.y || row >= tree_rect.y + tree_rect.height {
        return None;
    }
    let index = (row - tree_rect.y) as usize;
    if index < model.flat_items().len() {
        Some(index)
    } else {
        None
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
    use super::{build_normal_footer_text, fit_footer_actions, window_connector};

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
}
