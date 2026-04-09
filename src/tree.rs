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
    /// True when this is an expanded folder (used for cursor skip logic).
    pub expanded: bool,
    /// 1-based visible index for navigable rows, cached for rendering.
    pub visible_number: Option<usize>,
    /// Whether this item is the last sibling in its parent list.
    pub is_last_sibling: bool,
}

pub fn build_tree(windows: &[WindowInfo]) -> Vec<TreeNode> {
    let mut roots: Vec<TreeNode> = Vec::new();
    let mut folder_positions: HashMap<String, usize> = HashMap::new();

    for window in windows {
        if let Some((folder_name, remainder)) = split_folder_name(&window.name) {
            let folder_index = ensure_folder_index(
                &mut roots,
                &mut folder_positions,
                folder_name.to_string(),
                folder_name,
            );

            if let Some(TreeNode::Folder { children, .. }) = roots.get_mut(folder_index) {
                // Check for second-level folder: folder:subfolder:tab
                if let Some((subfolder_name, leaf_name)) = split_folder_name(remainder) {
                    let sub_key = format!("{}:{}", folder_name, subfolder_name);
                    let sub_index = ensure_folder_index(
                        children,
                        &mut folder_positions,
                        sub_key,
                        subfolder_name,
                    );
                    if let Some(TreeNode::Folder {
                        children: sub_children,
                        ..
                    }) = children.get_mut(sub_index)
                    {
                        push_window_leaf(sub_children, window, leaf_name);
                    }
                } else {
                    push_window_leaf(children, window, remainder);
                }
            }
        } else {
            roots.push(TreeNode::Window {
                info: window.clone(),
            });
        }
    }

    roots
}

fn ensure_folder_index(
    nodes: &mut Vec<TreeNode>,
    folder_positions: &mut HashMap<String, usize>,
    key: String,
    display_name: &str,
) -> usize {
    if let Some(&idx) = folder_positions.get(&key) {
        idx
    } else {
        let idx = nodes.len();
        nodes.push(TreeNode::Folder {
            name: display_name.to_string(),
            children: Vec::new(),
            expanded: true,
        });
        folder_positions.insert(key, idx);
        idx
    }
}

fn push_window_leaf(nodes: &mut Vec<TreeNode>, window: &WindowInfo, leaf_name: &str) {
    let mut child_info = window.clone();
    child_info.name = leaf_name.to_string();
    nodes.push(TreeNode::Window { info: child_info });
}

pub fn flatten(nodes: &[TreeNode]) -> Vec<FlatItem> {
    let mut out = Vec::new();
    let mut visible_number = 0;
    for (index, node) in nodes.iter().enumerate() {
        let mut path = Vec::with_capacity(2);
        path.push(index);
        flatten_node(
            node,
            0,
            &mut path,
            index + 1 == nodes.len(),
            &mut visible_number,
            &mut out,
        );
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

/// Returns true if a flat item can be the cursor target (i.e. a navigable row).
/// Expanded folder headers are skipped — the cursor moves directly to their children.
pub fn is_navigable_item(item: &FlatItem) -> bool {
    !(item.kind == FlatNodeKind::Folder && item.expanded)
}

pub fn visible_item_indices(flat: &[FlatItem]) -> Vec<usize> {
    flat.iter()
        .enumerate()
        .filter_map(|(idx, item)| is_navigable_item(item).then_some(idx))
        .collect()
}

pub fn visible_item_number(flat: &[FlatItem], index: usize) -> Option<usize> {
    flat.get(index)?.visible_number
}

pub fn next_visible_item(flat: &[FlatItem], current: usize) -> Option<usize> {
    if current >= flat.len() {
        return None;
    }

    let mut idx = current + 1;
    while idx < flat.len() {
        // Skip expanded folders — cursor goes straight to their children
        if !is_navigable_item(&flat[idx]) {
            idx += 1;
            continue;
        }
        return Some(idx);
    }
    None
}

pub fn prev_visible_item(flat: &[FlatItem], current: usize) -> Option<usize> {
    if current == 0 || current >= flat.len() {
        return None;
    }

    let mut idx = current - 1;
    loop {
        // Skip expanded folders
        if !is_navigable_item(&flat[idx]) {
            if idx == 0 {
                return None;
            }
            idx -= 1;
            continue;
        }
        return Some(idx);
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

pub fn move_node_to_position(
    nodes: &mut Vec<TreeNode>,
    path: &[usize],
    position: usize,
) -> Result<()> {
    fn siblings_for_parent_path<'a>(
        nodes: &'a mut Vec<TreeNode>,
        parent_path: &[usize],
    ) -> Result<&'a mut Vec<TreeNode>> {
        if parent_path.is_empty() {
            return Ok(nodes);
        }

        let node = get_node_mut(nodes.as_mut_slice(), parent_path)?;
        match node {
            TreeNode::Folder { children, .. } => Ok(children),
            TreeNode::Window { .. } => Err(anyhow!("window node has no children")),
        }
    }

    let (index, parent_path) = path
        .split_last()
        .ok_or_else(|| anyhow!("path cannot be empty"))?;
    let siblings = siblings_for_parent_path(nodes, parent_path)?;

    if *index >= siblings.len() {
        return Err(anyhow!("index out of bounds: {}", index));
    }
    if position == 0 || position > siblings.len() {
        return Err(anyhow!("position out of bounds: {}", position));
    }

    let target = position - 1;
    if target == *index {
        return Ok(());
    }

    let node = siblings.remove(*index);
    siblings.insert(target, node);
    Ok(())
}

pub fn folder_path_for_path(nodes: &[TreeNode], path: &[usize]) -> Result<String> {
    let mut parts = Vec::new();
    for depth in 0..path.len() {
        let ancestor_path = &path[..=depth];
        if let TreeNode::Folder { name, .. } = get_node(nodes, ancestor_path)? {
            parts.push(name.clone());
        }
    }
    Ok(parts.join(":"))
}

pub fn window_ids_in_order(nodes: &[TreeNode]) -> Vec<String> {
    fn visit(nodes: &[TreeNode], out: &mut Vec<String>) {
        for node in nodes {
            match node {
                TreeNode::Window { info } => out.push(info.id.clone()),
                TreeNode::Folder { children, .. } => visit(children, out),
            }
        }
    }

    let mut out = Vec::new();
    visit(nodes, &mut out);
    out
}

pub fn find_folder_flat_index_by_path(
    flat_items: &[FlatItem],
    nodes: &[TreeNode],
    folder_path: &str,
) -> Option<usize> {
    for (idx, item) in flat_items.iter().enumerate() {
        if item.kind != FlatNodeKind::Folder {
            continue;
        }
        if folder_path_for_path(nodes, &item.path).ok().as_deref() == Some(folder_path) {
            return Some(idx);
        }
    }
    None
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

fn flatten_node(
    node: &TreeNode,
    depth: usize,
    path: &mut Vec<usize>,
    is_last_sibling: bool,
    visible_number: &mut usize,
    out: &mut Vec<FlatItem>,
) {
    match node {
        TreeNode::Folder {
            children, expanded, ..
        } => {
            let item_visible_number = if *expanded {
                None
            } else {
                *visible_number += 1;
                Some(*visible_number)
            };
            out.push(FlatItem {
                depth,
                path: path.clone(),
                kind: FlatNodeKind::Folder,
                expanded: *expanded,
                visible_number: item_visible_number,
                is_last_sibling,
            });

            if *expanded {
                for (child_idx, child) in children.iter().enumerate() {
                    path.push(child_idx);
                    flatten_node(
                        child,
                        depth + 1,
                        path,
                        child_idx + 1 == children.len(),
                        visible_number,
                        out,
                    );
                    path.pop();
                }
            }
        }
        TreeNode::Window { .. } => {
            *visible_number += 1;
            out.push(FlatItem {
                depth,
                path: path.clone(),
                kind: FlatNodeKind::Window,
                expanded: false,
                visible_number: Some(*visible_number),
                is_last_sibling,
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
        assert_eq!(flat[0].visible_number, Some(1));
        assert!(!flat[0].is_last_sibling);
        assert_eq!(flat[1].depth, 0);
        assert_eq!(flat[1].kind, FlatNodeKind::Window);
        assert_eq!(flat[1].visible_number, Some(2));
        assert!(flat[1].is_last_sibling);
    }

    #[test]
    fn flatten_caches_visible_numbers_and_sibling_positions() {
        let tree = build_tree(&[
            w("@1", 1, "proj:edit"),
            w("@2", 2, "proj:term"),
            w("@3", 3, "solo"),
        ]);

        let flat = flatten(&tree);

        assert_eq!(flat[0].visible_number, None); // expanded folder is skipped
        assert!(!flat[0].is_last_sibling);

        assert_eq!(flat[1].visible_number, Some(1));
        assert!(!flat[1].is_last_sibling);

        assert_eq!(flat[2].visible_number, Some(2));
        assert!(flat[2].is_last_sibling);

        assert_eq!(flat[3].visible_number, Some(3));
        assert!(flat[3].is_last_sibling);
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

        // flat[0] = "proj" folder (expanded) → skipped by cursor
        assert_eq!(next_visible_item(&flat, 0), Some(1)); // expanded folder → first child
        assert_eq!(next_visible_item(&flat, 2), Some(3));
        assert_eq!(next_visible_item(&flat, 3), None);

        assert_eq!(prev_visible_item(&flat, 0), None);
        // expanded folder at [0] is skipped
        assert_eq!(prev_visible_item(&flat, 1), None);
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
        let mut tree = build_tree(&[w("@1", 1, "proj:edit"), w("@2", 2, "solo")]);

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

    #[test]
    fn build_tree_two_level_nesting() {
        let windows = vec![
            w("@1", 1, "proj:sub:edit"),
            w("@2", 2, "proj:sub:term"),
            w("@3", 3, "proj:main"),
            w("@4", 4, "solo"),
        ];

        let tree = build_tree(&windows);
        assert_eq!(tree.len(), 2); // folder "proj" + window "solo"

        match &tree[0] {
            TreeNode::Folder { name, children, .. } => {
                assert_eq!(name, "proj");
                assert_eq!(children.len(), 2); // subfolder "sub" + window "main"

                match &children[0] {
                    TreeNode::Folder {
                        name,
                        children: sub_children,
                        ..
                    } => {
                        assert_eq!(name, "sub");
                        assert_eq!(sub_children.len(), 2);
                        assert_eq!(window_name(&sub_children[0]), "edit");
                        assert_eq!(window_name(&sub_children[1]), "term");
                    }
                    _ => panic!("expected subfolder"),
                }

                assert_eq!(window_name(&children[1]), "main");
            }
            _ => panic!("expected folder"),
        }

        assert_eq!(window_name(&tree[1]), "solo");
    }

    #[test]
    fn flatten_two_level_nesting() {
        let tree = build_tree(&[
            w("@1", 1, "proj:sub:edit"),
            w("@2", 2, "proj:sub:term"),
            w("@3", 3, "proj:main"),
            w("@4", 4, "solo"),
        ]);

        let flat = flatten(&tree);
        // proj(0) > sub(1) > edit(2), term(3), main(4), solo(5)
        // but proj and sub are expanded folders so depth 0,1,2,2,1,0
        assert_eq!(flat.len(), 6);
        assert_eq!(flat[0].depth, 0);
        assert_eq!(flat[0].kind, FlatNodeKind::Folder); // proj
        assert_eq!(flat[1].depth, 1);
        assert_eq!(flat[1].kind, FlatNodeKind::Folder); // sub
        assert_eq!(flat[2].depth, 2);
        assert_eq!(flat[2].kind, FlatNodeKind::Window); // edit
        assert_eq!(flat[3].depth, 2);
        assert_eq!(flat[3].kind, FlatNodeKind::Window); // term
        assert_eq!(flat[4].depth, 1);
        assert_eq!(flat[4].kind, FlatNodeKind::Window); // main
        assert_eq!(flat[5].depth, 0);
        assert_eq!(flat[5].kind, FlatNodeKind::Window); // solo
    }

    #[test]
    fn flatten_two_level_nesting_caches_nested_sibling_positions() {
        let tree = build_tree(&[
            w("@1", 1, "proj:sub:edit"),
            w("@2", 2, "proj:sub:term"),
            w("@3", 3, "proj:main"),
            w("@4", 4, "solo"),
        ]);

        let flat = flatten(&tree);

        assert_eq!(flat[0].visible_number, None); // proj
        assert!(!flat[0].is_last_sibling);

        assert_eq!(flat[1].visible_number, None); // sub
        assert!(!flat[1].is_last_sibling);

        assert_eq!(flat[2].visible_number, Some(1)); // edit
        assert!(!flat[2].is_last_sibling);

        assert_eq!(flat[3].visible_number, Some(2)); // term
        assert!(flat[3].is_last_sibling);

        assert_eq!(flat[4].visible_number, Some(3)); // main
        assert!(flat[4].is_last_sibling);

        assert_eq!(flat[5].visible_number, Some(4)); // solo
        assert!(flat[5].is_last_sibling);
    }

    #[test]
    fn find_window_in_nested_folder() {
        let tree = build_tree(&[w("@1", 1, "proj:sub:edit"), w("@2", 2, "solo")]);
        let flat = flatten(&tree);

        assert_eq!(find_window_flat_index_by_id(&flat, &tree, "@1"), Some(2));
    }

    #[test]
    fn expand_to_window_in_nested_folder() {
        let mut tree = build_tree(&[w("@1", 1, "proj:sub:edit"), w("@2", 2, "solo")]);

        // Collapse both levels
        if let TreeNode::Folder {
            expanded, children, ..
        } = &mut tree[0]
        {
            *expanded = false;
            if let TreeNode::Folder { expanded, .. } = &mut children[0] {
                *expanded = false;
            }
        }

        assert!(expand_to_window_by_id(tree.as_mut_slice(), "@1"));

        // Both levels should be expanded now
        match &tree[0] {
            TreeNode::Folder {
                expanded, children, ..
            } => {
                assert!(*expanded);
                match &children[0] {
                    TreeNode::Folder { expanded, .. } => assert!(*expanded),
                    _ => panic!("expected subfolder"),
                }
            }
            _ => panic!("expected folder"),
        }
    }

    #[test]
    fn move_node_to_position_reorders_root_nodes() {
        let mut tree = build_tree(&[
            w("@1", 1, "proj:edit"),
            w("@2", 2, "scratch"),
            w("@3", 3, "proj:term"),
            w("@4", 4, "other:main"),
        ]);

        move_node_to_position(&mut tree, &[1], 1).unwrap();

        assert_eq!(window_ids_in_order(&tree), vec!["@2", "@1", "@3", "@4"]);
    }

    #[test]
    fn move_node_to_position_reorders_nested_nodes() {
        let mut tree = build_tree(&[
            w("@1", 1, "proj:edit"),
            w("@2", 2, "scratch"),
            w("@3", 3, "proj:term"),
        ]);

        move_node_to_position(&mut tree, &[0, 0], 2).unwrap();

        assert_eq!(window_ids_in_order(&tree), vec!["@3", "@1", "@2"]);
    }

    fn window_name(node: &TreeNode) -> &str {
        match node {
            TreeNode::Window { info } => &info.name,
            _ => panic!("expected window"),
        }
    }
}
