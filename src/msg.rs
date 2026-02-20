use crossterm::event::KeyEvent;

use crate::tree::WindowInfo;

#[derive(Debug, Clone)]
pub enum Msg {
    // Key events
    Key(KeyEvent),

    // tmux events (from control mode)
    WindowAdded(String),
    WindowClosed(String),
    WindowRenamed(String, String),
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
    RenameWindow,
    CloseWindow,

    // Internal
    Tick,
    Quit,
}
