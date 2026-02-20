#[derive(Debug, Clone)]
pub enum Cmd {
    // tmux commands
    SelectWindow { id: String },
    FocusRightPane,
    NewWindow { name: String },
    RenameWindow { id: String, name: String },
    CloseWindow { id: String },
    ListWindows,

    // App commands
    Render,
    Quit,

    // Batch
    Batch(Vec<Cmd>),
}
