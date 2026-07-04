//! SpacetimeDB integration for box3d; re-exports the upstream `box3d` wrapper API.
//!
//! The model: **your tables are the record of truth**; the in-memory `b3World` is a rebuildable
//! cache validated by a durable generation stamp. Reducers are transactional — table writes roll
//! back on abort, linear memory does not — so the cache is never trusted without the stamp.
//! Register a world once with [`create_world`] (its definition is stored durably), then drive
//! everything through [`with_world`]; it reconciles, runs your game logic, and steps.
//!
//! ```ignore
//! box3d_stdb::create_world(ctx, world_key, &WorldDef::default())?; // once, e.g. match setup
//!
//! #[spacetimedb::reducer]
//! fn tick(ctx: &ReducerContext, timer: TickTimer) -> Result<(), String> {
//!     box3d_stdb::with_world(ctx, timer.world_key, 1.0 / 60.0, 4,
//!         |w| rebuild_bodies(ctx, w),   // construction-only, after cache drops
//!         |w| apply_inputs(ctx, w))     // per-tick game logic, before the step
//!         .map(|_| ())
//! }
//! ```
//!
//! See `examples/demo-module` for the full create → scheduled-tick → teardown lifecycle.

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
compile_error!("box3d-stdb targets wasm32-unknown-unknown only (SpacetimeDB modules)");

pub use box3d;

mod world;
// Glob is deliberate: consumer `#[view]`s need the row types and the
// macro-generated accessor traits, whose names aren't stable API to enumerate.
pub use world::*;

// Load-bearing for the cdylib link gate: keeps box3d-sys's #[no_mangle]
// exports (box3d_smoke + allocator/libm symbols) in this crate's call graph.
pub use box3d_sys::smoke;

/// Route box3d's internal warnings (`b3Log`) to the module log.
/// Call once, e.g. from the module's `init` reducer.
pub fn install_box3d_logging() {
    unsafe { box3d_sys::b3SetLogFcn(Some(log_trampoline)) };
}

unsafe extern "C" fn log_trampoline(message: *const core::ffi::c_char) {
    if message.is_null() {
        return;
    }
    let msg = unsafe { core::ffi::CStr::from_ptr(message) }.to_string_lossy();
    log::warn!("box3d: {msg}");
}

/// Live bytes in box3d's C heap (b3Alloc minus b3Free) — leak observability.
pub fn c_byte_count() -> i32 {
    unsafe { box3d_sys::b3GetByteCount() }
}
