// Extern symbols box3d's C needs on bare wasm32; C-side decls in shims/include.

use core::alloc::Layout;

// --- allocator ---
// b3Alloc references aligned_alloc/free unconditionally (core.c:180), so the
// symbols must exist. Layout is recovered in `free` from a header block; the
// header is `align` bytes so the returned pointer keeps the requested alignment.

#[no_mangle]
pub unsafe extern "C" fn aligned_alloc(alignment: usize, size: usize) -> *mut u8 {
    let align = alignment.max(16);
    let Some(total) = size.checked_add(align) else {
        return core::ptr::null_mut();
    };
    let Ok(layout) = Layout::from_size_align(total, align) else {
        return core::ptr::null_mut();
    };
    let base = std::alloc::alloc(layout);
    if base.is_null() {
        return base;
    }
    let user = base.add(align);
    (user.sub(16) as *mut usize).write(total);
    (user.sub(8) as *mut usize).write(align);
    user
}

#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut u8 {
    aligned_alloc(16, size)
}

#[no_mangle]
pub unsafe extern "C" fn free(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let total = (ptr.sub(16) as *const usize).read();
    let align = (ptr.sub(8) as *const usize).read();
    std::alloc::dealloc(
        ptr.sub(align),
        Layout::from_size_align_unchecked(total, align),
    );
}

// --- libm ---
// Only the functions clang can't lower to native wasm ops.

#[no_mangle]
pub extern "C" fn sinf(x: f32) -> f32 {
    libm::sinf(x)
}

#[no_mangle]
pub extern "C" fn cosf(x: f32) -> f32 {
    libm::cosf(x)
}

#[no_mangle]
pub extern "C" fn tanf(x: f32) -> f32 {
    libm::tanf(x)
}

#[no_mangle]
pub extern "C" fn asinf(x: f32) -> f32 {
    libm::asinf(x)
}

#[no_mangle]
pub extern "C" fn acosf(x: f32) -> f32 {
    libm::acosf(x)
}

#[no_mangle]
pub extern "C" fn atanf(x: f32) -> f32 {
    libm::atanf(x)
}

#[no_mangle]
pub extern "C" fn atan2f(y: f32, x: f32) -> f32 {
    libm::atan2f(y, x)
}

#[no_mangle]
pub extern "C" fn fmodf(x: f32, y: f32) -> f32 {
    libm::fmodf(x, y)
}

#[no_mangle]
pub extern "C" fn remainderf(x: f32, y: f32) -> f32 {
    libm::remainderf(x, y)
}

#[no_mangle]
pub extern "C" fn powf(x: f32, y: f32) -> f32 {
    libm::powf(x, y)
}

#[no_mangle]
pub extern "C" fn expf(x: f32) -> f32 {
    libm::expf(x)
}

#[no_mangle]
pub extern "C" fn logf(x: f32) -> f32 {
    libm::logf(x)
}

// --- smoke ---

extern "C" {
    fn b3stdb_smoke(steps: i32) -> f32;
}

/// Create a world, drop a sphere, step, return its final height.
/// Exists to keep the whole world-step call graph live for the link gate.
pub fn smoke(steps: i32) -> f32 {
    unsafe { b3stdb_smoke(steps) }
}

/// cdylib export — keeps the gate call graph in the linked artifact.
#[no_mangle]
pub extern "C" fn box3d_smoke(steps: i32) -> f32 {
    smoke(steps)
}
