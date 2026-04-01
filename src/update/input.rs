use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::cmd::Cmd;
use crate::model::{Mode, Model, PendingRename};
use crate::msg::Msg;
use crate::tree::{get_node, TreeNode};

use super::naming::{collect_folder_children, reconstruct_full_name};
use super::navigation::{exit_to_normal_mode, handle_move_enter};
use super::update;

pub(super) fn handle_key(model: &mut Model, event: KeyEvent) -> Vec<Cmd> {
    match &model.mode {
        Mode::Normal => handle_normal_key(model, event),
        Mode::Renaming { .. } => handle_renaming_key(model, event),
        Mode::RenamingFolder { .. } => handle_renaming_folder_key(model, event),
        Mode::CreatingProject => handle_creating_project_key(model, event),
        Mode::Moving { .. } => handle_moving_key(model, event),
        Mode::ConfirmClose { .. } => handle_confirm_close_key(model, event),
    }
}

fn is_plain_key(event: &KeyEvent) -> bool {
    event.modifiers.is_empty() || event.modifiers == KeyModifiers::SHIFT
}

fn char_to_byte_offset(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(s.len())
}

fn handle_input_key(model: &mut Model, code: KeyCode) -> Option<Vec<Cmd>> {
    match code {
        KeyCode::Left => {
            if model.input_cursor > 0 {
                model.input_cursor -= 1;
            }
            Some(vec![Cmd::Render])
        }
        KeyCode::Right => {
            if model.input_cursor < model.input_buffer.chars().count() {
                model.input_cursor += 1;
            }
            Some(vec![Cmd::Render])
        }
        KeyCode::Home => {
            model.input_cursor = 0;
            Some(vec![Cmd::Render])
        }
        KeyCode::End => {
            model.input_cursor = model.input_buffer.chars().count();
            Some(vec![Cmd::Render])
        }
        KeyCode::Backspace => {
            if model.input_cursor > 0 {
                model.input_cursor -= 1;
                let byte_idx = char_to_byte_offset(&model.input_buffer, model.input_cursor);
                model.input_buffer.remove(byte_idx);
            }
            Some(vec![Cmd::Render])
        }
        KeyCode::Delete => {
            if model.input_cursor < model.input_buffer.chars().count() {
                let byte_idx = char_to_byte_offset(&model.input_buffer, model.input_cursor);
                model.input_buffer.remove(byte_idx);
            }
            Some(vec![Cmd::Render])
        }
        KeyCode::Char(c) => {
            let byte_idx = char_to_byte_offset(&model.input_buffer, model.input_cursor);
            model.input_buffer.insert(byte_idx, c);
            model.input_cursor += 1;
            Some(vec![Cmd::Render])
        }
        _ => None,
    }
}

pub(super) fn clear_input(model: &mut Model) {
    model.input_buffer.clear();
    model.input_cursor = 0;
}

pub(super) fn set_input(model: &mut Model, value: String) {
    model.input_cursor = value.chars().count();
    model.input_buffer = value;
}

fn handle_normal_key(model: &mut Model, event: KeyEvent) -> Vec<Cmd> {
    if !is_plain_key(&event) {
        if event.modifiers == KeyModifiers::CONTROL && event.code == KeyCode::Char('c') {
            return update(model, Msg::Quit);
        }
        return vec![];
    }

    match event.code {
        KeyCode::Char('j') | KeyCode::Down => update(model, Msg::CursorDown),
        KeyCode::Char('k') | KeyCode::Up => update(model, Msg::CursorUp),
        KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => update(model, Msg::SelectItem),
        KeyCode::Char('h') | KeyCode::Left => update(model, Msg::CollapseOrParent),
        KeyCode::Char(' ') => update(model, Msg::ToggleFolder),
        KeyCode::Esc => update(model, Msg::Escape),
        KeyCode::Char('r') => update(model, Msg::RenameWindow),
        KeyCode::Char('x') => update(model, Msg::CloseWindow),
        KeyCode::Char('c') => update(model, Msg::NewWindow),
        KeyCode::Char('m') => update(model, Msg::MoveItem),
        KeyCode::Char('L') => update(model, Msg::ApplyLayoutHelper),
        KeyCode::Char('M') => update(model, Msg::MoveProject),
        KeyCode::Char('C') => update(model, Msg::NewProject),
        KeyCode::Char('R') => update(model, Msg::Restart),
        KeyCode::Char('q') => update(model, Msg::Quit),
        KeyCode::Char(d @ '1'..='9') => {
            let n = d.to_digit(10).unwrap() as usize;
            update(model, Msg::JumpToVisibleIndex(n))
        }
        KeyCode::Char('0') => vec![],
        _ => vec![],
    }
}

fn handle_renaming_key(model: &mut Model, event: KeyEvent) -> Vec<Cmd> {
    if !is_plain_key(&event) {
        return vec![];
    }
    if let Some(cmds) = handle_input_key(model, event.code) {
        return cmds;
    }
    match event.code {
        KeyCode::Enter => handle_renaming_enter(model),
        KeyCode::Esc => exit_to_normal_mode(model),
        _ => vec![],
    }
}

fn handle_creating_project_key(model: &mut Model, event: KeyEvent) -> Vec<Cmd> {
    if !is_plain_key(&event) {
        return vec![];
    }
    if let Some(cmds) = handle_input_key(model, event.code) {
        return cmds;
    }
    match event.code {
        KeyCode::Enter => handle_creating_project_enter(model),
        KeyCode::Esc => exit_to_normal_mode(model),
        _ => vec![],
    }
}

fn handle_renaming_folder_key(model: &mut Model, event: KeyEvent) -> Vec<Cmd> {
    if !is_plain_key(&event) {
        return vec![];
    }
    if let Some(cmds) = handle_input_key(model, event.code) {
        return cmds;
    }
    match event.code {
        KeyCode::Enter => handle_renaming_folder_enter(model),
        KeyCode::Esc => exit_to_normal_mode(model),
        _ => vec![],
    }
}

fn handle_confirm_close_key(model: &mut Model, event: KeyEvent) -> Vec<Cmd> {
    if !is_plain_key(&event) {
        return vec![];
    }
    match event.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            let mode = model.mode.clone();
            if let Mode::ConfirmClose { window_id } = mode {
                model.mode = Mode::Normal;
                vec![Cmd::CloseWindow { id: window_id }, Cmd::Render]
            } else {
                vec![]
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            model.mode = Mode::Normal;
            vec![Cmd::Render]
        }
        _ => vec![],
    }
}

fn handle_moving_key(model: &mut Model, event: KeyEvent) -> Vec<Cmd> {
    if !is_plain_key(&event) {
        return vec![];
    }
    if let Some(cmds) = handle_input_key(model, event.code) {
        return cmds;
    }
    match event.code {
        KeyCode::Enter => handle_move_enter(model),
        KeyCode::Esc => exit_to_normal_mode(model),
        _ => vec![],
    }
}

fn handle_renaming_enter(model: &mut Model) -> Vec<Cmd> {
    let mode = model.mode.clone();
    if let Mode::Renaming { window_id } = mode {
        let new_name = model.input_buffer.trim().to_string();
        if new_name.is_empty() {
            return vec![Cmd::Render];
        }
        let already_named = model.flat_items().iter().enumerate().any(|(idx, item)| {
            if let Ok(TreeNode::Window { info }) = get_node(model.tree(), &item.path) {
                if info.id != window_id {
                    return false;
                }
                let full_name = reconstruct_full_name(model, idx, &info.name);
                full_name == new_name
            } else {
                false
            }
        });
        if already_named {
            return exit_to_normal_mode(model);
        }
        model.mode = Mode::Normal;
        model.renames.pending.insert(
            window_id.clone(),
            PendingRename {
                target_name: new_name.clone(),
                observed_count: 0,
            },
        );
        model.renames.last_window_id = Some(window_id.clone());
        clear_input(model);
        vec![
            Cmd::RenameWindow {
                id: window_id,
                name: new_name,
            },
            Cmd::Render,
        ]
    } else {
        vec![]
    }
}

fn handle_creating_project_enter(model: &mut Model) -> Vec<Cmd> {
    let name = model.input_buffer.trim().to_string();
    if name.is_empty() {
        return vec![Cmd::Render];
    }
    model.mode = Mode::Normal;
    clear_input(model);
    let window_name = format!("{}:tab1", name);
    vec![Cmd::NewWindow { name: window_name }]
}

fn handle_renaming_folder_enter(model: &mut Model) -> Vec<Cmd> {
    let mode = model.mode.clone();
    if let Mode::RenamingFolder { folder_name } = mode {
        let new_name = model.input_buffer.trim().to_string();
        if new_name.is_empty() || new_name == folder_name {
            return exit_to_normal_mode(model);
        }
        model.mode = Mode::Normal;
        clear_input(model);

        let children = collect_folder_children(model.tree(), &folder_name);
        let mut cmds = Vec::new();
        for (window_id, child_name) in &children {
            let full_name = format!("{}:{}", new_name, child_name);
            model.renames.pending.insert(
                window_id.clone(),
                PendingRename {
                    target_name: full_name.clone(),
                    observed_count: 0,
                },
            );
            model.renames.last_window_id = Some(window_id.clone());
            cmds.push(Cmd::RenameWindow {
                id: window_id.clone(),
                name: full_name,
            });
        }
        cmds.push(Cmd::Render);
        cmds
    } else {
        vec![]
    }
}
