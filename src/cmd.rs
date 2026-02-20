#[derive(Debug, Clone)]
pub enum Cmd {
    // Preview: swap target window's pane into right slot
    PreviewWindow { id: String },
    RestorePreview,
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
