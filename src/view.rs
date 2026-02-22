use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use unicode_width::UnicodeWidthChar;

use std::collections::HashSet;

use crate::model::{Mode, Model};
use crate::tree::{get_node, FlatItem, FlatNodeKind, TreeNode};

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

fn render_tree_item(
    model: &Model,
    index: usize,
    item: &FlatItem,
    width: usize,
) -> ListItem<'static> {
    let indent = " ".repeat(item.depth * 2);
    let mut content = String::new();
    let mut style = Style::default();
    let mut show_ai_dot = false;

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
                content = format!("{}{} {}", indent, marker, name);
                style = style.add_modifier(Modifier::BOLD);
                if children.is_empty() {
                    style = style.fg(Color::DarkGray);
                }
                // Bubble up: show dot only when collapsed (expanded shows individual dots)
                show_ai_dot =
                    !*expanded && folder_has_ai_window(children, &model.ai_windows);
            }
            (FlatNodeKind::Window, TreeNode::Window { info }) => {
                let branch = window_branch(model.tree(), &item.path);
                if info.id == model.sidebar_window_id {
                    content = format!("{}{} * {}", indent, branch, info.name);
                    style = style.fg(Color::Yellow);
                } else {
                    content = format!("{}{} {}", indent, branch, info.name);
                }
                show_ai_dot = model.ai_windows.contains(&info.id);
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

    if show_ai_dot && width > 1 {
        let truncated = truncate(&content, width.saturating_sub(2));
        let text_width = truncated.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>();
        let padding = width.saturating_sub(text_width).saturating_sub(1);
        let line = Line::from(vec![
            Span::styled(truncated, style),
            Span::styled(" ".repeat(padding), style),
            Span::styled("●", style.fg(Color::Yellow)),
        ]);
        ListItem::new(line)
    } else {
        ListItem::new(truncate(&content, width)).style(style)
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

fn build_footer_text(model: &Model, width: usize) -> String {
    if let Some(err) = &model.error_message {
        return truncate(err, width);
    }
    if let Some(info) = &model.info_message {
        return truncate(info, width);
    }

    match &model.mode {
        Mode::Normal => truncate("[r]ename [x]close [c]new [C]proj [R]estart", width),
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
        Mode::ConfirmClose { .. } => truncate("Close window? [y/n]", width),
    }
}

/// Returns the branch character for a window node based on whether it's the last
/// child in its parent folder.
fn window_branch(tree: &[TreeNode], path: &[usize]) -> &'static str {
    if path.len() < 2 {
        // Top-level window (depth 0) has no parent folder
        return " ";
    }

    let parent_path = &path[..path.len() - 1];
    let child_idx = path[path.len() - 1];

    match get_node(tree, parent_path) {
        Ok(TreeNode::Folder { children, .. }) => {
            if child_idx + 1 == children.len() {
                "└"
            } else {
                "├"
            }
        }
        _ => "├",
    }
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
