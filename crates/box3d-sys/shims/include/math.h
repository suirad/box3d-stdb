#pragma once

// sqrtf/floorf/ceilf/fabsf/fminf/fmaxf lower to native wasm ops; the rest are
// exported from Rust's libm (src/stdb.rs).
float sinf(float);
float cosf(float);
float tanf(float);
float asinf(float);
float acosf(float);
float atanf(float);
float atan2f(float, float);
float sqrtf(float);
float floorf(float);
float ceilf(float);
float fabsf(float);
float fmodf(float, float);
float remainderf(float, float);
float powf(float, float);
float expf(float);
float logf(float);
float fminf(float, float);
float fmaxf(float, float);

// The wasm-sse2 compat headers (emscripten's SSE2->wasm128 shims) call these
// double/rounding libm functions. All lower to native wasm ops (f64.abs,
// f64.nearest + trunc conversions) — no libcall, no Rust-side export needed.
static __inline__ double fabs(double __x) { return __builtin_fabs(__x); }
static __inline__ long lrint(double __x) { return (long)__builtin_rint(__x); }
static __inline__ long lrintf(float __x) { return (long)__builtin_rintf(__x); }
static __inline__ long long llrint(double __x) { return (long long)__builtin_rint(__x); }
static __inline__ long long llrintf(float __x) { return (long long)__builtin_rintf(__x); }

#define isinf(x) __builtin_isinf(x)
#define isnan(x) __builtin_isnan(x)
#define isfinite(x) __builtin_isfinite(x)

#define INFINITY (__builtin_inff())
#define NAN (__builtin_nanf(""))
#define FLT_EPSILON __FLT_EPSILON__
