use std::collections::HashMap;

use anyhow::{anyhow, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: String,
    pub index: usize,
    pub name: String,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeNode {
    Folder {
        name: String,
        children: Vec<TreeNode>,
        expanded: bool,
    },
    Window {
        info: WindowInfo,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlatNodeKind {
    Folder,
    Window,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatItem {
    pub depth: usize,
    pub path: Vec<usize>,
    pub kind: FlatNodeKind,
}

pub fn build_tree(windows: &[WindowInfo]) -> Vec<TreeNode> {
    let mut roots: Vec<TreeNode> = Vec::new();
    let mut folder_positions: HashMap<String, usize> = HashMap::new();

    for window in windows {
        if let Some((folder_name, child_name)) = split_folder_name(&window.name) {
            let folder_index = if let Some(&idx) = folder_positions.get(folder_name) {
                idx
            } else {
                let idx = roots.len();
                roots.push(TreeNode::Folder {
                    name: folder_name.to_string(),
                    children: Vec::new(),
                    expanded: true,
                });
                folder_positions.insert(folder_name.to_string(), idx);
                idx
            };

            let mut child_info = window.clone();
            child_info.name = child_name.to_string();

            if let Some(TreeNode::Folder { children, .. }) = roots.get_mut(folder_index) {
                children.push(TreeNode::Window { info: child_info });
            }
        } else {
            roots.push(TreeNode::Window {
                info: window.clone(),
            });
        }
    }

    roots
}

pub fn flatten(nodes: &[TreeNode]) -> Vec<FlatItem> {
    let mut out = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        let mut path = Vec::with_capacity(2);
        path.push(index);
        flatten_node(node, 0, &mut path, &mut out);
    }
    out
}

pub fn toggle_expand(node: &mut TreeNode) {
    if let TreeNode::Folder { expanded, .. } = node {
        *expanded = !*expanded;
    }
}

pub fn find_parent_folder(flat: &[FlatItem], current: usize) -> Option<usize> {
    let item = flat.get(current)?;
    if item.depth == 0 {
        return None;
    }

    let parent_depth = item.depth - 1;
    (0..current)
        .rev()
        .find(|&idx| flat[idx].depth == parent_depth && flat[idx].kind == FlatNodeKind::Folder)
}

pub fn next_visible_item(flat: &[FlatItem], current: usize) -> Option<usize> {
    if current >= flat.len() {
        return None;
    }

    let next = current + 1;
    if next < flat.len() {
        Some(next)
    } else {
        None
    }
}

pub fn prev_visible_item(flat: &[FlatItem], current: usize) -> Option<usize> {
    if current == 0 || current >= flat.len() {
        None
    } else {
        Some(current - 1)
    }
}

pub fn get_node<'a>(nodes: &'a [TreeNode], path: &[usize]) -> Result<&'a TreeNode> {
    let (first, rest) = path
        .split_first()
        .ok_or_else(|| anyhow!("path cannot be empty"))?;

    let mut node = nodes
        .get(*first)
        .ok_or_else(|| anyhow!("root index out of bounds: {}", first))?;

    for index in rest {
        match node {
            TreeNode::Folder { children, .. } => {
                node = children
                    .get(*index)
                    .ok_or_else(|| anyhow!("child index out of bounds: {}", index))?;
            }
            TreeNode::Window { .. } => {
                return Err(anyhow!("cannot descend into window node"));
            }
        }
    }

    Ok(node)
}

pub fn get_node_mut<'a>(nodes: &'a mut [TreeNode], path: &[usize]) -> Result<&'a mut TreeNode> {
    fn descend<'a>(nodes: &'a mut [TreeNode], path: &[usize]) -> Result<&'a mut TreeNode> {
        let (index, rest) = path
            .split_first()
            .ok_or_else(|| anyhow!("path cannot be empty"))?;

        let node = nodes
            .get_mut(*index)
            .ok_or_else(|| anyhow!("index out of bounds: {}", index))?;

        if rest.is_empty() {
            return Ok(node);
        }

        match node {
            TreeNode::Folder { children, .. } => descend(children.as_mut_slice(), rest),
            TreeNode::Window { .. } => Err(anyhow!("cannot descend into window node")),
        }
    }

    descend(nodes, path)
}

/// Expand folder ancestors needed to show the given window in the flat list.
///
/// Returns `true` when `window_id` was found.
pub fn expand_to_window_by_id(nodes: &mut [TreeNode], window_id: &str) -> bool {
    for node in nodes.iter_mut() {
        match node {
            TreeNode::Window { info } => {
                if info.id == window_id {
                    return true;
                }
            }
            TreeNode::Folder {
                expanded, children, ..
            } => {
                if expand_to_window_by_id(children.as_mut_slice(), window_id) {
                    *expanded = true;
                    return true;
                }
            }
        }
    }
    false
}

/// Return the flat index for a window id, if present in the current tree.
pub fn find_window_flat_index_by_id(
    flat_items: &[FlatItem],
    nodes: &[TreeNode],
    window_id: &str,
) -> Option<usize> {
    for (idx, item) in flat_items.iter().enumerate() {
        if item.kind != FlatNodeKind::Window {
            continue;
        }
        if let Ok(TreeNode::Window { info }) = get_node(nodes, &item.path) {
            if info.id == window_id {
                return Some(idx);
            }
        }
    }
    None
}

fn flatten_node(node: &TreeNode, depth: usize, path: &mut Vec<usize>, out: &mut Vec<FlatItem>) {
    match node {
        TreeNode::Folder {
            children, expanded, ..
        } => {
            out.push(FlatItem {
                depth,
                path: path.clone(),
                kind: FlatNodeKind::Folder,
            });

            if *expanded {
                for (child_idx, child) in children.iter().enumerate() {
                    path.push(child_idx);
                    flatten_node(child, depth + 1, path, out);
                    path.pop();
                }
            }
        }
        TreeNode::Window { .. } => {
            out.push(FlatItem {
                depth,
                path: path.clone(),
                kind: FlatNodeKind::Window,
            });
        }
    }
}

fn split_folder_name(name: &str) -> Option<(&str, &str)> {
    let (folder, child) = name.split_once(':')?;
    if folder.is_empty() || child.is_empty() {
        return None;
    }
    Some((folder, child))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(id: &str, index: usize, name: &str) -> WindowInfo {
        WindowInfo {
            id: id.to_string(),
            index,
            name: name.to_string(),
            active: false,
        }
    }

    #[test]
    fn build_tree_groups_by_prefix_and_preserves_order() {
        let windows = vec![
            w("@1", 1, "proj:edit"),
            w("@2", 2, "scratch"),
            w("@3", 3, "proj:term"),
            w("@4", 4, "other:main"),
            w("@5", 5, "proj:logs"),
            w("@6", 6, "bare"),
        ];

        let tree = build_tree(&windows);
        assert_eq!(tree.len(), 4);

        match &tree[0] {
            TreeNode::Folder {
                name,
                children,
                expanded,
            } => {
                assert_eq!(name, "proj");
                assert!(*expanded);
                assert_eq!(children.len(), 3);
                assert_eq!(window_name(&children[0]), "edit");
                assert_eq!(window_name(&children[1]), "term");
                assert_eq!(window_name(&children[2]), "logs");
            }
            _ => panic!("expected folder"),
        }

        assert_eq!(window_name(&tree[1]), "scratch");

        match &tree[2] {
            TreeNode::Folder { name, children, .. } => {
                assert_eq!(name, "other");
                assert_eq!(children.len(), 1);
                assert_eq!(window_name(&children[0]), "main");
            }
            _ => panic!("expected folder"),
        }

        assert_eq!(window_name(&tree[3]), "bare");
    }

    #[test]
    fn flatten_hides_collapsed_children() {
        let mut tree = build_tree(&[
            w("@1", 1, "proj:edit"),
            w("@2", 2, "proj:term"),
            w("@3", 3, "solo"),
        ]);
        if let TreeNode::Folder { expanded, .. } = &mut tree[0] {
            *expanded = false;
        }

        let flat = flatten(&tree);
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].depth, 0);
        assert_eq!(flat[0].kind, FlatNodeKind::Folder);
        assert_eq!(flat[1].depth, 0);
        assert_eq!(flat[1].kind, FlatNodeKind::Window);
    }

    #[test]
    fn toggle_expand_only_for_folders() {
        let mut folder = TreeNode::Folder {
            name: "proj".to_string(),
            children: vec![],
            expanded: true,
        };
        toggle_expand(&mut folder);
        assert!(matches!(
            folder,
            TreeNode::Folder {
                expanded: false,
                ..
            }
        ));

        let mut window = TreeNode::Window {
            info: w("@1", 1, "edit"),
        };
        toggle_expand(&mut window);
        assert!(matches!(window, TreeNode::Window { .. }));
    }

    #[test]
    fn navigation_helpers_work() {
        let tree = build_tree(&[
            w("@1", 1, "proj:edit"),
            w("@2", 2, "proj:term"),
            w("@3", 3, "solo"),
        ]);
        let flat = flatten(&tree);

        assert_eq!(flat.len(), 4);
        assert_eq!(find_parent_folder(&flat, 1), Some(0));
        assert_eq!(find_parent_folder(&flat, 2), Some(0));
        assert_eq!(find_parent_folder(&flat, 0), None);
        assert_eq!(find_parent_folder(&flat, 3), None);

        assert_eq!(next_visible_item(&flat, 0), Some(1));
        assert_eq!(next_visible_item(&flat, 2), Some(3));
        assert_eq!(next_visible_item(&flat, 3), None);

        assert_eq!(prev_visible_item(&flat, 0), None);
        assert_eq!(prev_visible_item(&flat, 1), Some(0));
        assert_eq!(prev_visible_item(&flat, 3), Some(2));
    }

    #[test]
    fn get_node_by_path_supports_nested_access() {
        let tree = build_tree(&[w("@1", 1, "proj:edit"), w("@2", 2, "solo")]);
        let flat = flatten(&tree);

        let folder = get_node(&tree, &flat[0].path).unwrap();
        assert!(matches!(folder, TreeNode::Folder { .. }));

        let child = get_node(&tree, &flat[1].path).unwrap();
        assert_eq!(window_name(child), "edit");
    }

    #[test]
    fn find_window_flat_index_by_id_finds_window() {
        let tree = build_tree(&[
            w("@1", 1, "proj:edit"),
            w("@2", 2, "proj:term"),
            w("@3", 3, "solo"),
        ]);
        let flat = flatten(&tree);

        assert_eq!(find_window_flat_index_by_id(&flat, &tree, "@2"), Some(2));
        assert_eq!(find_window_flat_index_by_id(&flat, &tree, "@missing"), None);
    }

    #[test]
    fn expand_to_window_by_id_expands_path() {
        let mut tree = build_tree(&[
            w("@1", 1, "proj:edit"),
            w("@2", 2, "solo"),
        ]);

        assert!(expand_to_window_by_id(tree.as_mut_slice(), "@1"));

        assert_eq!(
            match &tree[0] {
                TreeNode::Folder { expanded, .. } => *expanded,
                _ => false,
            },
            true
        );
        assert!(!expand_to_window_by_id(tree.as_mut_slice(), "@missing"));
    }

    fn window_name(node: &TreeNode) -> &str {
        match node {
            TreeNode::Window { info } => &info.name,
            _ => panic!("expected window"),
        }
    }
}
