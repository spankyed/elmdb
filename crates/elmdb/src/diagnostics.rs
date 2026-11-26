//! Optional span collector behind the `diagnostics` feature (off by default).

#[derive(Debug, Clone, Default)]
pub struct Taken {
    pub spans: Vec<String>,
    pub dropped: u64,
}

#[cfg(feature = "diagnostics")]
mod active {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use tracing::Subscriber;
    use tracing::span::{Attributes, Id, Record};

    use super::Taken;

    struct State {
        spans: VecDeque<String>,
        dropped: u64,
        next_id: u64,
    }

    pub struct Collector {
        cap: usize,
        state: Mutex<State>,
    }

    impl Collector {
        pub fn new(cap: usize) -> Self {
            Self { cap: cap.max(1), state: Mutex::new(State { spans: VecDeque::new(), dropped: 0, next_id: 1 }) }
        }

        pub fn take(&self) -> Taken {
            let mut g = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let spans = g.spans.drain(..).collect();
            let dropped = std::mem::take(&mut g.dropped);
            Taken { spans, dropped }
        }
    }

    impl Subscriber for Collector {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, attrs: &Attributes<'_>) -> Id {
            let mut g = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let id = g.next_id;
            g.next_id = g.next_id.saturating_add(1).max(1);
            let entry = serde_json::json!({ "span": attrs.metadata().name(), "id": id }).to_string();
            if g.spans.len() >= self.cap {
                g.spans.pop_front();
                g.dropped += 1;
            }
            g.spans.push_back(entry);
            Id::from_u64(id)
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}
        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
        fn event(&self, _event: &tracing::Event<'_>) {}
        fn enter(&self, _span: &Id) {}
        fn exit(&self, _span: &Id) {}
    }
}

#[cfg(feature = "diagnostics")]
pub use active::Collector;
