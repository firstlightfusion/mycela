use serde::Serialize;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[derive(Default)]
pub struct RuntimeMetrics {
    active_sse_clients: AtomicUsize,
    widget_poller_count: AtomicUsize,
    modbus_queue_depth: AtomicUsize,
    modbus_write_count: AtomicU64,
    modbus_write_latency_total_us: AtomicU64,
    modbus_write_latency_max_us: AtomicU64,
    modbus_write_latency_latest_us: AtomicU64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeMetricsSnapshot {
    pub active_sse_clients: usize,
    pub widget_poller_count: usize,
    pub modbus_queue_depth: usize,
    pub modbus_write_count: u64,
    pub modbus_write_latency_average_us: u64,
    pub modbus_write_latency_max_us: u64,
    pub modbus_write_latency_latest_us: u64,
}

impl RuntimeMetrics {
    pub fn increment_sse_clients(&self) {
        self.active_sse_clients.fetch_add(1, Ordering::Relaxed);
    }

    pub fn decrement_sse_clients(&self) {
        self.active_sse_clients.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn increment_widget_pollers(&self) {
        self.widget_poller_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn decrement_widget_pollers(&self) {
        self.widget_poller_count.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn increment_modbus_queue_depth(&self) {
        self.modbus_queue_depth.fetch_add(1, Ordering::Relaxed);
    }

    pub fn decrement_modbus_queue_depth(&self) {
        self.modbus_queue_depth.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn record_modbus_write_latency(&self, latency_us: u64) {
        self.modbus_write_count.fetch_add(1, Ordering::Relaxed);
        self.modbus_write_latency_total_us
            .fetch_add(latency_us, Ordering::Relaxed);
        self.modbus_write_latency_latest_us
            .store(latency_us, Ordering::Relaxed);
        self.modbus_write_latency_max_us
            .fetch_max(latency_us, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RuntimeMetricsSnapshot {
        let write_count = self.modbus_write_count.load(Ordering::Relaxed);
        let total_latency = self.modbus_write_latency_total_us.load(Ordering::Relaxed);
        RuntimeMetricsSnapshot {
            active_sse_clients: self.active_sse_clients.load(Ordering::Relaxed),
            widget_poller_count: self.widget_poller_count.load(Ordering::Relaxed),
            modbus_queue_depth: self.modbus_queue_depth.load(Ordering::Relaxed),
            modbus_write_count: write_count,
            modbus_write_latency_average_us: if write_count == 0 { 0 } else { total_latency / write_count },
            modbus_write_latency_max_us: self.modbus_write_latency_max_us.load(Ordering::Relaxed),
            modbus_write_latency_latest_us: self.modbus_write_latency_latest_us.load(Ordering::Relaxed),
        }
    }
}