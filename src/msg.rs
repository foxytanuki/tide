use std::collections::HashSet;

use crossterm::event::KeyEvent;

use crate::tree::WindowInfo;

#[derive(Debug, Clone)]
pub enum Msg {
    // Key events
    Key(KeyEvent),

    // tmux events (from control mode)
    WindowChanged,
    WindowRenamed {
        window_id: String,
        name: String,
    },
    WindowListLoaded(Vec<WindowInfo>),
    WindowFocusChanged(String), // window_id

    // UI events
    CursorUp,
    CursorDown,
    JumpToVisibleIndex(usize),
    SelectItem,
    CollapseOrParent,
    ToggleFolder,
    Escape,
    NewWindow,
    NewProject,
    RenameWindow,
    CloseWindow,

    // Mouse events
    MouseClick {
        row: u16,
    },
    MouseScrollUp,
    MouseScrollDown,

    // AI process detection
    AiProcessPollResult {
        panes: HashSet<String>,   // active AI pane_ids
        windows: HashSet<String>, // derived window_ids (for sidebar)
    },

    Restart,
    Quit,
}
