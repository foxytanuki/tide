use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TmuxMetricsSnapshot {
    pub pane_output_dropped: u64,
    pub coalesced_resync_deferred: u64,
    pub coalesced_resync_flushed: u64,
    pub command_failures: u64,
    pub batch_reconciles: u64,
}

static PANE_OUTPUT_DROPPED: AtomicU64 = AtomicU64::new(0);
static COALESCED_RESYNC_DEFERRED: AtomicU64 = AtomicU64::new(0);
static COALESCED_RESYNC_FLUSHED: AtomicU64 = AtomicU64::new(0);
static COMMAND_FAILURES: AtomicU64 = AtomicU64::new(0);
static BATCH_RECONCILES: AtomicU64 = AtomicU64::new(0);

pub fn snapshot_tmux_metrics() -> TmuxMetricsSnapshot {
    TmuxMetricsSnapshot {
        pane_output_dropped: PANE_OUTPUT_DROPPED.load(Ordering::Relaxed),
        coalesced_resync_deferred: COALESCED_RESYNC_DEFERRED.load(Ordering::Relaxed),
        coalesced_resync_flushed: COALESCED_RESYNC_FLUSHED.load(Ordering::Relaxed),
        command_failures: COMMAND_FAILURES.load(Ordering::Relaxed),
        batch_reconciles: BATCH_RECONCILES.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
pub fn reset_tmux_metrics() {
    PANE_OUTPUT_DROPPED.store(0, Ordering::Relaxed);
    COALESCED_RESYNC_DEFERRED.store(0, Ordering::Relaxed);
    COALESCED_RESYNC_FLUSHED.store(0, Ordering::Relaxed);
    COMMAND_FAILURES.store(0, Ordering::Relaxed);
    BATCH_RECONCILES.store(0, Ordering::Relaxed);
}

pub(crate) fn record_pane_output_dropped() -> u64 {
    PANE_OUTPUT_DROPPED.fetch_add(1, Ordering::Relaxed) + 1
}

pub(crate) fn record_coalesced_resync_deferred() -> u64 {
    COALESCED_RESYNC_DEFERRED.fetch_add(1, Ordering::Relaxed) + 1
}

pub(crate) fn record_coalesced_resync_flushed() -> u64 {
    COALESCED_RESYNC_FLUSHED.fetch_add(1, Ordering::Relaxed) + 1
}

pub(crate) fn record_tmux_command_failure() -> u64 {
    COMMAND_FAILURES.fetch_add(1, Ordering::Relaxed) + 1
}

pub(crate) fn record_batch_reconcile() -> u64 {
    BATCH_RECONCILES.fetch_add(1, Ordering::Relaxed) + 1
}
