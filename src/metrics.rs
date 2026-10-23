//! Prometheus metrics.

use std::sync::{Arc, Mutex};

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SessionLabels {
    method: String,
    accel: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct MethodLabels {
    method: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ScanLabels {
    library: String,
    result: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct LibraryLabels {
    library: String,
}

#[derive(Clone)]
pub struct Metrics {
    registry: Arc<Mutex<Registry>>,
    sessions: Family<SessionLabels, Gauge>,
    playbacks: Family<MethodLabels, Counter>,
    scans: Family<ScanLabels, Counter>,
    items: Family<LibraryLabels, Gauge>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix("ferrocine");
        let sessions = Family::<SessionLabels, Gauge>::default();
        registry.register(
            "playback_sessions",
            "Active HLS sessions by method and acceleration",
            sessions.clone(),
        );
        let playbacks = Family::<MethodLabels, Counter>::default();
        registry.register("playbacks", "Playbacks started by method", playbacks.clone());
        let scans = Family::<ScanLabels, Counter>::default();
        registry.register("scans", "Library scans by result", scans.clone());
        let items = Family::<LibraryLabels, Gauge>::default();
        registry.register("library_items", "Movies or series per library", items.clone());
        Self {
            registry: Arc::new(Mutex::new(registry)),
            sessions,
            playbacks,
            scans,
            items,
        }
    }

    pub fn set_sessions(&self, method: &str, accel: &str, n: i64) {
        self.sessions
            .get_or_create(&SessionLabels {
                method: method.into(),
                accel: accel.into(),
            })
            .set(n);
    }

    pub fn playback(&self, method: &str) {
        self.playbacks
            .get_or_create(&MethodLabels { method: method.into() })
            .inc();
    }

    pub fn scan(&self, library: &str, ok: bool) {
        self.scans
            .get_or_create(&ScanLabels {
                library: library.into(),
                result: if ok { "ok" } else { "error" }.into(),
            })
            .inc();
    }

    pub fn set_items(&self, library: &str, n: i64) {
        self.items
            .get_or_create(&LibraryLabels {
                library: library.into(),
            })
            .set(n);
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Ok(r) = self.registry.lock() {
            let _ = encode(&mut out, &r);
        }
        out
    }
}
