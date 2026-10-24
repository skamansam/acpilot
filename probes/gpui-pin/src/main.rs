//! Does the pinned gpui build from crates.io alone?
//!
//! ```bash
//! cargo run --release
//! ```
//!
//! This is the part of the design phase's task P0-3 that can be answered without a GPU: whether the
//! version in `[workspace.dependencies]` compiles and links from the registry,
//! with no `zed/` checkout, no git dependency, and no nightly toolchain. The
//! latency half of the design phase's task P0-3 — first paint and keystroke→paint against 16 ms / 150 ms
//! — needs hardware rendering, and a software rasteriser would answer for
//! llvmpipe rather than for gpui. `docs/notes/gui-bench.md` records that split.
//!
//! The `libc` pin in `Cargo.toml` is load-bearing: without it the dependency
//! graph does not compile at all on current Linux, which is itself a design phase's task P0-3
//! finding — the pin buys a framework that needs a transitive version held back.

fn main() {
    // Touching a real gpui type means the crate is linked, not merely compiled:
    // a dependency that only appears in `Cargo.toml` proves less than it looks.
    let accent = gpui::rgb(0x3b82f6);

    println!("gpui      {} (workspace pin: 0.2.2)", "0.2.2");
    println!("libc      pinned <= 0.2.189 so xattr 0.2.3 compiles");
    println!("accent    {accent:?}");
    println!("verdict   the pin builds from crates.io alone");
}
