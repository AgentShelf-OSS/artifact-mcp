use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Low-cardinality live preview-queue gauges updated by the production queue itself.
#[derive(Clone, Debug, Default)]
pub struct RenderQueuePressure {
    pub(crate) pending: Arc<AtomicU64>,
    pub(crate) running: Arc<AtomicU64>,
    pub(crate) reserved_bytes: Arc<AtomicU64>,
}

impl RenderQueuePressure {
    pub fn update(&self, pending: usize, running: bool, reserved_bytes: u64) {
        self.pending.store(pending as u64, Ordering::Relaxed);
        self.running.store(u64::from(running), Ordering::Relaxed);
        self.reserved_bytes.store(reserved_bytes, Ordering::Relaxed);
    }
}
