//! Measurement must refuse incorrect answers even when row counts and value lengths stay the same.
#[path = "support/reads.rs"]
mod reads;

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use elmdb::engine::batch::{Batch, Op};

#[test]
fn read_measurements_reject_a_wrong_middle_value_without_a_size_change() {
    let messages = reads::Messages::new(1_000);
    for workload in messages.workloads() {
        workload.assert_answer(&messages.db, messages.rows);
    }
    messages
        .db
        .write(
            &Batch {
                ops: vec![Op::Set {
                    id: "Message-00000742".to_owned(),
                    fields: BTreeMap::from([("title".to_owned(), "message 000".into())]),
                    touch: false,
                }],
                ..Default::default()
            },
            false,
        )
        .unwrap();
    for workload in messages.workloads() {
        assert!(
            catch_unwind(AssertUnwindSafe(|| workload.assert_answer(&messages.db, messages.rows))).is_err(),
            "{} must refuse the altered fixture before it measures anything",
            workload.name
        );
    }
    std::hint::black_box(&messages.directory);
}
