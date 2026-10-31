//! How a Node program collects elmdb's diagnostics (`elmdb::diagnostics`): an experiment behind this crate's
//! `diagnostics` feature, off by default, proving the engine's spans reach JavaScript before any public configuration
//! is chosen (`docs/evidence/diagnostics.md`).
//!
//! `collectDiagnostics()` installs elmdb's `Collector` once per process; `takeDiagnostics()` hands JavaScript what it
//! kept since the last call. JavaScript asks rather than being called, since spans close on the threads elmdb runs on,
//! which are not JavaScript's. Both need Node, so `examples/diagnostics.mjs` runs them rather than a Rust test.

use std::sync::{Arc, OnceLock};

use elmdb::diagnostics::Collector;
use napi_derive::napi;

/// The most spans kept between two takes
const KEPT: usize = 10_000;

static COLLECTOR: OnceLock<Arc<Collector>> = OnceLock::new();

/// Starts keeping elmdb's spans for `takeDiagnostics`, for the rest of the process. Refused when the process already
/// has a collector of another kind
#[napi(js_name = "collectDiagnostics")]
pub fn collect_diagnostics() -> napi::Result<()> {
    if COLLECTOR.get().is_some() {
        return Ok(());
    }
    let collector = Arc::new(Collector::new(KEPT));
    tracing::subscriber::set_global_default(Arc::clone(&collector))
        .map_err(|e| napi::Error::from_reason(format!("This process already collects diagnostics: {e}")))?;
    let _ = COLLECTOR.set(collector);
    Ok(())
}

#[napi(object)]
pub struct Diagnostics {
    /// Each span closed since the last take, oldest first, as JSON: `span` and `id`, `parent` and `parent_id`, and its
    /// fields
    pub spans: Vec<String>,
    /// Spans closed past the most kept, and so not in `spans`
    pub dropped: f64,
}

/// What `collectDiagnostics` kept since the last take
#[napi(js_name = "takeDiagnostics")]
pub fn take_diagnostics() -> Diagnostics {
    let taken = COLLECTOR.get().map(|collector| collector.take()).unwrap_or_default();
    Diagnostics { spans: taken.spans, dropped: taken.dropped as f64 }
}
