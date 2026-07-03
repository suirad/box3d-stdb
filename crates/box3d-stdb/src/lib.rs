//! SpacetimeDB integration for box3d; re-exports the upstream `box3d` wrapper API.

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
compile_error!("box3d-stdb targets wasm32-unknown-unknown only (SpacetimeDB modules)");

pub use box3d;

// Load-bearing for the cdylib link gate: keeps box3d-sys's #[no_mangle]
// exports (box3d_smoke + allocator/libm symbols) in this crate's call graph.
pub use box3d_sys::smoke;
