use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::model::{Mode, Model};
use crate::tree::{get_node, FlatItem, FlatNodeKind, TreeNode};

pub fn render(model: &Model, frame: &mut Frame) {
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", model.session_name));
    let area = frame.area();
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let footer_height: u16 = if matches!(model.mode, Mode::Renaming { .. }) {
        2
    } else {
        1
    };

    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(footer_height),
    ])
    .split(inner);

    // Tree area
    let tree_items: Vec<ListItem> = model
        .flat_items
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

    match get_node(&model.tree, &item.path) {
        Ok(node) => match (&item.kind, node) {
            (FlatNodeKind::Folder, TreeNode::Folder { name, expanded, children }) => {
                let marker = if *expanded { "v" } else { ">" };
                let content = format!("{}{} {}", indent, marker, name);
                line.push_str(&truncate(&content, width));
                style = style.add_modifier(Modifier::BOLD);
                if children.is_empty() {
                    style = style.fg(Color::DarkGray);
                }
            }
            (FlatNodeKind::Window, TreeNode::Window { info }) => {
                let branch = window_branch(&model.tree, &item.path);
                if info.active {
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

    if index == model.cursor {
        style = style.add_modifier(Modifier::REVERSED);
    }

    ListItem::new(line).style(style)
}

fn build_footer_text(model: &Model, width: usize) -> String {
    if let Some(err) = &model.error_message {
        return truncate(err, width);
    }

    match &model.mode {
        Mode::Normal => truncate("[r]ename [x]close [c]new", width),
        Mode::Renaming { .. } => {
            let line1 = truncate(
                &format!("Rename: {}_", model.input_buffer),
                width,
            );
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

/// Truncate a string to at most `max_chars` Unicode scalar values.
fn truncate(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    input.chars().take(max_chars).collect()
}
