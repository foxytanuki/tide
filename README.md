# tide

tmux session manager with a sidebar-style TUI. Manages windows as a tree (folders via `name:subname` convention), supports preview, rename, and keyboard-driven navigation.

## Install

```bash
cargo install --path .
```

## Usage

```bash
tide
```

Runs inside a tmux pane and takes over window management for the current session.

### Keybindings

| Key | Action |
|-----|--------|
| `j` / `k` | Move cursor |
| `l` / Enter | Select / enter window |
| `h` | Collapse folder / go to parent |
| `Space` | Toggle folder expand |
| `c` | Create window |
| `C` | Create project (folder + window) |
| `r` | Rename window |
| `x` | Close window |
| `Esc` | Focus right pane |
| `q` | Quit |

## Known issues

### Shell `precmd`/`preexec` hooks that call `tmux rename-window` will conflict with tide

tide manages window names internally. If your shell configuration contains hooks like:

```zsh
# DO NOT use this with tide
add-zsh-hook precmd _my_auto_rename
```

or any `precmd` / `preexec` / `PROMPT_COMMAND` that calls `tmux rename-window`, it will override tide's window names every time the prompt is drawn.

`automatic-rename off` and `allow-rename off` do **not** prevent this, because those options only block terminal escape sequences, not explicit `tmux rename-window` commands.

**If you use tide, remove any shell-level `tmux rename-window` hooks.**
