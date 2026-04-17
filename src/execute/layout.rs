use std::collections::{HashMap, HashSet, VecDeque};

use tracing::{debug, info, warn};

use super::{TmuxApi, LAYOUT_CHANGE_SUPPRESSION_MS, SIDEBAR_WIDTH_CHARS};
use crate::cmd::Cmd;
use crate::model::{Model, PreviewState};
use crate::tmux::commands;
use crate::tmux::{quote_tmux, WindowInfo};

async fn query_window_layout<T: TmuxApi>(tmux: &mut T, window_id: &str) -> Option<String> {
    match tmux
        .send_command(&format!(
            "display-message -t {} -p '#{{window_layout}}'",
            window_id
        ))
        .await
    {
        Ok(output) => {
            let layout = output.trim().to_string();
            if layout.is_empty() {
                None
            } else {
                Some(layout)
            }
        }
        Err(_) => None,
    }
}

async fn query_pane_current_path<T: TmuxApi>(tmux: &mut T, pane_id: &str) -> Option<String> {
    if pane_id.is_empty() {
        return None;
    }

    match tmux
        .send_command(&format!(
            "display-message -p -t {} '#{{pane_current_path}}'",
            pane_id
        ))
        .await
    {
        Ok(output) => {
            let path = output.trim().to_string();
            if path.is_empty() {
                None
            } else {
                Some(path)
            }
        }
        Err(_) => None,
    }
}

pub(super) async fn resolve_new_window_cwd<T: TmuxApi>(
    model: &Model,
    tmux: &mut T,
) -> Option<String> {
    let home_pane_id = model.sidebar.home_pane_id.as_str();
    if home_pane_id != model.sidebar.pane_id && !home_pane_id.is_empty() {
        if let Some(path) = query_pane_current_path(tmux, home_pane_id).await {
            return Some(path);
        }
    }

    let fallback_pane_id =
        choose_home_pane_in_window(tmux, &model.sidebar.window_id, &model.sidebar.pane_id).await;
    if fallback_pane_id.is_empty()
        || fallback_pane_id == model.sidebar.pane_id
        || fallback_pane_id == home_pane_id
    {
        return None;
    }

    query_pane_current_path(tmux, &fallback_pane_id).await
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct WindowPaneTargets {
    pub leftmost: String,
    pub home: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct WindowHomeTarget {
    first_non_sidebar: String,
    active: String,
}

pub(super) async fn query_window_pane_targets<T: TmuxApi>(
    tmux: &mut T,
    window_id: &str,
    sidebar_pane_id: &str,
) -> WindowPaneTargets {
    let pane_list = tmux
        .send_command(&format!(
            "list-panes -t {} -F '#{{pane_id}}\t#{{pane_left}}\t#{{pane_top}}\t#{{pane_active}}'",
            window_id
        ))
        .await
        .unwrap_or_default();

    let mut first_non_sidebar = String::new();
    let mut leftmost: Option<(u16, u16, String)> = None;
    let mut active = String::new();

    for line in pane_list.lines() {
        let mut parts = line.split('\t');
        let pane_id = parts.next().unwrap_or("").trim();
        let pane_left: u16 = match parts.next().unwrap_or("").trim().parse() {
            Ok(value) => value,
            Err(_) => continue,
        };
        let pane_top: u16 = match parts.next().unwrap_or("").trim().parse() {
            Ok(value) => value,
            Err(_) => continue,
        };
        let is_active = parts.next().unwrap_or("").trim() == "1";
        if pane_id.is_empty() || pane_id == sidebar_pane_id {
            continue;
        }
        if first_non_sidebar.is_empty() {
            first_non_sidebar = pane_id.to_string();
        }
        if is_active {
            active = pane_id.to_string();
        }

        match &leftmost {
            Some((best_left, best_top, _))
                if pane_left > *best_left || (pane_left == *best_left && pane_top >= *best_top) => {
            }
            _ => leftmost = Some((pane_left, pane_top, pane_id.to_string())),
        }
    }

    WindowPaneTargets {
        leftmost: leftmost.map(|(_, _, pane_id)| pane_id).unwrap_or_default(),
        home: if active.is_empty() {
            first_non_sidebar
        } else {
            active
        },
    }
}

async fn query_sidebar_window_and_home<T: TmuxApi>(
    tmux: &mut T,
    session_name: &str,
    sidebar_pane_id: &str,
) -> Option<(String, String)> {
    let pane_list = tmux
        .send_command(&format!(
            "list-panes -s -t {} -F '#{{window_id}}\t#{{pane_id}}\t#{{pane_active}}'",
            session_name
        ))
        .await
        .ok()?;

    let mut sidebar_window_id = String::new();
    let mut homes_by_window: HashMap<String, WindowHomeTarget> = HashMap::new();

    for line in pane_list.lines() {
        let mut parts = line.split('\t');
        let window_id = parts.next().unwrap_or("").trim();
        let pane_id = parts.next().unwrap_or("").trim();
        let is_active = parts.next().unwrap_or("").trim() == "1";
        if window_id.is_empty() || pane_id.is_empty() {
            continue;
        }
        if pane_id == sidebar_pane_id {
            sidebar_window_id = window_id.to_string();
            continue;
        }

        let entry = homes_by_window.entry(window_id.to_string()).or_default();
        if entry.first_non_sidebar.is_empty() {
            entry.first_non_sidebar = pane_id.to_string();
        }
        if is_active {
            entry.active = pane_id.to_string();
        }
    }

    if sidebar_window_id.is_empty() {
        return None;
    }

    let targets = homes_by_window
        .remove(&sidebar_window_id)
        .unwrap_or_default();
    let home_pane_id = if targets.active.is_empty() {
        targets.first_non_sidebar
    } else {
        targets.active
    };

    Some((sidebar_window_id, home_pane_id))
}

pub(super) async fn save_window_layout<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    window_id: &str,
) {
    if let Some(layout) = query_window_layout(tmux, window_id).await {
        let (term_width, _) = model.terminal_size;
        model
            .sidebar
            .pane_layouts
            .insert(window_id.to_string(), (term_width, layout));
    }
}

pub(super) async fn save_window_layout_without_pane<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    window_id: &str,
    pane_id: &str,
) {
    let Some(layout) = query_window_layout(tmux, window_id).await else {
        return;
    };
    let Some(layout) = layout_without_pane(&layout, pane_id) else {
        return;
    };

    let (term_width, _) = model.terminal_size;
    model
        .sidebar
        .pane_layouts
        .insert(window_id.to_string(), (term_width, layout));
}

pub(super) fn cleanup_helper_managed_windows(model: &mut Model, windows: &[WindowInfo]) {
    let live: HashSet<&str> = windows.iter().map(|window| window.id.as_str()).collect();
    model
        .sidebar
        .helper_managed_windows
        .retain(|id| live.contains(id.as_str()));
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LayoutKind {
    LeftRight,
    TopBottom,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LayoutNode {
    Pane {
        sx: u32,
        sy: u32,
        xoff: u32,
        yoff: u32,
        pane_id: u32,
    },
    Split {
        sx: u32,
        sy: u32,
        xoff: u32,
        yoff: u32,
        kind: LayoutKind,
        children: Vec<LayoutNode>,
    },
}

impl LayoutNode {
    pub(super) fn sx(&self) -> u32 {
        match self {
            Self::Pane { sx, .. } | Self::Split { sx, .. } => *sx,
        }
    }

    pub(super) fn sy(&self) -> u32 {
        match self {
            Self::Pane { sy, .. } | Self::Split { sy, .. } => *sy,
        }
    }

    fn set_geometry(&mut self, sx: u32, sy: u32, xoff: u32, yoff: u32) {
        match self {
            Self::Pane {
                sx: node_sx,
                sy: node_sy,
                xoff: node_xoff,
                yoff: node_yoff,
                ..
            }
            | Self::Split {
                sx: node_sx,
                sy: node_sy,
                xoff: node_xoff,
                yoff: node_yoff,
                ..
            } => {
                *node_sx = sx;
                *node_sy = sy;
                *node_xoff = xoff;
                *node_yoff = yoff;
            }
        }
    }

    fn write_body(&self, out: &mut String) {
        match self {
            Self::Pane {
                sx,
                sy,
                xoff,
                yoff,
                pane_id,
            } => {
                out.push_str(&format!("{sx}x{sy},{xoff},{yoff},{pane_id}"));
            }
            Self::Split {
                sx,
                sy,
                xoff,
                yoff,
                kind,
                children,
            } => {
                let (open, close) = match kind {
                    LayoutKind::LeftRight => ('{', '}'),
                    LayoutKind::TopBottom => ('[', ']'),
                };
                out.push_str(&format!("{sx}x{sy},{xoff},{yoff}{open}"));
                for (index, child) in children.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    child.write_body(out);
                }
                out.push(close);
            }
        }
    }
}

fn tmux_layout_checksum(layout: &str) -> u16 {
    let mut checksum = 0u16;
    for byte in layout.bytes() {
        checksum = (checksum >> 1) + ((checksum & 1) << 15);
        checksum = checksum.wrapping_add(byte as u16);
    }
    checksum
}

fn parse_pane_number(pane_id: &str) -> Option<u32> {
    pane_id.trim().strip_prefix('%')?.parse().ok()
}

fn parse_layout_number(layout: &str, index: &mut usize) -> Option<u32> {
    let bytes = layout.as_bytes();
    let start = *index;
    while *index < bytes.len() && bytes[*index].is_ascii_digit() {
        *index += 1;
    }
    if *index == start {
        return None;
    }
    layout[start..*index].parse().ok()
}

fn parse_layout_node(layout: &str, index: &mut usize) -> Option<LayoutNode> {
    let sx = parse_layout_number(layout, index)?;
    if layout.as_bytes().get(*index)? != &b'x' {
        return None;
    }
    *index += 1;
    let sy = parse_layout_number(layout, index)?;
    if layout.as_bytes().get(*index)? != &b',' {
        return None;
    }
    *index += 1;
    let xoff = parse_layout_number(layout, index)?;
    if layout.as_bytes().get(*index)? != &b',' {
        return None;
    }
    *index += 1;
    let yoff = parse_layout_number(layout, index)?;

    match layout.as_bytes().get(*index).copied() {
        Some(b',') => {
            *index += 1;
            let pane_id = parse_layout_number(layout, index)?;
            Some(LayoutNode::Pane {
                sx,
                sy,
                xoff,
                yoff,
                pane_id,
            })
        }
        Some(b'{') | Some(b'[') => {
            let (kind, close) = match layout.as_bytes()[*index] {
                b'{' => (LayoutKind::LeftRight, b'}'),
                b'[' => (LayoutKind::TopBottom, b']'),
                _ => unreachable!(),
            };
            *index += 1;
            let mut children = Vec::new();
            loop {
                let child = parse_layout_node(layout, index)?;
                children.push(child);
                match layout.as_bytes().get(*index).copied() {
                    Some(b',') => *index += 1,
                    Some(ch) if ch == close => {
                        *index += 1;
                        break;
                    }
                    _ => return None,
                }
            }
            Some(LayoutNode::Split {
                sx,
                sy,
                xoff,
                yoff,
                kind,
                children,
            })
        }
        _ => None,
    }
}

fn remove_pane_from_layout(node: LayoutNode, pane_id: u32) -> Option<LayoutNode> {
    match node {
        LayoutNode::Pane { pane_id: id, .. } if id == pane_id => None,
        LayoutNode::Pane { .. } => Some(node),
        LayoutNode::Split {
            sx,
            sy,
            xoff,
            yoff,
            kind,
            children,
        } => {
            let mut kept = children
                .into_iter()
                .filter_map(|child| remove_pane_from_layout(child, pane_id))
                .collect::<Vec<_>>();

            match kept.len() {
                0 => None,
                1 => Some(kept.remove(0)),
                _ => Some(LayoutNode::Split {
                    sx,
                    sy,
                    xoff,
                    yoff,
                    kind,
                    children: kept,
                }),
            }
        }
    }
}

fn resize_layout(node: &mut LayoutNode, sx: u32, sy: u32, xoff: u32, yoff: u32) {
    node.set_geometry(sx, sy, xoff, yoff);

    let LayoutNode::Split { kind, children, .. } = node else {
        return;
    };

    if children.is_empty() {
        return;
    }

    let child_count = children.len() as u32;
    match kind {
        LayoutKind::LeftRight => {
            let content_sx = sx.saturating_sub(child_count.saturating_sub(1));
            let old_total: u32 = children.iter().map(LayoutNode::sx).sum();
            let mut next_xoff = xoff;
            let mut remaining_sx = content_sx;
            for (index, child) in children.iter_mut().enumerate() {
                let child_sx = if index + 1 == child_count as usize {
                    remaining_sx
                } else {
                    let proposed =
                        ((child.sx() as u64 * content_sx as u64) / old_total as u64).max(1) as u32;
                    let max_allowed = remaining_sx.saturating_sub(child_count - index as u32 - 1);
                    proposed.min(max_allowed)
                };
                resize_layout(child, child_sx, sy, next_xoff, yoff);
                next_xoff = next_xoff.saturating_add(child_sx + 1);
                remaining_sx = remaining_sx.saturating_sub(child_sx);
            }
        }
        LayoutKind::TopBottom => {
            let content_sy = sy.saturating_sub(child_count.saturating_sub(1));
            let old_total: u32 = children.iter().map(LayoutNode::sy).sum();
            let mut next_yoff = yoff;
            let mut remaining_sy = content_sy;
            for (index, child) in children.iter_mut().enumerate() {
                let child_sy = if index + 1 == child_count as usize {
                    remaining_sy
                } else {
                    let proposed =
                        ((child.sy() as u64 * content_sy as u64) / old_total as u64).max(1) as u32;
                    let max_allowed = remaining_sy.saturating_sub(child_count - index as u32 - 1);
                    proposed.min(max_allowed)
                };
                resize_layout(child, sx, child_sy, xoff, next_yoff);
                next_yoff = next_yoff.saturating_add(child_sy + 1);
                remaining_sy = remaining_sy.saturating_sub(child_sy);
            }
        }
    }
}

pub(super) fn layout_without_pane(layout: &str, pane_id: &str) -> Option<String> {
    let pane_id = parse_pane_number(pane_id)?;
    let (_, body) = layout.split_once(',')?;
    let mut index = 0;
    let root = parse_layout_node(body, &mut index)?;
    if index != body.len() {
        return None;
    }

    let root_sx = root.sx();
    let root_sy = root.sy();
    let mut trimmed = remove_pane_from_layout(root, pane_id)?;
    resize_layout(&mut trimmed, root_sx, root_sy, 0, 0);

    let mut out = String::new();
    trimmed.write_body(&mut out);
    Some(format!("{:04x},{}", tmux_layout_checksum(&out), out))
}

pub(super) async fn restore_window_layout<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    window_id: &str,
) {
    let layout = match model.sidebar.pane_layouts.get(window_id) {
        Some((saved_width, layout)) => {
            let (term_width, _) = model.terminal_size;
            if *saved_width != term_width {
                model.sidebar.pane_layouts.remove(window_id);
                return;
            }
            layout.clone()
        }
        None => return,
    };
    if let Err(err) = tmux
        .send_command(&format!(
            "select-layout -t {} {}",
            window_id,
            quote_tmux(&layout)
        ))
        .await
    {
        warn!(%err, window = %window_id, "restore window layout failed (ignored)");
    }
}

pub(super) async fn choose_leftmost_pane_in_window<T: TmuxApi>(
    tmux: &mut T,
    window_id: &str,
    sidebar_pane_id: &str,
) -> String {
    let pane_list = tmux
        .send_command(&format!(
            "list-panes -t {} -F '#{{pane_id}}\t#{{pane_left}}\t#{{pane_top}}'",
            window_id
        ))
        .await
        .unwrap_or_default();

    let mut best: Option<(u16, u16, String)> = None;
    for line in pane_list.lines() {
        let mut parts = line.split('\t');
        let pane_id = parts.next().unwrap_or("").trim();
        let pane_left: u16 = match parts.next().unwrap_or("").trim().parse() {
            Ok(value) => value,
            Err(_) => continue,
        };
        let pane_top: u16 = match parts.next().unwrap_or("").trim().parse() {
            Ok(value) => value,
            Err(_) => continue,
        };
        if pane_id.is_empty() || pane_id == sidebar_pane_id {
            continue;
        }

        match &best {
            Some((best_left, best_top, _))
                if pane_left > *best_left || (pane_left == *best_left && pane_top >= *best_top) => {
            }
            _ => best = Some((pane_left, pane_top, pane_id.to_string())),
        }
    }

    best.map(|(_, _, pane_id)| pane_id).unwrap_or_default()
}

pub(super) async fn choose_home_pane_in_window<T: TmuxApi>(
    tmux: &mut T,
    window_id: &str,
    sidebar_pane_id: &str,
) -> String {
    let pane_list = tmux
        .send_command(&format!(
            "list-panes -t {} -F '#{{pane_id}}\t#{{pane_active}}'",
            window_id
        ))
        .await
        .unwrap_or_default();

    let mut first_non_sidebar = String::new();
    for line in pane_list.lines() {
        let mut parts = line.split('\t');
        let pane_id = parts.next().unwrap_or("").trim();
        let active = parts.next().unwrap_or("").trim();
        if pane_id.is_empty() || pane_id == sidebar_pane_id {
            continue;
        }
        if first_non_sidebar.is_empty() {
            first_non_sidebar = pane_id.to_string();
        }
        if active == "1" {
            return pane_id.to_string();
        }
    }

    first_non_sidebar
}

pub(super) async fn ensure_sidebar_width<T: TmuxApi>(model: &Model, tmux: &mut T) {
    let width_cmd = commands::pane_width_query(&model.sidebar.pane_id);
    let needs_resize = match tmux.send_command(&width_cmd).await {
        Ok(output) => output.trim().parse::<u16>().unwrap_or(0) != SIDEBAR_WIDTH_CHARS,
        Err(_) => true,
    };
    if needs_resize {
        if let Err(err) = tmux
            .send_command(&commands::resize_pane_width(
                &model.sidebar.pane_id,
                SIDEBAR_WIDTH_CHARS,
            ))
            .await
        {
            warn!(%err, "ensure resize-pane failed");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PaneGeom {
    pub left: u32,
    pub top: u32,
}

pub(super) fn parse_layout_pane_line(line: &str) -> Option<(String, PaneGeom)> {
    let mut parts = line.trim().split('\t');
    let pane_id = parts.next()?.to_string();
    let left = parts.next()?.parse().ok()?;
    let top = parts.next()?.parse().ok()?;
    Some((pane_id, PaneGeom { left, top }))
}

fn serialize_layout(root: &LayoutNode) -> String {
    let mut out = String::new();
    root.write_body(&mut out);
    format!("{:04x},{}", tmux_layout_checksum(&out), out)
}

pub(super) fn query_layout_root(layout: &str) -> Option<LayoutNode> {
    let (_, body) = layout.split_once(',')?;
    let mut index = 0;
    let root = parse_layout_node(body, &mut index)?;
    if index != body.len() {
        return None;
    }
    Some(root)
}

pub(super) fn content_pane_ids(panes: &[(String, PaneGeom)], sidebar_pane_id: &str) -> Vec<String> {
    let mut content: Vec<(String, PaneGeom)> = panes
        .iter()
        .filter(|(id, _)| id != sidebar_pane_id)
        .map(|(id, geom)| (id.clone(), *geom))
        .collect();
    content.sort_by(|(a_id, a_geom), (b_id, b_geom)| {
        (a_geom.left, a_geom.top, a_id).cmp(&(b_geom.left, b_geom.top, b_id))
    });
    content.into_iter().map(|(id, _)| id).collect()
}

pub(super) fn build_split_window_cmd(target: &str, current_path: Option<&str>) -> String {
    match current_path {
        Some(path) if !path.is_empty() => {
            format!("split-window -d -t {target} -h -c {}", quote_tmux(path))
        }
        _ => format!("split-window -d -t {target} -h"),
    }
}

fn shell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

pub(super) fn build_cd_send_keys_cmd(target: &str, current_path: &str) -> String {
    let command = format!("cd -- {}", shell_quote(current_path));
    format!("send-keys -t {target} {} C-m", quote_tmux(&command))
}

pub(super) fn is_shell_command(current_command: &str) -> bool {
    let command = current_command.trim();
    if command.is_empty() {
        return true;
    }

    matches!(
        command,
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ash" | "ksh" | "csh" | "tcsh" | "nu" | "pwsh"
    )
}

fn split_even(total: u32, parts: usize) -> Vec<u32> {
    let parts = parts as u32;
    let base = total / parts;
    let remainder = total % parts;
    (0..parts)
        .map(|index| base + u32::from(index < remainder))
        .collect()
}

pub(super) fn build_sidebar_main_3x2_layout(
    root_sx: u32,
    root_sy: u32,
    sidebar_pane_id: u32,
    content_pane_ids: &[u32],
) -> Option<String> {
    if content_pane_ids.len() != 6 || root_sx < 36 || root_sy < 3 {
        return None;
    }

    let sidebar_sx = SIDEBAR_WIDTH_CHARS as u32;
    if root_sx <= sidebar_sx + 3 {
        return None;
    }

    let main_sx = root_sx.saturating_sub(sidebar_sx + 1);
    let column_widths = split_even(main_sx.saturating_sub(2), 3);
    let row_heights = split_even(root_sy.saturating_sub(1), 2);

    let mut next_xoff = sidebar_sx + 1;
    let mut columns = Vec::new();
    for (column_index, width) in column_widths.into_iter().enumerate() {
        let mut next_yoff = 0;
        let mut rows = Vec::new();
        for (row_index, height) in row_heights.iter().copied().enumerate() {
            let pane_id = content_pane_ids[column_index * 2 + row_index];
            rows.push(LayoutNode::Pane {
                sx: width,
                sy: height,
                xoff: next_xoff,
                yoff: next_yoff,
                pane_id,
            });
            next_yoff = next_yoff.saturating_add(height + 1);
        }
        columns.push(LayoutNode::Split {
            sx: width,
            sy: root_sy,
            xoff: next_xoff,
            yoff: 0,
            kind: LayoutKind::TopBottom,
            children: rows,
        });
        next_xoff = next_xoff.saturating_add(width + 1);
    }

    let root = LayoutNode::Split {
        sx: root_sx,
        sy: root_sy,
        xoff: 0,
        yoff: 0,
        kind: LayoutKind::LeftRight,
        children: vec![
            LayoutNode::Pane {
                sx: sidebar_sx,
                sy: root_sy,
                xoff: 0,
                yoff: 0,
                pane_id: sidebar_pane_id,
            },
            LayoutNode::Split {
                sx: main_sx,
                sy: root_sy,
                xoff: sidebar_sx + 1,
                yoff: 0,
                kind: LayoutKind::LeftRight,
                children: columns,
            },
        ],
    };

    Some(serialize_layout(&root))
}

pub(super) async fn reapply_helper_layout_if_needed<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    window_id: &str,
) {
    if !model.sidebar.helper_managed_windows.contains(window_id) {
        return;
    }

    let list_cmd = format!(
        "list-panes -t {} -F '#{{pane_id}}\t#{{pane_left}}\t#{{pane_top}}'",
        window_id
    );
    let Ok(output) = tmux.send_command(&list_cmd).await else {
        return;
    };

    let panes: Vec<(String, PaneGeom)> =
        output.lines().filter_map(parse_layout_pane_line).collect();
    let content_ids = content_pane_ids(&panes, &model.sidebar.pane_id);
    if content_ids.len() != 6 {
        return;
    }

    let Some(layout) = query_window_layout(tmux, window_id).await else {
        return;
    };
    let Some(root) = query_layout_root(&layout) else {
        return;
    };
    let Some(sidebar_pane_id) = parse_pane_number(&model.sidebar.pane_id) else {
        return;
    };

    let mut content_pane_numbers = Vec::with_capacity(content_ids.len());
    for pane_id in &content_ids {
        let Some(pane_number) = parse_pane_number(pane_id) else {
            return;
        };
        content_pane_numbers.push(pane_number);
    }

    let Some(explicit_layout) =
        build_sidebar_main_3x2_layout(root.sx(), root.sy(), sidebar_pane_id, &content_pane_numbers)
    else {
        return;
    };

    suppress_sidebar_layout_validation(model);
    let _ = tmux
        .send_command(&format!(
            "select-layout -t {} {}",
            window_id,
            quote_tmux(&explicit_layout)
        ))
        .await;
}

fn suppress_sidebar_layout_validation(model: &mut Model) {
    model.sidebar.ignore_layout_change_until = Some(
        std::time::Instant::now() + std::time::Duration::from_millis(LAYOUT_CHANGE_SUPPRESSION_MS),
    );
}

pub(super) async fn validate_sidebar_panes<T: TmuxApi>(
    model: &Model,
    tmux: &mut T,
    queue: &mut VecDeque<Cmd>,
) {
    let pane_list = tmux
        .send_command(&format!(
            "list-panes -t {} -F '#{{pane_id}}'",
            model.sidebar.window_id
        ))
        .await
        .unwrap_or_default();

    let has_content = pane_list
        .lines()
        .map(|line| line.trim())
        .any(|line| !line.is_empty() && line != model.sidebar.pane_id);

    if !has_content {
        debug!(
            window = %model.sidebar.window_id,
            "sidebar window lost all content panes, evacuating"
        );
        match &model.sidebar.preview {
            PreviewState::Previewing { .. } => {
                queue.push_front(Cmd::ListWindows);
                queue.push_front(Cmd::RestorePreview);
            }
            PreviewState::Home => {
                let sidebar_window_id = model.sidebar.window_id.clone();
                if let Some(other_id) = model.find_another_window_id(&sidebar_window_id) {
                    queue.push_front(Cmd::ListWindows);
                    queue.push_front(Cmd::FollowToWindow {
                        window_id: other_id,
                    });
                }
            }
        }
    }
}

pub(super) async fn apply_layout_helper<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    queue: &mut VecDeque<Cmd>,
) {
    suppress_sidebar_layout_validation(model);
    let list_cmd = format!(
        "list-panes -t {} -F '#{{pane_id}}\t#{{pane_left}}\t#{{pane_top}}'",
        model.sidebar.window_id
    );
    let Ok(output) = tmux.send_command(&list_cmd).await else {
        model.error_message = Some("layout helper: list-panes failed".to_string());
        queue.push_front(Cmd::Render);
        return;
    };

    let mut panes: Vec<(String, PaneGeom)> =
        output.lines().filter_map(parse_layout_pane_line).collect();
    let initial_content_ids: HashSet<String> = content_pane_ids(&panes, &model.sidebar.pane_id)
        .into_iter()
        .collect();
    let content_count = initial_content_ids.len();
    if content_count == 0 {
        model.error_message = Some("layout helper: no content pane".to_string());
        queue.push_front(Cmd::Render);
        return;
    }
    if content_count > 6 {
        model.error_message = Some("layout helper: too many panes".to_string());
        queue.push_front(Cmd::Render);
        return;
    }

    let base_pane_id = content_pane_ids(&panes, &model.sidebar.pane_id)
        .into_iter()
        .next()
        .expect("content_count > 0 ensures content pane exists");
    let base_current_path = query_pane_current_path(tmux, &base_pane_id).await;

    let mut splits_needed = 6usize.saturating_sub(content_count);
    while splits_needed > 0 {
        let target = panes
            .iter()
            .filter(|(id, _)| id != &model.sidebar.pane_id)
            .max_by_key(|(_, geom)| (geom.left, geom.top))
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| model.sidebar.pane_id.clone());
        let split_cmd = build_split_window_cmd(&target, base_current_path.as_deref());
        if let Err(err) = tmux.send_command(&split_cmd).await {
            model.error_message = Some(format!("layout helper: {err}"));
            queue.push_front(Cmd::Render);
            return;
        }
        let Ok(refreshed) = tmux.send_command(&list_cmd).await else {
            model.error_message = Some("layout helper: refresh failed".to_string());
            queue.push_front(Cmd::Render);
            return;
        };
        panes = refreshed
            .lines()
            .filter_map(parse_layout_pane_line)
            .collect();
        splits_needed -= 1;
    }

    let Some(layout) = query_window_layout(tmux, &model.sidebar.window_id).await else {
        model.error_message = Some("layout helper: layout query failed".to_string());
        queue.push_front(Cmd::Render);
        return;
    };

    let Some(root) = query_layout_root(&layout) else {
        model.error_message = Some("layout helper: layout parse failed".to_string());
        queue.push_front(Cmd::Render);
        return;
    };

    let content_ids = content_pane_ids(&panes, &model.sidebar.pane_id);
    let Some(sidebar_pane_id) = parse_pane_number(&model.sidebar.pane_id) else {
        model.error_message = Some("layout helper: invalid sidebar pane".to_string());
        queue.push_front(Cmd::Render);
        return;
    };
    let mut content_pane_numbers = Vec::with_capacity(content_ids.len());
    for pane_id in &content_ids {
        let Some(pane_number) = parse_pane_number(pane_id) else {
            model.error_message = Some("layout helper: invalid pane id".to_string());
            queue.push_front(Cmd::Render);
            return;
        };
        content_pane_numbers.push(pane_number);
    }
    let Some(explicit_layout) =
        build_sidebar_main_3x2_layout(root.sx(), root.sy(), sidebar_pane_id, &content_pane_numbers)
    else {
        model.error_message = Some("layout helper: window too small".to_string());
        queue.push_front(Cmd::Render);
        return;
    };

    if let Err(err) = tmux
        .send_command(&format!(
            "select-layout -t {} {}",
            model.sidebar.window_id,
            quote_tmux(&explicit_layout)
        ))
        .await
    {
        model.error_message = Some(format!("layout helper: {err}"));
        queue.push_front(Cmd::Render);
        return;
    }

    let disable_rename = commands::disable_window_rename(&model.sidebar.window_id);
    if let Err(err) = tmux.send_command(&disable_rename).await {
        warn!(
            window_id = %model.sidebar.window_id,
            %err,
            "layout helper: failed to disable automatic rename"
        );
    }

    let top = &content_ids[4];
    let bottom = &content_ids[5];

    let helper_already_managed = model
        .sidebar
        .helper_managed_windows
        .contains(&model.sidebar.window_id);
    let top_is_new = !initial_content_ids.contains(top);
    let bottom_is_new = !initial_content_ids.contains(bottom);
    let needs_existing_pane_commands = !helper_already_managed && (!top_is_new || !bottom_is_new);
    let pane_commands: HashMap<String, String> = if needs_existing_pane_commands {
        let commands_cmd = format!(
            "list-panes -t {} -F '#{{pane_id}}\t#{{pane_current_command}}'",
            model.sidebar.window_id
        );
        tmux.send_command(&commands_cmd)
            .await
            .ok()
            .map(|output| {
                output
                    .lines()
                    .filter_map(|line| {
                        let mut parts = line.trim().split('\t');
                        Some((parts.next()?.to_string(), parts.next()?.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        HashMap::new()
    };

    let can_initialize_existing_pane = |pane_id: &String| {
        pane_commands
            .get(pane_id)
            .is_some_and(|command| is_shell_command(command))
    };

    if let Some(base_current_path) = base_current_path.as_deref() {
        for pane_id in &content_ids {
            let is_new = !initial_content_ids.contains(pane_id);
            let is_helper_slot = pane_id == top || pane_id == bottom;
            let should_send_cd = if is_new {
                true
            } else if !helper_already_managed && is_helper_slot {
                can_initialize_existing_pane(pane_id)
            } else {
                false
            };

            if should_send_cd {
                let _ = tmux
                    .send_command(&build_cd_send_keys_cmd(pane_id, base_current_path))
                    .await;
            }
        }
    }

    if !helper_already_managed {
        if top_is_new || can_initialize_existing_pane(top) {
            let _ = tmux
                .send_command(&format!("send-keys -t {top} lazygit C-m"))
                .await;
        }
        if bottom_is_new || can_initialize_existing_pane(bottom) {
            let _ = tmux
                .send_command(&format!("send-keys -t {bottom} yazi C-m"))
                .await;
        }
    }
    let _ = tmux
        .send_command(&commands::resize_pane_width(
            &model.sidebar.pane_id,
            SIDEBAR_WIDTH_CHARS,
        ))
        .await;
    model.info_message = Some(
        if helper_already_managed {
            "layout helper refreshed"
        } else {
            "layout helper applied"
        }
        .to_string(),
    );
    model
        .sidebar
        .helper_managed_windows
        .insert(model.sidebar.window_id.clone());
    queue.push_front(Cmd::Render);
    queue.push_front(Cmd::ListWindows);
}

pub(super) async fn reconcile_sidebar_state<T: TmuxApi>(model: &mut Model, tmux: &mut T) {
    if let Some((window_id, home_pane_id)) =
        query_sidebar_window_and_home(tmux, &model.session_name, &model.sidebar.pane_id).await
    {
        if model.sidebar.window_id != window_id {
            info!(
                old = %model.sidebar.window_id,
                new = %window_id,
                "reconcile: sidebar window id updated"
            );
        }
        model.sidebar.window_id = window_id.clone();

        if !home_pane_id.is_empty() {
            if model.sidebar.home_pane_id != home_pane_id {
                info!(
                    old = %model.sidebar.home_pane_id,
                    new = %home_pane_id,
                    window = %window_id,
                    "reconcile: home pane id updated"
                );
            }
            model.sidebar.home_pane_id = home_pane_id;
        } else {
            warn!(
                window = %window_id,
                "reconcile: could not determine non-sidebar home pane"
            );
        }
        return;
    }

    let mut window_updated = false;
    let current_window = tmux
        .send_command(&format!(
            "display-message -t {} -p '#{{window_id}}'",
            model.sidebar.pane_id
        ))
        .await
        .ok()
        .map(|output| output.trim().to_string())
        .filter(|value| !value.is_empty());

    if let Some(window_id) = current_window {
        if model.sidebar.window_id != window_id {
            info!(
                old = %model.sidebar.window_id,
                new = %window_id,
                "reconcile: sidebar window id updated"
            );
        }
        model.sidebar.window_id = window_id;
        window_updated = true;
    }

    let new_home =
        choose_home_pane_in_window(tmux, &model.sidebar.window_id, &model.sidebar.pane_id).await;
    if !new_home.is_empty() {
        if model.sidebar.home_pane_id != new_home {
            info!(
                old = %model.sidebar.home_pane_id,
                new = %new_home,
                window = %model.sidebar.window_id,
                "reconcile: home pane id updated"
            );
        }
        model.sidebar.home_pane_id = new_home;
    } else if window_updated {
        warn!(
            window = %model.sidebar.window_id,
            "reconcile: could not determine non-sidebar home pane"
        );
    }
}
