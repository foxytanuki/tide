# AGENTS.md

This file provides repository guidance for coding agents and contributors working in this repository.

`AGENTS.md` is the canonical copy. `CLAUDE.md` may exist as a symlink for tool compatibility.

## Project Overview

**tide** — tmux session manager with a sidebar-style TUI (25-column left pane). Manages windows as a collapsible tree using `:` as folder delimiter. Built in Rust with The Elm Architecture (TEA).

## Build & Development Commands

```bash
cargo build              # debug build
cargo clippy -- -D warnings  # lint (warnings = errors)
cargo test               # all tests
cargo test --lib         # unit tests only
cargo test <test_name>   # single test
cargo install --path .   # install binary as `tide`
```

Task runner (`justfile`): `just build`, `just check`, `just install`, `just test`

### Logging

```bash
TIDE_LOG=debug tide      # enable debug logging
```

Logs write to `/tmp/tide.log`. Controlled by `TIDE_LOG` env var (uses `tracing_subscriber::EnvFilter`).

## Architecture: TEA (The Elm Architecture)

```
tmux control mode events ─┐
                           ├→ Msg → update(model, msg) → (Model, Vec<Cmd>)
crossterm key events ──────┘                                     │
                                                         execute_commands()
                                                                 │
                                                    ┌────────────┤
                                                    ▼            ▼
                                             render(model)   tmux CLI
```

- `update()` is a **pure function** — no side effects, returns `Vec<Cmd>`
- Side effects are expressed as `Cmd` values, executed by `execute_commands()`

### Module Responsibilities

| Module | Role |
|--------|------|
| `main.rs` | Bootstrap (launcher), event loop (`tokio::select!`), cleanup |
| `msg.rs` | `Msg` enum — all events (keys, tmux notifications) |
| `cmd.rs` | `Cmd` enum — all side effects (tmux commands, render, quit) |
| `model.rs` | `Model` struct — application state (tree, cursor, mode, preview) |
| `update.rs` | Pure `update(model, msg) → Vec<Cmd>` — core logic |
| `view.rs` | ratatui rendering (header + tree + footer) |
| `execute.rs` | Command execution, batch orchestration, error reconciliation |
| `tree.rs` | `TreeNode` / `FlatItem`, `build_tree()` (`:` grouping), `flatten()` |
| `tmux/control.rs` | PTY allocation, `tmux -CC attach`, async event stream, command queue |
| `tmux/parser.rs` | Parse control mode events (`%window-add`, `%begin`/`%end`, etc.) |
| `launcher.rs` | Session bootstrap, sidebar pane creation, re-entry guard (`TIDE_SIDEBAR=1`) |

### Key Patterns

**Preview vs. Follow**: Cursor movement previews windows (pane swap, focus stays in sidebar). Enter follows to the window (focus moves right). Both use `join-pane` batched commands with `ignore_window_changes` counter to suppress self-triggered events.

**Window rename stabilization**: `PendingRename` tracks renames with `observed_count`. Retries if external hooks override the name. Gives up after 6 observations.

**Batch command reconciliation**: Semicolon-joined tmux commands can fail mid-batch. `send_batch_with_reconcile()` re-syncs model with actual tmux state on failure.

**Folder expansion persistence**: `collect_folder_expanded()` / `restore_folder_expanded()` preserves expand/collapse state across tree rebuilds.

**Mode state machine**: `Normal` → `Renaming` (r) → `Normal` (Enter/Esc), `Normal` → `ConfirmClose` (x) → `Normal` (y/n)

### tmux Communication

Uses control mode (`tmux -CC attach -t <session>`) via PTY. Commands sent through stdin, events received on stdout. Async read thread with oneshot channel response watchers. Session name is `tide`.

## Known Issues

Shell `precmd`/`preexec` hooks that call `tmux rename-window` conflict with tide's window name management. `automatic-rename off` / `allow-rename off` do not prevent this. Users must remove shell-level rename hooks.
