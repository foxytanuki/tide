use tracing::debug;

use crate::model::Model;
use crate::tree::{
    find_parent_folder, folder_path_for_path, get_node, join_folder_path, FlatNodeKind, TreeNode,
};

pub(super) fn reconstruct_full_name(model: &Model, flat_idx: usize, leaf_name: &str) -> String {
    let mut parts = vec![leaf_name.to_string()];
    let mut idx = flat_idx;
    while let Some(parent_idx) = find_parent_folder(model.flat_items(), idx) {
        if let Some(parent_item) = model.flat_items().get(parent_idx) {
            if let Ok(TreeNode::Folder { name, .. }) = get_node(model.tree(), &parent_item.path) {
                parts.push(name.clone());
            }
        }
        idx = parent_idx;
    }
    parts.reverse();
    parts.join(":")
}

pub(super) fn reconstruct_folder_full_name(model: &Model, path: &[usize]) -> String {
    folder_path_for_path(model.tree(), path).unwrap_or_default()
}

pub(super) fn collect_window_names(nodes: &[TreeNode]) -> Vec<String> {
    collect_window_names_inner(nodes, None)
}

fn collect_window_names_inner(nodes: &[TreeNode], prefix: Option<&str>) -> Vec<String> {
    let mut names = Vec::new();
    for node in nodes {
        match node {
            TreeNode::Window { info } => {
                let name = match prefix {
                    Some(p) => format!("{}:{}", p, info.name),
                    None => info.name.clone(),
                };
                names.push(name);
            }
            TreeNode::Folder { name, children, .. } => {
                let full_prefix = join_folder_path(prefix, name);
                names.extend(collect_window_names_inner(children, Some(&full_prefix)));
            }
        }
    }
    names
}

fn next_tab_name(existing_names: &[String], folder: Option<&str>) -> String {
    for i in 1.. {
        let candidate = match folder {
            Some(f) => format!("{}:tab{}", f, i),
            None => format!("tab{}", i),
        };
        if !existing_names.iter().any(|n| n == &candidate) {
            return candidate;
        }
    }
    unreachable!()
}

pub(super) fn determine_new_window_name(model: &Model) -> String {
    let existing = collect_window_names(model.tree());

    let item = match model.flat_items().get(model.cursor()) {
        Some(item) => item,
        None => return next_tab_name(&existing, None),
    };

    if let Some(selected) = model.selected_window_info() {
        if let Some(pending) = model.renames.pending.get(selected.id.as_str()) {
            if let Some((folder, _)) = pending.target_name.rsplit_once(':') {
                let generated = next_tab_name(&existing, Some(folder));
                debug!(
                    cursor = model.cursor(),
                    generated, "new window name from pending rename context"
                );
                return generated;
            }
        }
    }

    match item.kind {
        FlatNodeKind::Folder => {
            let folder_full = reconstruct_folder_full_name(model, &item.path);
            if !folder_full.is_empty() {
                let generated = next_tab_name(&existing, Some(&folder_full));
                debug!(
                    cursor = model.cursor(),
                    generated, "new window name from folder"
                );
                generated
            } else {
                debug!(cursor = model.cursor(), "new window name fallback");
                next_tab_name(&existing, None)
            }
        }
        FlatNodeKind::Window => {
            if let Some(parent_idx) = find_parent_folder(model.flat_items(), model.cursor()) {
                if let Some(parent_item) = model.flat_items().get(parent_idx) {
                    let parent_full = reconstruct_folder_full_name(model, &parent_item.path);
                    if !parent_full.is_empty() {
                        let generated = next_tab_name(&existing, Some(&parent_full));
                        debug!(
                            cursor = model.cursor(),
                            generated, "new window name from parent folder"
                        );
                        return generated;
                    }
                }
            }
            debug!(
                cursor = model.cursor(),
                "new window name fallback from window w/o folder"
            );
            next_tab_name(&existing, None)
        }
    }
}

pub(super) fn collect_folder_children(
    tree: &[TreeNode],
    folder_path: &str,
) -> Vec<(String, String)> {
    if let Some(TreeNode::Folder { children, .. }) = find_folder_by_path(tree, folder_path) {
        let mut result = Vec::new();
        collect_folder_children_recursive(children, None, &mut result);
        return result;
    }
    Vec::new()
}

fn find_folder_by_path<'a>(tree: &'a [TreeNode], path: &str) -> Option<&'a TreeNode> {
    let parts: Vec<&str> = path.split(':').collect();
    let mut nodes = tree;
    let mut target: Option<&TreeNode> = None;
    for part in &parts {
        let mut found = false;
        for node in nodes {
            if let TreeNode::Folder { name, children, .. } = node {
                if name == part {
                    target = Some(node);
                    nodes = children;
                    found = true;
                    break;
                }
            }
        }
        if !found {
            return None;
        }
    }
    target
}

fn collect_folder_children_recursive(
    nodes: &[TreeNode],
    prefix: Option<&str>,
    out: &mut Vec<(String, String)>,
) {
    for node in nodes {
        match node {
            TreeNode::Window { info } => {
                let suffix = match prefix {
                    Some(p) => format!("{}:{}", p, info.name),
                    None => info.name.clone(),
                };
                out.push((info.id.clone(), suffix));
            }
            TreeNode::Folder { name, children, .. } => {
                let new_prefix = join_folder_path(prefix, name);
                collect_folder_children_recursive(children, Some(&new_prefix), out);
            }
        }
    }
}
