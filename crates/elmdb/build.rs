//! Refuses a build that would give the collator other data than its crates hold.
//!
//! `ICU4X_DATA_DIR` makes ICU4X's data crates bake the data of a directory in place of their own. A sort key made
//! from other data is another collator's under the same crate versions, which is all the stamp of an index's order
//! holds of them (`src/engine/order.rs`): the order would be read stale, wrong and with no error.
//!
//! And links the instruction-count benchmark so that Valgrind reads its symbols, whatever its size (`main`, below).

fn main() {
    println!("cargo:rerun-if-env-changed=ICU4X_DATA_DIR");
    if std::env::var_os("ICU4X_DATA_DIR").is_some() {
        panic!(
            "ICU4X_DATA_DIR is set, which replaces the collation data elmdb's stored sort keys are stamped with: \
             unset it and build again"
        );
    }
    // `benches/instructions.rs` is counted from the function Callgrind finds by name, so Valgrind has to read the
    // binary's symbols. It reads them once it has seen a mapping for each of the binary's writable segments, and the
    // linker makes two, the first read-only after relocation (RELRO). Where the first happens to end on a page
    // boundary the two are mapped as one, Valgrind 3.22 waits for a second that never comes, reads no symbols and
    // counts nothing, in every case of the benchmark. Whether it ends there follows the binary's size, so any change
    // to the engine could bring it. Without RELRO there is one writable segment. Binding stays eager, so nothing in
    // what is counted changes
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg-bench=instructions=-Wl,-z,norelro");
    }
}
