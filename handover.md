# Handover

## Current state
- Version: `0.2.4`
- Working tree target: clean after the next commit
- Recent focus: stability + window refresh fast paths + tmux query reduction + AI poll idle backoff

## Just completed
- Added safe fast paths for `WindowListLoaded`
  - same-list skip
  - rename-only leaf updates
  - root-level single add/remove
  - nested single add/remove for existing folder paths
- Reduced restore-preview tmux queries by reusing a single pane-list query
- Fixed benchmark inputs so `window_list_loaded` benches actually measure fast-path cases
- Added AI poll idle backoff
  - 4 empty polls: skip 1 tick
  - 8 empty polls: skip 2 ticks
  - 16 empty polls: skip 4 ticks
  - 32+ empty polls: skip 8 ticks
  - reset immediately when AI candidates appear

## Latest runtime observations
From `/tmp/tide.log` on a simple 2-window demo session:
- `tmux metrics`: all zero
  - `pane_output_dropped=0`
  - `coalesced_resync_deferred=0`
  - `coalesced_resync_flushed=0`
  - `command_failures=0`
  - `batch_reconciles=0`
- render timings were about `0.6ms - 1.7ms` for `flat_items=4`
- dominant background work is still `list-panes -s ...` for AI polling
- idle backoff is working, but the short demo only reached about `1s` cadence, not the higher backoff tiers

## Latest benchmark takeaways
- `window_list_loaded/rename_only` recovered strongly after fixing the benchmark and detection order
- `window_list_loaded/add_only` and `remove_only` are now measured separately
- `view_build_tree_items/500` and `/1000` improved again after the metric-fix patch

## Next recommended work

### 1. Reduce remaining `execute.rs` tmux round-trips
Focus areas:
- `reconcile_sidebar_state()`
- preview/follow/restore layout queries
- repeated `display-message` / `list-panes` sequences around move/recover paths

Goal:
- fewer tmux round-trips per navigation/recovery action
- lower latency under rapid preview/follow operations

### 2. Make AI poll backoff more observable
Add trace logging for:
- current `idle_polls`
- current `poll_skip_ticks`
- when backoff resets

Goal:
- verify real sessions are reaching the intended skip tiers
- make `/tmp/tide.log` sufficient for tuning without code inspection

### 3. Run a longer real-session metrics pass
Suggested scenarios:
- idle session for 30-60s
- rapid cursor movement / preview switching
- repeated window rename/add/remove bursts

Capture and inspect:
- `tmux metrics`
- `render completed`
- `ai classification completed`
- any `tmux command failed` / `reconcile: batch failed`

## Useful commands
- Bench: `cargo bench --bench phase0`
- Tests: `cargo test --lib && cargo test --bin tide`
- Debug runtime log:
  - `TIDE_LOG=trace tide <session>`
  - log file: `/tmp/tide.log`

## Risk notes
- incremental add/remove fast paths still intentionally fall back for:
  - folder creation/removal
  - empty-folder pruning cases
  - multi-item structural changes
  - reparenting / hierarchy-changing renames
- keep correctness-first fallback behavior unless new fast paths are covered by tests
