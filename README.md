# tide

**A tree-view sidebar for tmux.**

Your IDE has a file tree for navigating code. tide brings the same idea to tmux — a persistent sidebar that organizes windows into a collapsible project tree.

```
┌──────────┬──────────────────────────────┐
│ tide     │                              │
│          │    your working pane         │
│  proj-a  │                              │
│  ├ edit  │                              │
│  └ term  │                              │
│  proj-b  │                              │
│  └ main  │                              │
│          │                              │
└──────────┴──────────────────────────────┘
```

## Why

Agentic development means juggling multiple projects and sessions at once. But tmux still makes you memorize window indices or mash `prefix+w` to find where you are.

tide fixes this. Name your windows `project:task` and they fold into a tree — just like files in a directory. One glance shows everything. One keystroke switches context.

- **See everything** — all projects, all windows, one sidebar
- **Switch instantly** — cursor to preview, Enter to jump
- **Stay organized** — `:` delimiter auto-groups windows into folders
- **Stay in sync** — tmux control mode keeps the tree updated in real time

## Install

```bash
cargo install --path .
```

## Usage

```bash
tide
```

Launch inside any tmux session. tide creates a 25-column sidebar and takes over window management.

### Keybindings

| Key | Action |
|-----|--------|
| `j` / `k` | Move cursor (+ preview) |
| `Enter` / `l` | Select window / expand folder |
| `h` | Collapse folder / go to parent |
| `Space` | Toggle folder expand/collapse |
| `c` | Create window |
| `C` | Create project (folder + window) |
| `r` | Rename window |
| `x` | Close window (with confirmation) |
| `Esc` | Focus right pane |
| `q` | Quit tide |

### Window naming convention

Use `:` as a delimiter to create folder structure:

```
myapp:editor    →  myapp/
myapp:terminal       ├ editor
api:server           └ terminal
api:logs         api/
                     ├ server
                     └ logs
```

## How it works

tide connects to tmux via **control mode** (`tmux -CC attach`) and uses The Elm Architecture (TEA) internally:

- All tmux events (window add/close/rename) flow in as messages
- A pure `update()` function computes the next state
- Side effects are expressed as commands, executed separately

This keeps the logic testable and the UI responsive.

## Requirements

- tmux 3.2+
- Rust 1.70+ (to build)

## Known issues

### Shell hooks that rename windows

If your shell config has `precmd` / `preexec` / `PROMPT_COMMAND` hooks that call `tmux rename-window`, they will override tide's window names.

`automatic-rename off` and `allow-rename off` do **not** prevent this — those only block terminal escape sequences, not explicit rename commands.

**Fix**: remove any shell-level `tmux rename-window` hooks when using tide.

## License

[MIT](LICENSE)
