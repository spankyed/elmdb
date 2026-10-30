//! What the engine reports about its slow operations while they run, as [`tracing`](https://docs.rs/tracing) spans
//! (`docs/architecture/testing.md`, Diagnostics).
//!
//! Spans are coarse — one per operation or phase, never one per row — and carry phases, counts, durations and outcomes:
//! schema names, never entity values, ids, query payloads or paths. The engine never installs a collector; the
//! program embedding it does, or nothing is collected.
//!
//! Without the `diagnostics` feature, which is off by default, `tracing` is not a dependency and an [`Op`] is empty, so
//! every call here compiles to nothing. With it and no collector, an [`Op`] is `None` after one check of its callsite,
//! and no clock is read and no field computed: `scripts/diagnostics-overhead.py` holds that cost to a declared bound.

#[cfg(feature = "diagnostics")]
use std::time::Instant;

use crate::error::{Error, Result};

/// The spans' names and fields, which the tests and `docs/architecture/testing.md` name too
pub mod names {
    /// A rebuild of derived data: every index, or the one named by `entity_type` and `index` (both empty for every
    /// index)
    pub const REBUILD: &str = "rebuild";
    /// One write transaction, from asking for the writer to the commit and its sync
    pub const WRITE: &str = "write";
    /// Opening a unit, which holds the writer across calls
    pub const BEGIN: &str = "begin";
    /// Waiting for an open unit to end
    pub const UNIT_WAIT: &str = "unit_wait";
    /// Sorting derived entries, in memory and in runs spilled to scratch files
    pub const SORT: &str = "sort";

    /// What every span records as it ends: `committed`, `durable` or `not_durable` (committed, its sync failed) for a
    /// write or a rebuild, `begun` for a unit, `returned` for a wait, `sorted` for a sort, or `failed`
    pub const OUTCOME: &str = "outcome";
    /// A failed operation's `ErrorCode`
    pub const ERROR: &str = "error";
    /// Microseconds from the span's start to its end
    pub const ELAPSED_US: &str = "elapsed_us";
    /// Microseconds from the span's start until it held the writer, or the unit it waited on ended
    pub const WAITED_US: &str = "waited_us";
    /// Times a write grew the map after filling it, and ran again
    pub const GROWTHS: &str = "growths";
    /// Times a write found another process had grown the file, and adopted its size
    pub const RESIZES: &str = "resizes";
    /// Entries a sort was given
    pub const ROWS: &str = "rows";
    /// Runs a sort wrote to scratch files, the last one at its finish included, and the bytes they held then
    pub const RUNS: &str = "runs";
    pub const SPILLED_BYTES: &str = "spilled_bytes";
}

/// The target every span is reported under, for a collector to filter on
pub const TARGET: &str = "elmdb";

/// One operation being reported: empty without the feature, `None` inside when nothing collects it
pub(crate) struct Op {
    #[cfg(feature = "diagnostics")]
    live: Option<(tracing::Span, Instant)>,
}

impl Op {
    #[cfg(feature = "diagnostics")]
    fn live(span: tracing::Span) -> Self {
        // A span nobody collects is disabled after its callsite's cached check: no clock is read for it
        Self { live: (!span.is_disabled()).then(|| (span, Instant::now())) }
    }

    #[inline(always)]
    pub fn rebuild(index: Option<(&str, &str)>) -> Self {
        #[cfg(feature = "diagnostics")]
        {
            use tracing::field::Empty;
            let (entity_type, index) = index.unwrap_or(("", ""));
            Self::live(tracing::info_span!(
                target: TARGET, "rebuild", entity_type, index, outcome = Empty, error = Empty,
                elapsed_us = Empty
            ))
        }
        #[cfg(not(feature = "diagnostics"))]
        {
            let _ = index;
            Self {}
        }
    }

    #[inline(always)]
    pub fn write(durable: bool) -> Self {
        #[cfg(feature = "diagnostics")]
        {
            use tracing::field::Empty;
            Self::live(tracing::info_span!(
                target: TARGET, "write", durable, waited_us = Empty, growths = Empty, resizes = Empty,
                outcome = Empty, error = Empty, elapsed_us = Empty
            ))
        }
        #[cfg(not(feature = "diagnostics"))]
        {
            let _ = durable;
            Self {}
        }
    }

    #[inline(always)]
    pub fn begin() -> Self {
        #[cfg(feature = "diagnostics")]
        {
            use tracing::field::Empty;
            Self::live(tracing::info_span!(
                target: TARGET, "begin", waited_us = Empty, resizes = Empty, outcome = Empty, error = Empty,
                elapsed_us = Empty
            ))
        }
        #[cfg(not(feature = "diagnostics"))]
        Self {}
    }

    #[inline(always)]
    pub fn unit_wait() -> Self {
        #[cfg(feature = "diagnostics")]
        {
            use tracing::field::Empty;
            Self::live(tracing::info_span!(
                target: TARGET, "unit_wait", waited_us = Empty, outcome = Empty, elapsed_us = Empty
            ))
        }
        #[cfg(not(feature = "diagnostics"))]
        Self {}
    }

    #[inline(always)]
    pub fn sort() -> Self {
        #[cfg(feature = "diagnostics")]
        {
            use tracing::field::Empty;
            Self::live(tracing::info_span!(
                target: TARGET, "sort", rows = Empty, runs = Empty, spilled_bytes = Empty, outcome = Empty,
                error = Empty, elapsed_us = Empty
            ))
        }
        #[cfg(not(feature = "diagnostics"))]
        Self {}
    }

    /// An operation reported nowhere, standing in for one already ended
    #[inline(always)]
    pub fn none() -> Self {
        Self {
            #[cfg(feature = "diagnostics")]
            live: None,
        }
    }

    /// Runs `f` inside the span, so the spans `f` starts are its children
    #[inline(always)]
    pub fn in_scope<T>(&self, f: impl FnOnce() -> T) -> T {
        #[cfg(feature = "diagnostics")]
        if let Some((span, _)) = &self.live {
            return span.in_scope(f);
        }
        f()
    }

    /// Records `field`, computing it only when the span is collected
    #[inline(always)]
    pub fn record(&self, field: &'static str, value: impl FnOnce() -> u64) {
        #[cfg(feature = "diagnostics")]
        if let Some((span, _)) = &self.live {
            span.record(field, value());
        }
        #[cfg(not(feature = "diagnostics"))]
        let _ = (field, value);
    }

    /// Records the microseconds since the span started as `field`
    #[inline(always)]
    pub fn record_elapsed(&self, field: &'static str) {
        #[cfg(feature = "diagnostics")]
        if let Some((span, started)) = &self.live {
            span.record(field, micros(started));
        }
        #[cfg(not(feature = "diagnostics"))]
        let _ = field;
    }

    /// Ends the span with `outcome`, its elapsed time and, for a failure, its error's code
    #[inline(always)]
    pub fn end(self, outcome: std::result::Result<&'static str, &Error>) {
        #[cfg(feature = "diagnostics")]
        if let Some((span, started)) = self.live {
            match outcome {
                Ok(outcome) => span.record(names::OUTCOME, outcome),
                Err(error) => {
                    span.record(names::OUTCOME, "failed");
                    span.record(names::ERROR, tracing::field::debug(error.code))
                }
            };
            span.record(names::ELAPSED_US, micros(&started));
        }
        #[cfg(not(feature = "diagnostics"))]
        let _ = outcome;
    }

    /// Ends the span with `outcome`, or the error `result` holds, answering `result`
    #[inline(always)]
    pub fn ended<T>(self, result: Result<T>, outcome: &'static str) -> Result<T> {
        self.end(result.as_ref().map(|_| outcome));
        result
    }
}

#[cfg(feature = "diagnostics")]
fn micros(started: &Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}
