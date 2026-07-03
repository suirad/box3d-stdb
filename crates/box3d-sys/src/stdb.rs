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

// --- printf ---
// b3Log formats through vsnprintf BEFORE its log callback (core.c), and bare
// wasm32 has no formatter — the C shim forwards here. Covers box3d's actual
// specifier census (%d %i %u %x %s %f, precision, l/ll); unknown specifiers
// emit literally. %g renders via Rust's `{}` (shortest roundtrip).
// On wasm32, clang's va_list is a pointer into an arg buffer with args at
// natural alignment (ints promoted to i32, floats to f64).

struct VaArgs(*const u8);

impl VaArgs {
    unsafe fn read<T: Copy>(&mut self) -> T {
        let align = core::mem::align_of::<T>();
        let addr = (self.0 as usize + align - 1) & !(align - 1);
        let v = (addr as *const T).read();
        self.0 = (addr + core::mem::size_of::<T>()) as *const u8;
        v
    }
}

/// snprintf semantics: writes are clamped to `cap - 1` + NUL, `len` tracks the
/// untruncated length for the return value.
struct CBuf {
    ptr: *mut u8,
    cap: usize,
    len: usize,
}

impl CBuf {
    fn push(&mut self, b: u8) {
        if self.cap > 0 && self.len < self.cap - 1 {
            unsafe { self.ptr.add(self.len).write(b) };
        }
        self.len += 1;
    }
}

impl core::fmt::Write for CBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            self.push(b);
        }
        Ok(())
    }
}

#[no_mangle]
pub unsafe extern "C" fn b3stdb_vsnprintf(
    buf: *mut u8,
    n: usize,
    fmt: *const core::ffi::c_char,
    args: *mut core::ffi::c_void,
) -> i32 {
    use core::fmt::Write;

    let mut out = CBuf { ptr: buf, cap: n, len: 0 };
    let mut ap = VaArgs(args as *const u8);
    let f = core::ffi::CStr::from_ptr(fmt).to_bytes();

    let mut i = 0;
    while i < f.len() {
        if f[i] != b'%' {
            out.push(f[i]);
            i += 1;
            continue;
        }
        let spec_start = i;
        i += 1;
        // flags/width — none in box3d's census; skip so they can't derail parsing
        while i < f.len() && matches!(f[i], b'-' | b'+' | b' ' | b'0'..=b'9') {
            i += 1;
        }
        let mut prec: Option<usize> = None;
        if i < f.len() && f[i] == b'.' {
            i += 1;
            let mut p = 0usize;
            while i < f.len() && f[i].is_ascii_digit() {
                p = p * 10 + (f[i] - b'0') as usize;
                i += 1;
            }
            prec = Some(p);
        }
        let mut long_long = false;
        while i < f.len() && f[i] == b'l' {
            // wasm32: long == int, so only `ll` widens
            long_long = i + 1 < f.len() && f[i + 1] == b'l';
            i += 1;
        }
        let conv = if i < f.len() { f[i] } else { 0 };
        i += 1;
        match conv {
            b'%' => out.push(b'%'),
            b'd' | b'i' => {
                if long_long {
                    let v: i64 = ap.read();
                    let _ = write!(out, "{v}");
                } else {
                    let v: i32 = ap.read();
                    let _ = write!(out, "{v}");
                }
            }
            b'u' => {
                if long_long {
                    let v: u64 = ap.read();
                    let _ = write!(out, "{v}");
                } else {
                    let v: u32 = ap.read();
                    let _ = write!(out, "{v}");
                }
            }
            b'x' | b'X' => {
                if long_long {
                    let v: u64 = ap.read();
                    let _ = write!(out, "{v:x}");
                } else {
                    let v: u32 = ap.read();
                    let _ = write!(out, "{v:x}");
                }
            }
            b'f' | b'F' => {
                let v: f64 = ap.read();
                let _ = write!(out, "{v:.p$}", p = prec.unwrap_or(6));
            }
            b'g' | b'G' | b'e' | b'E' => {
                let v: f64 = ap.read();
                let _ = write!(out, "{v}");
            }
            b's' => {
                let p: *const core::ffi::c_char = ap.read();
                if p.is_null() {
                    let _ = out.write_str("(null)");
                } else {
                    for &b in core::ffi::CStr::from_ptr(p).to_bytes() {
                        out.push(b);
                    }
                }
            }
            // unknown/unsupported: emit the raw specifier so the message stays legible
            _ => {
                for &b in &f[spec_start..i] {
                    out.push(b);
                }
            }
        }
    }

    if n > 0 {
        let end = out.len.min(n - 1);
        buf.add(end).write(0);
    }
    out.len as i32
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
