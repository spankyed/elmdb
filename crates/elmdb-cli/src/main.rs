//! The `elmdb` command. Its subcommands (`inspect`, `check`, `backup`, `upgrade`) arrive with the engine's admin
//! operations.

fn main() {
    eprintln!("elmdb {}: no subcommands yet", env!("CARGO_PKG_VERSION"));
    std::process::exit(2);
}
