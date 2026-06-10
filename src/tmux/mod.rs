pub mod commands;
pub mod control;
pub mod parser;

pub use control::TmuxControl;
pub use parser::TmuxEvent;

pub use crate::tree::WindowInfo;

pub const SIDEBAR_WIDTH: u16 = 30;

/// Escape a string for use in tmux command arguments.
/// tmux control mode uses its own parser (not shell). Double-quote and escape inside.
pub fn quote_tmux(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    out.push('"');
    for c in input.chars() {
        match c {
            '"' | '\\' | '$' | '#' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Escape a string for single-quoted POSIX shell command text.
/// This is distinct from `quote_tmux`, which escapes tmux command arguments.
pub(crate) fn shell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}
