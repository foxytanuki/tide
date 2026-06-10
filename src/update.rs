use crate::cmd::Cmd;
use crate::model::{Mode, Model};
use crate::msg::Msg;

mod ai;
mod input;
mod naming;
mod navigation;
mod window_list;

use ai::*;
use input::*;
use naming::*;
use navigation::*;
use window_list::*;

const MISSING_PENDING_RENAME_THRESHOLD: u8 = 6;
const STABLE_PENDING_RENAME_THRESHOLD: u8 = 2;

pub fn update(model: &mut Model, msg: Msg) -> Vec<Cmd> {
    // Clear error on any user action (exclude background events)
    if !matches!(
        msg,
        Msg::WindowChanged
            | Msg::WindowRenamed { .. }
            | Msg::WindowListLoaded(_)
            | Msg::WindowFocusChanged(_)
            | Msg::AiProcessPollResult { .. }
    ) {
        model.error_message = None;
        model.info_message = None;
    }

    match msg {
        Msg::CursorUp => handle_cursor_up(model),
        Msg::CursorDown => handle_cursor_down(model),
        Msg::JumpToVisibleIndex(n) => handle_jump_to_index(model, n),
        Msg::SelectItem => handle_select_item(model),
        Msg::CollapseOrParent => handle_collapse_or_parent(model),
        Msg::ToggleFolder => handle_toggle_folder(model),
        Msg::Escape => vec![Cmd::FocusRightPane],
        Msg::NewWindow => {
            let name = determine_new_window_name(model);
            vec![Cmd::NewWindow { name }]
        }
        Msg::NewProject => {
            clear_input(model);
            model.mode = Mode::CreatingProject;
            vec![Cmd::Render]
        }
        Msg::MoveItem => handle_move_item(model),
        Msg::MoveProject => handle_move_project(model),
        Msg::RenameWindow => handle_rename_window(model),
        Msg::CloseWindow => handle_close_window(model),
        Msg::ApplyLayoutHelper => vec![Cmd::ApplyLayoutHelper],
        Msg::WindowFocusChanged(window_id) => handle_window_focus_changed(model, window_id),
        Msg::WindowChanged => vec![Cmd::Resync],
        Msg::WindowRenamed { window_id, name } => handle_window_renamed(model, window_id, name),
        Msg::WindowListLoaded(windows) => handle_window_list_loaded(model, windows),
        Msg::AiProcessPollResult { panes, windows } => {
            handle_ai_process_poll_result(model, panes, windows)
        }
        Msg::MouseClick { row } => {
            if !matches!(model.mode, Mode::Normal) {
                return vec![];
            }
            match model.item_at_row(row) {
                Some(index) => handle_mouse_click(model, index),
                None => vec![],
            }
        }
        Msg::MouseScrollUp => {
            if !matches!(model.mode, Mode::Normal) {
                return vec![];
            }
            handle_cursor_up(model)
        }
        Msg::MouseScrollDown => {
            if !matches!(model.mode, Mode::Normal) {
                return vec![];
            }
            handle_cursor_down(model)
        }
        Msg::Key(event) => handle_key(model, event),
        Msg::Restart => {
            model.should_quit = true;
            model.restart_requested = true;
            vec![Cmd::ResetAllBorders, Cmd::RestorePreview, Cmd::Restart]
        }
        Msg::Quit => {
            model.should_quit = true;
            vec![Cmd::ResetAllBorders, Cmd::RestorePreview, Cmd::Quit]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MoveSubject, PendingRename, PreviewState, SelectionTarget};
    use crate::tree::{
        build_tree, find_folder_flat_index_by_path, find_window_flat_index_by_id, get_node,
        window_ids_in_order, TreeNode, WindowInfo,
    };
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

    fn assert_follow_window(cmds: &[Cmd], window_id: &str) {
        assert_eq!(cmds.len(), 3);
        assert!(matches!(
            &cmds[0],
            Cmd::FollowToWindow { window_id: id } if id == window_id
        ));
        assert!(matches!(cmds[1], Cmd::EnsureSidebarWidth));
        assert!(matches!(cmds[2], Cmd::Resync));
    }

    fn assert_ensure_only(cmds: &[Cmd]) {
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::EnsureSidebarWidth));
    }

    #[test]
    fn unexpected_focus_change_is_not_swallowed_during_suppression() {
        let mut model = test_model();
        model.sidebar.preview = PreviewState::Previewing {
            original_window_id: "@home".to_string(),
            original_home_pane_id: "%home".to_string(),
        };
        model.sidebar.ignore_window_changes = 2;
        model.sidebar.pending_internal_focus_window = Some("@internal".to_string());

        let cmds = update(&mut model, Msg::WindowFocusChanged("@user".to_string()));

        assert_eq!(model.sidebar.preview, PreviewState::Home);
        assert_eq!(model.sidebar.ignore_window_changes, 0);
        assert_eq!(model.sidebar.pending_internal_focus_window, None);
        assert_follow_window(&cmds, "@user");
    }

    #[test]
    fn window_change_requests_single_resync_each_time() {
        let mut model = test_model();

        let first = update(&mut model, Msg::WindowChanged);
        let second = update(&mut model, Msg::WindowChanged);

        assert_eq!(first.len(), 1);
        assert!(matches!(first[0], Cmd::Resync));
        assert_eq!(second.len(), 1);
        assert!(matches!(second[0], Cmd::Resync));
    }

    #[test]
    fn burst_of_non_plain_keys_is_dropped_without_rendering() {
        let mut model = test_model();

        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
        );

        assert!(cmds.is_empty());
        assert_eq!(model.mode, Mode::Normal);
    }

    fn wi(id: &str, index: usize, name: &str) -> WindowInfo {
        WindowInfo {
            id: id.to_string(),
            index,
            name: name.to_string(),
            active: false,
        }
    }

    fn active_wi(id: &str, index: usize, name: &str) -> WindowInfo {
        WindowInfo {
            id: id.to_string(),
            index,
            name: name.to_string(),
            active: true,
        }
    }

    fn assert_new_window_name(cmds: &[Cmd], expected: &str) {
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            Cmd::NewWindow { name } => assert_eq!(name, expected),
            other => panic!("expected NewWindow, got {:?}", other),
        }
    }

    #[test]
    fn new_window_on_folder_gets_prefix() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "proj:edit"), wi("@2", 2, "proj:term")];
        update(&mut model, Msg::WindowListLoaded(windows));
        // expanded folder is skipped, cursor lands on first child
        assert_eq!(model.cursor(), 1);
        // collapse the folder so cursor can sit on it
        model.set_cursor(0);
        model.mutate_tree(|tree| {
            if let TreeNode::Folder { expanded, .. } = &mut tree[0] {
                *expanded = false;
            }
        });
        // after collapse + rebuild, cursor stays on the now-collapsed folder
        assert_eq!(model.cursor(), 0);
        let cmds = update(&mut model, Msg::NewWindow);
        assert_new_window_name(&cmds, "proj:tab1");
    }

    #[test]
    fn new_window_on_child_gets_prefix() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "proj:edit"), wi("@2", 2, "proj:term")];
        update(&mut model, Msg::WindowListLoaded(windows));
        // move cursor to child window (index 1 = proj/edit)
        model.set_cursor(1);
        let cmds = update(&mut model, Msg::NewWindow);
        assert_new_window_name(&cmds, "proj:tab1");
    }

    #[test]
    fn new_window_on_root_window_is_plain() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "proj:edit"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(windows));
        // flat: [0]=folder "proj", [1]=window "edit", [2]=window "scratch"
        model.set_cursor(2);
        let cmds = update(&mut model, Msg::NewWindow);
        assert_new_window_name(&cmds, "tab1");
    }

    #[test]
    fn collect_window_names_preserves_nested_folder_paths() {
        let windows = vec![
            wi("@1", 1, "proj:sub:edit"),
            wi("@2", 2, "proj:sub:term"),
            wi("@3", 3, "scratch"),
        ];
        let tree = build_tree(&windows);
        let names = collect_window_names(&tree);
        assert!(names.iter().any(|n| n == "proj:sub:edit"));
        assert!(names.iter().any(|n| n == "proj:sub:term"));
        assert!(names.iter().any(|n| n == "scratch"));
    }

    #[test]
    fn new_window_name_uses_full_pending_nested_folder_path() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "proj:sub:tab1"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(windows));
        // flat: [0]=proj [1]=sub [2]=tab1 [3]=scratch
        model.set_cursor(2);
        model.renames.pending.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "proj:sub:renamed".to_string(),
                observed_count: 0,
            },
        );

        let cmds = update(&mut model, Msg::NewWindow);
        assert_new_window_name(&cmds, "proj:sub:tab2");
    }

    #[test]
    fn renaming_nested_window_to_same_name_is_noop() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "proj:sub:edit"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(windows));
        model.set_cursor(2);
        model.mode = Mode::Renaming {
            window_id: "@1".to_string(),
        };
        model.input_buffer = "proj:sub:edit".to_string();
        model.input_cursor = model.input_buffer.chars().count();

        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert_eq!(model.mode, Mode::Normal);
        assert!(model.renames.pending.is_empty());
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Render));
    }

    #[test]
    fn expected_focus_change_clears_suppression() {
        let mut model = test_model();
        model.sidebar.ignore_window_changes = 2;
        model.sidebar.pending_internal_focus_window = Some("@target".to_string());

        let cmds = update(&mut model, Msg::WindowFocusChanged("@target".to_string()));
        assert_ensure_only(&cmds);
        assert_eq!(model.sidebar.ignore_window_changes, 0);
        assert_eq!(model.sidebar.pending_internal_focus_window, None);
    }

    #[test]
    fn window_list_loaded_restores_cursor_by_window_id() {
        let mut model = test_model();
        let before = vec![
            wi("@1", 1, "root-a"),
            wi("@2", 2, "scratch"),
            wi("@3", 3, "root-b"),
        ];
        update(&mut model, Msg::WindowListLoaded(before));

        // Select window @2 in the flat list.
        model.set_cursor(1);

        let after = vec![
            wi("@1", 1, "root-a"),
            wi("@2", 2, "proj:new"),
            wi("@3", 3, "root-b"),
        ];
        update(&mut model, Msg::WindowListLoaded(after));

        assert_eq!(model.cursor(), 2);
        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@2")
        );
    }

    #[test]
    fn window_list_loaded_defaults_cursor_to_sidebar_host_window() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "root-a"), active_wi("@home", 2, "root-b")];

        update(&mut model, Msg::WindowListLoaded(windows));

        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@home")
        );
        assert_eq!(model.cursor(), 1);
    }

    #[test]
    fn window_list_loaded_preserves_renaming_mode_when_target_exists() {
        let mut model = test_model();
        let before = vec![wi("@1", 1, "dev"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(before));

        // Cursor can drift for any reason (e.g. async refresh timing), so keep a stale cursor.
        model.set_cursor(1);
        model.mode = Mode::Renaming {
            window_id: "@1".to_string(),
        };

        let after = vec![wi("@1", 1, "new:new"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(after));

        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@1")
        );
        assert_eq!(model.cursor(), 1);
        // Renaming mode preserved: target window @1 still exists
        assert!(matches!(model.mode, Mode::Renaming { .. }));
    }

    #[test]
    fn window_list_loaded_cancels_renaming_mode_when_target_gone() {
        let mut model = test_model();
        let before = vec![wi("@1", 1, "dev"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(before));

        model.mode = Mode::Renaming {
            window_id: "@1".to_string(),
        };

        // @1 is gone from the new list
        let after = vec![wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(after));

        assert_eq!(model.mode, Mode::Normal);
    }

    #[test]
    fn window_list_loaded_prefers_pending_rename_id_over_cursor() {
        let mut model = test_model();
        let before = vec![wi("@1", 1, "dev"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(before));
        model.set_cursor(2);
        model.renames.pending.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "dev:new".to_string(),
                observed_count: 0,
            },
        );
        model.renames.last_window_id = Some("@1".to_string());

        let after = vec![wi("@1", 1, "new:new"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(after));

        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@1")
        );
        assert_eq!(model.cursor(), 1);
        assert!(model.renames.pending.contains_key("@1"));
    }

    #[test]
    fn window_list_loaded_does_not_let_stable_pending_rename_hijack_selection() {
        let mut model = test_model();
        let before = vec![wi("@1", 1, "dev:new"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(before));
        model.set_cursor(2);
        model.renames.pending.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "dev:new".to_string(),
                observed_count: 1,
            },
        );
        model.renames.last_window_id = Some("@1".to_string());

        let after = vec![wi("@1", 1, "dev:new"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(after));

        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@2")
        );
        assert_eq!(model.cursor(), 2);
        assert_eq!(model.renames.last_window_id, None);
    }

    #[test]
    fn new_window_name_uses_pending_rename_folder_when_context_collapses() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "new:old"), wi("@2", 2, "dev")];
        update(&mut model, Msg::WindowListLoaded(windows));
        model.set_cursor(1);
        model.renames.pending.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 0,
            },
        );

        let cmds = update(&mut model, Msg::NewWindow);
        assert_new_window_name(&cmds, "new:tab1");
    }

    #[test]
    fn window_list_loaded_keeps_pending_rename_if_target_missing_once() {
        let mut model = test_model();
        let initial = vec![wi("@1", 1, "new:new"), wi("@2", 2, "dev")];
        update(&mut model, Msg::WindowListLoaded(initial));
        model.renames.pending.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 1,
            },
        );
        model.renames.last_window_id = Some("@1".to_string());

        let missing = vec![wi("@2", 2, "dev"), wi("@3", 3, "scratch")];
        update(&mut model, Msg::WindowListLoaded(missing));

        assert!(model.renames.pending.contains_key("@1"));
    }

    #[test]
    fn window_list_loaded_clears_pending_rename_after_repeated_missing_updates() {
        let mut model = test_model();
        let initial = vec![wi("@1", 1, "new:new")];
        update(&mut model, Msg::WindowListLoaded(initial));
        model.renames.pending.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 1,
            },
        );
        model.renames.last_window_id = Some("@1".to_string());

        for _ in 0..5 {
            let missing = vec![wi("@2", 2, "dev")];
            update(&mut model, Msg::WindowListLoaded(missing));
        }

        assert!(!model.renames.pending.contains_key("@1"));
        assert_eq!(model.renames.last_window_id, None);
    }

    #[test]
    fn window_list_loaded_clears_pending_rename_when_target_name_stable() {
        let mut model = test_model();
        let initial = vec![wi("@1", 1, "new:new"), wi("@2", 2, "dev")];
        update(&mut model, Msg::WindowListLoaded(initial));
        model.renames.pending.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 5,
            },
        );
        model.renames.last_window_id = Some("@1".to_string());

        let stable = vec![wi("@1", 1, "new:new"), wi("@2", 2, "dev")];
        update(&mut model, Msg::WindowListLoaded(stable));

        assert!(!model.renames.pending.contains_key("@1"));
        assert_eq!(model.renames.last_window_id, None);
    }

    #[test]
    fn window_list_loaded_sorts_windows_by_index_before_building_tree() {
        let mut model = test_model();
        let windows = vec![wi("@2", 2, "beta"), wi("@1", 1, "alpha")];

        update(&mut model, Msg::WindowListLoaded(windows));

        match &model.tree()[0] {
            TreeNode::Window { info } => assert_eq!(info.id, "@1"),
            other => panic!("expected first root window, got {:?}", other),
        }
        match &model.tree()[1] {
            TreeNode::Window { info } => assert_eq!(info.id, "@2"),
            other => panic!("expected second root window, got {:?}", other),
        }
    }

    #[test]
    fn window_list_loaded_skips_rebuild_for_same_list() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "proj:edit"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(windows.clone()));

        model.mutate_tree(|tree| {
            if let TreeNode::Folder { expanded, .. } = &mut tree[0] {
                *expanded = false;
            }
        });
        model.set_cursor(0);

        let cmds = update(&mut model, Msg::WindowListLoaded(windows));

        assert_eq!(model.cursor(), 0);
        assert!(matches!(
            model.tree()[0],
            TreeNode::Folder {
                expanded: false,
                ..
            }
        ));
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Render));
    }

    #[test]
    fn window_list_loaded_rename_only_preserves_folder_expansion() {
        let mut model = test_model();
        let before = vec![wi("@1", 1, "proj:edit"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(before));

        model.mutate_tree(|tree| {
            if let TreeNode::Folder { expanded, .. } = &mut tree[0] {
                *expanded = false;
            }
        });
        model.set_cursor(0);

        let after = vec![wi("@1", 1, "proj:new"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(after));

        assert_eq!(model.cursor(), 0);
        assert!(matches!(
            model.tree()[0],
            TreeNode::Folder {
                expanded: false,
                ..
            }
        ));
    }

    #[test]
    fn window_list_loaded_rename_only_updates_leaf_without_rebuild() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@1", 1, "proj:edit"), wi("@2", 2, "scratch")]),
        );

        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@1", 1, "proj:renamed"), wi("@2", 2, "scratch")]),
        );

        match &model.tree()[0] {
            TreeNode::Folder { children, .. } => match &children[0] {
                TreeNode::Window { info } => assert_eq!(info.name, "renamed"),
                other => panic!("expected folder child window, got {:?}", other),
            },
            other => panic!("expected root folder, got {:?}", other),
        }
    }

    #[test]
    fn window_list_loaded_add_only_fast_path_inserts_single_window() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@1", 1, "alpha"), wi("@3", 3, "gamma")]),
        );

        let cmds = update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@1", 1, "alpha"),
                wi("@2", 2, "beta"),
                wi("@3", 3, "gamma"),
            ]),
        );

        assert_eq!(model.tree().len(), 3);
        assert_eq!(model.cursor(), 0);
        assert!(cmds.iter().any(|cmd| matches!(cmd, Cmd::Render)));
    }

    #[test]
    fn window_list_loaded_remove_only_fast_path_removes_single_window() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@1", 1, "alpha"),
                wi("@2", 2, "beta"),
                wi("@3", 3, "gamma"),
            ]),
        );

        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@1", 1, "alpha"), wi("@3", 3, "gamma")]),
        );

        assert_eq!(model.tree().len(), 2);
        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@1")
        );
    }

    #[test]
    fn window_list_loaded_rebuilds_same_list_while_moving() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "proj:edit"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(windows.clone()));

        model.mutate_tree(|tree| {
            tree.push(TreeNode::Window {
                info: wi("@extra", 99, "extra"),
            });
        });
        model.set_cursor(0);
        model.mode = Mode::Moving {
            subject: crate::model::MoveSubject::Item(crate::model::SelectionTarget::Window(
                "@1".to_string(),
            )),
        };

        let cmds = update(&mut model, Msg::WindowListLoaded(windows));

        assert!(matches!(model.mode, Mode::Normal));
        assert_eq!(model.cursor(), 0);
        assert_eq!(model.tree().len(), 2);
        assert!(model.tree().iter().all(|node| match node {
            TreeNode::Window { info } => info.id != "@extra",
            TreeNode::Folder { .. } => true,
        }));
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Render));
    }

    #[test]
    fn window_list_loaded_add_remove_keeps_selected_window_by_id() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@1", 1, "alpha"), wi("@2", 2, "beta")]),
        );
        model.set_cursor(1);

        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@3", 1, "gamma"), wi("@2", 2, "beta")]),
        );

        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@2")
        );
        assert_eq!(model.cursor(), 1);
    }

    #[test]
    fn window_list_loaded_add_remove_fast_path_handles_root_level_addition() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@1", 1, "alpha"), wi("@3", 3, "gamma")]),
        );
        model.set_cursor(1);

        let cmds = update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@1", 1, "alpha"),
                wi("@2", 2, "beta"),
                wi("@3", 3, "gamma"),
            ]),
        );

        assert_eq!(model.cursor(), 2);
        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@3")
        );
        assert_eq!(model.tree().len(), 3);
        assert!(cmds.iter().any(|cmd| matches!(cmd, Cmd::Render)));
    }

    #[test]
    fn window_list_loaded_add_remove_fast_path_handles_root_level_removal() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@1", 1, "alpha"),
                wi("@2", 2, "beta"),
                wi("@3", 3, "gamma"),
            ]),
        );
        model.set_cursor(1);

        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@1", 1, "alpha"), wi("@3", 3, "gamma")]),
        );

        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@3")
        );
        assert_eq!(model.cursor(), 1);
        assert_eq!(model.tree().len(), 2);
    }

    #[test]
    fn window_list_loaded_add_remove_fast_path_handles_nested_folder_tab() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@1", 1, "proj:edit")]),
        );

        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@1", 1, "proj:edit"), wi("@2", 2, "proj:tab")]),
        );

        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@1")
        );
        assert_eq!(model.tree().len(), 1);
        match &model.tree()[0] {
            TreeNode::Folder { children, .. } => assert_eq!(children.len(), 2),
            other => panic!("expected folder, got {:?}", other),
        }

        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@2", 2, "proj:tab")]),
        );

        assert!(matches!(model.tree()[0], TreeNode::Folder { .. }));
        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@2")
        );
    }

    #[test]
    fn window_list_loaded_add_remove_fast_path_handles_nested_folder_api_tab() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@1", 1, "proj:api:edit")]),
        );

        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@1", 1, "proj:api:edit"),
                wi("@2", 2, "proj:api:tab"),
            ]),
        );

        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@1")
        );
        assert_eq!(model.tree().len(), 1);
        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@2", 2, "proj:api:tab")]),
        );
        assert_eq!(
            model.selected_window_info().map(|w| w.id.as_str()),
            Some("@2")
        );
    }

    #[test]
    fn window_renamed_pending_mismatch_triggers_immediate_rename() {
        let mut model = test_model();
        model.renames.pending.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 3,
            },
        );

        let cmds = update(
            &mut model,
            Msg::WindowRenamed {
                window_id: "@1".to_string(),
                name: "dev".to_string(),
            },
        );

        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            &cmds[0],
            Cmd::RenameWindow { id, name } if id == "@1" && name == "new:new"
        ));
        assert_eq!(
            model.renames.pending.get("@1").map(|p| p.observed_count),
            Some(4)
        );
    }

    #[test]
    fn window_renamed_pending_match_does_not_trigger_refresh() {
        let mut model = test_model();
        model.renames.pending.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 3,
            },
        );
        model.renames.last_window_id = Some("@1".to_string());

        let cmds = update(
            &mut model,
            Msg::WindowRenamed {
                window_id: "@1".to_string(),
                name: "new:new".to_string(),
            },
        );

        assert!(cmds.is_empty());
        assert!(!model.renames.pending.contains_key("@1"));
        assert_eq!(model.renames.last_window_id, None);
    }

    #[test]
    fn window_renamed_pending_match_waits_for_stable_observation() {
        let mut model = test_model();
        model.renames.pending.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 0,
            },
        );
        model.renames.last_window_id = Some("@1".to_string());

        let cmds = update(
            &mut model,
            Msg::WindowRenamed {
                window_id: "@1".to_string(),
                name: "new:new".to_string(),
            },
        );

        assert!(cmds.is_empty());
        assert_eq!(
            model.renames.pending.get("@1").map(|p| p.observed_count),
            Some(1)
        );
        assert_eq!(model.renames.last_window_id.as_deref(), Some("@1"));
    }

    #[test]
    fn window_renamed_pending_mismatch_clears_after_threshold() {
        let mut model = test_model();
        model.renames.pending.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: MISSING_PENDING_RENAME_THRESHOLD - 1,
            },
        );

        let cmds = update(
            &mut model,
            Msg::WindowRenamed {
                window_id: "@1".to_string(),
                name: "dev".to_string(),
            },
        );

        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Resync));
        assert!(!model.renames.pending.contains_key("@1"));
        assert_eq!(model.renames.last_window_id, None);
    }

    #[test]
    fn window_renamed_non_pending_triggers_refresh() {
        let mut model = test_model();
        let cmds = update(
            &mut model,
            Msg::WindowRenamed {
                window_id: "@2".to_string(),
                name: "dev".to_string(),
            },
        );

        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Resync));
    }

    #[test]
    fn window_list_loaded_corrects_all_pending_renames() {
        let mut model = test_model();
        model.renames.pending.insert(
            "@1".to_string(),
            PendingRename {
                target_name: "new:new".to_string(),
                observed_count: 0,
            },
        );
        model.renames.pending.insert(
            "@2".to_string(),
            PendingRename {
                target_name: "app:main".to_string(),
                observed_count: 0,
            },
        );
        model.renames.last_window_id = Some("@2".to_string());

        let cmds = update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@1", 1, "dev"), wi("@2", 2, "app:dev")]),
        );

        let rename_count = cmds
            .iter()
            .filter(|cmd| matches!(cmd, Cmd::RenameWindow { .. }))
            .count();
        assert_eq!(rename_count, 2);
        assert!(cmds.iter().any(|cmd| {
            matches!(
                cmd,
                Cmd::RenameWindow { id, name } if id == "@1" && name == "new:new"
            )
        }));
        assert!(cmds.iter().any(|cmd| {
            matches!(
                cmd,
                Cmd::RenameWindow { id, name } if id == "@2" && name == "app:main"
            )
        }));
        assert!(matches!(cmds.last(), Some(Cmd::Render)));
    }

    // --- JumpToVisibleIndex tests ---

    #[test]
    fn jump_to_index_1_lands_on_first_navigable_item() {
        let mut model = test_model();
        // flat: [0]=root-a, [1]=scratch, [2]=root-b (all plain windows)
        let windows = vec![
            wi("@1", 1, "root-a"),
            wi("@2", 2, "scratch"),
            wi("@3", 3, "root-b"),
        ];
        update(&mut model, Msg::WindowListLoaded(windows));
        model.set_cursor(2);

        update(&mut model, Msg::JumpToVisibleIndex(1));
        assert_eq!(model.cursor(), 0);
    }

    #[test]
    fn jump_to_index_0_is_noop() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "root-a"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(windows));
        model.set_cursor(0);

        update(&mut model, Msg::JumpToVisibleIndex(0));
        assert_eq!(model.cursor(), 0);
    }

    #[test]
    fn jump_to_index_out_of_range_is_noop() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "root-a"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(windows));
        model.set_cursor(0);

        update(&mut model, Msg::JumpToVisibleIndex(9));
        assert_eq!(model.cursor(), 0);
    }

    #[test]
    fn jump_skips_expanded_folder_header() {
        let mut model = test_model();
        // "proj:edit" and "proj:term" create an expanded folder at flat[0],
        // with children at flat[1] and flat[2].
        // visible list: [flat[1]=edit, flat[2]=term]  (flat[0] folder is expanded → skipped)
        let windows = vec![wi("@1", 1, "proj:edit"), wi("@2", 2, "proj:term")];
        update(&mut model, Msg::WindowListLoaded(windows));
        // cursor starts on flat[1] (first child) after load
        assert_eq!(model.cursor(), 1);

        // Jump to visible index 1 → should still be flat[1] (first non-folder row)
        update(&mut model, Msg::JumpToVisibleIndex(1));
        assert_eq!(model.cursor(), 1);

        // Jump to visible index 2 → flat[2]
        update(&mut model, Msg::JumpToVisibleIndex(2));
        assert_eq!(model.cursor(), 2);
    }

    #[test]
    fn jump_includes_collapsed_folder() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "proj:edit"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(windows));
        // Collapse the folder so flat[0] is a collapsed folder header
        model.set_cursor(0);
        model.mutate_tree(|tree| {
            if let TreeNode::Folder { expanded, .. } = &mut tree[0] {
                *expanded = false;
            }
        });

        // flat: [0]=collapsed-folder "proj", [1]=window "scratch"
        // visible: [flat[0], flat[1]]
        update(&mut model, Msg::JumpToVisibleIndex(1));
        assert_eq!(model.cursor(), 0); // collapsed folder is navigable

        update(&mut model, Msg::JumpToVisibleIndex(2));
        assert_eq!(model.cursor(), 1);
    }

    #[test]
    fn jump_digit_key_does_not_trigger_in_rename_mode() {
        let mut model = test_model();
        let windows = vec![wi("@1", 1, "dev"), wi("@2", 2, "scratch")];
        update(&mut model, Msg::WindowListLoaded(windows));
        model.set_cursor(0);
        model.mode = Mode::Renaming {
            window_id: "@1".to_string(),
        };
        model.input_buffer.clear();
        model.input_cursor = 0;

        // Pressing '3' in rename mode should insert into input_buffer, not jump
        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)),
        );
        assert_eq!(model.cursor(), 0); // cursor unchanged
        assert_eq!(model.input_buffer, "3"); // character inserted into buffer
    }

    #[test]
    fn move_item_key_enters_move_mode() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@1", 1, "proj:edit"), wi("@2", 2, "proj:term")]),
        );

        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
        );

        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Render));
        assert!(matches!(
            model.mode,
            Mode::Moving {
                subject: MoveSubject::Item(SelectionTarget::Window(ref id))
            } if id == "@1"
        ));
    }

    #[test]
    fn move_item_enter_reorders_within_project_scope() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@1", 1, "proj:edit"),
                wi("@2", 2, "scratch"),
                wi("@3", 3, "proj:term"),
            ]),
        );

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
        );
        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert_eq!(model.mode, Mode::Normal);
        assert_eq!(
            model.reorder.pending_selection,
            Some(SelectionTarget::Window("@1".to_string()))
        );
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            Cmd::ReorderWindows { order, selection } => {
                assert_eq!(
                    order,
                    &vec!["@3".to_string(), "@1".to_string(), "@2".to_string()]
                );
                assert_eq!(selection, &SelectionTarget::Window("@1".to_string()));
            }
            other => panic!("expected ReorderWindows, got {:?}", other),
        }
    }

    #[test]
    fn move_item_enter_excludes_sidebar_host_from_reorder_order() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@home", 1, "sidebar"),
                wi("@1", 2, "alpha"),
                wi("@2", 3, "beta"),
            ]),
        );

        let alpha_index = find_window_flat_index_by_id(model.flat_items(), model.tree(), "@1")
            .expect("alpha should be visible");
        model.set_cursor(alpha_index);

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
        );
        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        match &cmds[0] {
            Cmd::ReorderWindows { order, selection } => {
                assert_eq!(order, &vec!["@2".to_string(), "@1".to_string()]);
                assert_eq!(selection, &SelectionTarget::Window("@1".to_string()));
            }
            other => panic!("expected ReorderWindows, got {:?}", other),
        }
    }

    #[test]
    fn move_item_across_sidebar_host_restores_preview_on_reject() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@home", 1, "sidebar"),
                wi("@1", 2, "alpha"),
                wi("@2", 3, "beta"),
            ]),
        );
        let before = window_ids_in_order(model.tree());

        let alpha_index = find_window_flat_index_by_id(model.flat_items(), model.tree(), "@1")
            .expect("alpha should be visible");
        model.set_cursor(alpha_index);

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
        );
        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        );
        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Render));
        assert_eq!(model.mode, Mode::Normal);
        assert_eq!(
            model.info_message.as_deref(),
            Some("cannot move across sidebar host")
        );
        assert_eq!(window_ids_in_order(model.tree()), before);
        assert!(model.reorder.snapshot.is_none());
    }

    #[test]
    fn move_item_across_host_containing_folder_is_rejected() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@home", 1, "proj:alpha"),
                wi("@1", 2, "proj:beta"),
                wi("@2", 3, "notes"),
            ]),
        );
        let before = window_ids_in_order(model.tree());

        let notes_index = find_window_flat_index_by_id(model.flat_items(), model.tree(), "@2")
            .expect("notes should be visible");
        model.set_cursor(notes_index);

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
        );
        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        );
        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Render));
        assert_eq!(model.mode, Mode::Normal);
        assert_eq!(
            model.info_message.as_deref(),
            Some("cannot move across sidebar host")
        );
        assert_eq!(window_ids_in_order(model.tree()), before);
        assert!(model.reorder.snapshot.is_none());
    }

    #[test]
    fn move_project_across_sidebar_host_is_rejected() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@home", 1, "proj:alpha"),
                wi("@1", 2, "proj:beta"),
                wi("@2", 3, "other:main"),
            ]),
        );
        let before = window_ids_in_order(model.tree());

        let other_index = find_window_flat_index_by_id(model.flat_items(), model.tree(), "@2")
            .expect("other:main should be visible");
        model.set_cursor(other_index);

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT)),
        );
        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        );
        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Render));
        assert_eq!(model.mode, Mode::Normal);
        assert_eq!(
            model.info_message.as_deref(),
            Some("cannot move across sidebar host")
        );
        assert_eq!(window_ids_in_order(model.tree()), before);
        assert!(model.reorder.snapshot.is_none());
    }

    #[test]
    fn move_project_key_from_child_targets_root_project() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@1", 1, "proj:edit"),
                wi("@2", 2, "scratch"),
                wi("@3", 3, "proj:term"),
                wi("@4", 4, "other:main"),
            ]),
        );

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT)),
        );
        assert_eq!(model.cursor(), 0);
        assert!(matches!(
            model.mode,
            Mode::Moving {
                subject: MoveSubject::Project { ref folder_name }
            } if folder_name == "proj"
        ));

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert_eq!(
            model.reorder.pending_selection,
            Some(SelectionTarget::Folder("proj".to_string()))
        );
        match &cmds[0] {
            Cmd::ReorderWindows { order, selection } => {
                assert_eq!(
                    order,
                    &vec![
                        "@2".to_string(),
                        "@1".to_string(),
                        "@3".to_string(),
                        "@4".to_string()
                    ]
                );
                assert_eq!(selection, &SelectionTarget::Folder("proj".to_string()));
            }
            other => panic!("expected ReorderWindows, got {:?}", other),
        }
    }

    #[test]
    fn move_cancel_restores_original_order() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@1", 1, "proj:edit"),
                wi("@2", 2, "scratch"),
                wi("@3", 3, "proj:term"),
            ]),
        );
        let before = model.tree().to_vec();

        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
        );
        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );

        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Render));
        assert_eq!(model.mode, Mode::Normal);
        assert_eq!(
            window_ids_in_order(model.tree()),
            window_ids_in_order(&before)
        );
        assert_eq!(model.reorder.pending_selection, None);
    }

    #[test]
    fn sidebar_window_remains_visible_in_tree() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@home", 1, "sidebar"),
                wi("@1", 2, "proj:edit"),
                wi("@2", 3, "scratch"),
            ]),
        );

        let names: Vec<_> = model
            .flat_items()
            .iter()
            .filter_map(|item| match get_node(model.tree(), &item.path).ok()? {
                TreeNode::Window { info } => Some(info.id.clone()),
                TreeNode::Folder { .. } => None,
            })
            .collect();

        assert_eq!(
            names,
            vec!["@home".to_string(), "@1".to_string(), "@2".to_string()]
        );
    }

    #[test]
    fn move_item_rejects_sidebar_host_window() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@home", 1, "sidebar"), wi("@1", 2, "proj:edit")]),
        );
        model.set_cursor(0);

        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
        );

        assert_eq!(model.mode, Mode::Normal);
        assert_eq!(
            model.info_message.as_deref(),
            Some("cannot move sidebar host window")
        );
        assert!(cmds.iter().any(|cmd| matches!(cmd, Cmd::Render)));
        assert_eq!(model.reorder.pending_selection, None);
    }

    #[test]
    fn move_item_rejects_folder_containing_sidebar_host() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![wi("@home", 1, "proj:alpha"), wi("@1", 2, "proj:beta")]),
        );

        let folder_index = find_folder_flat_index_by_path(model.flat_items(), model.tree(), "proj")
            .expect("proj folder should be visible");
        model.set_cursor(folder_index);

        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
        );

        assert_eq!(model.mode, Mode::Normal);
        assert_eq!(
            model.info_message.as_deref(),
            Some("cannot move folder containing sidebar host")
        );
        assert!(cmds.iter().any(|cmd| matches!(cmd, Cmd::Render)));
        assert_eq!(model.reorder.pending_selection, None);
    }

    #[test]
    fn move_project_rejects_project_containing_sidebar_host() {
        let mut model = test_model();
        update(
            &mut model,
            Msg::WindowListLoaded(vec![
                wi("@home", 1, "proj:alpha"),
                wi("@1", 2, "proj:beta"),
                wi("@2", 3, "other:main"),
            ]),
        );

        let beta_index = find_window_flat_index_by_id(model.flat_items(), model.tree(), "@1")
            .expect("proj:beta should be visible");
        model.set_cursor(beta_index);

        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT)),
        );

        assert_eq!(model.mode, Mode::Normal);
        assert_eq!(
            model.info_message.as_deref(),
            Some("cannot move project containing sidebar host")
        );
        assert!(cmds.iter().any(|cmd| matches!(cmd, Cmd::Render)));
        assert_eq!(model.reorder.pending_selection, None);
    }

    #[test]
    fn layout_helper_key_maps_to_command() {
        let mut model = test_model();
        let cmds = update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::NONE)),
        );
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::ApplyLayoutHelper));
    }
}
