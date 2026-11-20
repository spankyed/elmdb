//! How long a test waits for what must happen: one deadline for every such wait, set by the slowest runner a suite
//! is run on rather than by how long the thing takes when the machine is idle

use std::time::Duration;

/// How long a test waits for something it expects to happen, a thread to reach a point or a call to return, before
/// it fails. A wait that succeeds takes only what the thing took, so this costs a passing test nothing, and is long:
/// two to five seconds, as these waits were, were met on CI's loaded runners by tests that then passed again on a
/// rerun (2026-11-18, #182, and 2026-11-19, #195). It is under the shortest deadline nextest gives a test that waits
/// so (`.config/nextest.toml`), so a test whose thing never happens fails on this, saying what it waited for. A wait
/// that something must not happen is another matter: it passes however long it is, and is kept short
pub(crate) const ARRIVES: Duration = Duration::from_secs(8);
