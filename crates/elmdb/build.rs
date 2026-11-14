//! Refuses a build that would give the collator other data than its crates hold.
//!
//! `ICU4X_DATA_DIR` makes ICU4X's data crates bake the data of a directory in place of their own. A sort key made
//! from other data is another collator's under the same crate versions, which is all the stamp of an index's order
//! holds of them (`src/engine/order.rs`): the order would be read stale, wrong and with no error.

fn main() {
    println!("cargo:rerun-if-env-changed=ICU4X_DATA_DIR");
    if std::env::var_os("ICU4X_DATA_DIR").is_some() {
        panic!(
            "ICU4X_DATA_DIR is set, which replaces the collation data elmdb's stored sort keys are stamped with: \
             unset it and build again"
        );
    }
}
