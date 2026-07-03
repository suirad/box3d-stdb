# wasm-sse2 compat headers

`emmintrin.h` / `xmmintrin.h` vendored from
[emscripten](https://github.com/emscripten-core/emscripten) (`system/include/compat/`, main branch,
fetched 2026-07-02). MIT/NCSA dual-licensed by the emscripten authors.

They map SSE/SSE2 intrinsics onto wasm SIMD128 (`wasm_simd128.h`, a clang builtin). Used only when
the `simd` cargo feature is enabled; build.rs defines `__SSE__`/`__SSE2__` (as emcc's `-msse2`
would) and adds this dir as `-isystem`.
