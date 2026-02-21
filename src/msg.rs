use crossterm::event::KeyEvent;

use crate::tree::WindowInfo;

#[derive(Debug, Clone)]
pub enum Msg {
    // Key events
    Key(KeyEvent),

    // tmux events (from control mode)
    WindowChanged,
    WindowRenamed { window_id: String, name: String },
    WindowListLoaded(Vec<WindowInfo>),
    WindowFocusChanged(String), // window_id

    // UI events
    CursorUp,
    CursorDown,
    SelectItem,
    CollapseOrParent,
    ToggleFolder,
    Escape,
    NewWindow,
    NewProject,
    RenameWindow,
    CloseWindow,

    Quit,
}
