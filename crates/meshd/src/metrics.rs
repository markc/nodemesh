use std::sync::atomic::{AtomicU64, Ordering};

/// Simple counters for observability.
pub struct Metrics {
    pub messages_sent: AtomicU64,
    pub messages_received: AtomicU64,
    pub messages_dropped: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            messages_sent: AtomicU64::new(0),
            messages_received: AtomicU64::new(0),
            messages_dropped: AtomicU64::new(0),
        }
    }

    pub fn inc_sent(&self) {
        self.messages_sent.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_received(&self) {
        self.messages_received.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_dropped(&self) {
        self.messages_dropped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            sent: self.messages_sent.load(Ordering::Relaxed),
            received: self.messages_received.load(Ordering::Relaxed),
            dropped: self.messages_dropped.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct MetricsSnapshot {
    pub sent: u64,
    pub received: u64,
    pub dropped: u64,
}
