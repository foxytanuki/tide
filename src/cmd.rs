#[derive(Debug, Clone)]
pub enum Cmd {
    // Preview: swap target window's pane into right slot
    PreviewWindow { id: String },
    RestorePreview,
    FocusRightPane,
    NewWindow { name: String },
    RenameWindow { id: String, name: String },
    CloseWindow { id: String },
    FollowToWindow { window_id: String },
    EnsureSidebarWidth,
    ListWindows,

    // App commands
    Render,
    Quit,
}
