//! SpacetimeDB integration for box3d; re-exports the upstream `box3d` wrapper API.

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
compile_error!("box3d-stdb targets wasm32-unknown-unknown only (SpacetimeDB modules)");

pub use box3d;

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
