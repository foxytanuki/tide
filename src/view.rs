use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use unicode_width::UnicodeWidthChar;

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
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let footer = Paragraph::new(footer_text).style(footer_style);
    frame.render_widget(footer, chunks[1]);

    // Show terminal block cursor in input modes
    if let Some(prefix_len) = input_prefix_len(&model.mode) {
        let cursor_x = chunks[1].x + prefix_len + model.input_cursor as u16;
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
    let mut line = String::new();
    let mut style = Style::default();

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
                let content = format!("{}{} {}", indent, marker, name);
                line.push_str(&truncate(&content, width));
                style = style.add_modifier(Modifier::BOLD);
                if children.is_empty() {
                    style = style.fg(Color::DarkGray);
                }
            }
            (FlatNodeKind::Window, TreeNode::Window { info }) => {
                let branch = window_branch(model.tree(), &item.path);
                if info.id == model.sidebar_window_id {
                    let content = format!("{}{} * {}", indent, branch, info.name);
                    line.push_str(&truncate(&content, width));
                    style = style.fg(Color::Yellow);
                } else {
                    let content = format!("{}{} {}", indent, branch, info.name);
                    line.push_str(&truncate(&content, width));
                }
            }
            _ => {
                line.push_str(&indent);
                line.push('?');
            }
        },
        Err(_) => {
            line.push('?');
        }
    }

    if index == model.cursor() {
        style = style.add_modifier(Modifier::REVERSED);
    }

    ListItem::new(line).style(style)
}

fn build_footer_text(model: &Model, width: usize) -> String {
    if let Some(err) = &model.error_message {
        return truncate(err, width);
    }

    match &model.mode {
        Mode::Normal => truncate("[r]ename [x]close [c]new [C]project", width),
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
