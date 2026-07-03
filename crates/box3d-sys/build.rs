use std::path::PathBuf;

// All TUs compile — recording hooks (b3RecWrite_*) are woven into the live
// step path, so recording.c can't be excluded. Its file I/O is inert at
// runtime: the shim fopen always returns NULL.
fn main() {
    // Fail loud before cc produces confusing native errors; lib.rs's
    // compile_error! only fires after the build script has already run.
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("wasm32") {
        panic!("box3d-sys (stdb flavor) builds for wasm32-unknown-unknown only; use upstream box3d-sys for native targets");
    }

    let src = PathBuf::from("../../vendor/box3d/src");
    let mut build = cc::Build::new();

    for entry in std::fs::read_dir(&src).expect("box3d submodule missing — run: git submodule update --init") {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "c") {
            build.file(&path);
        }
    }
    build.file("shims/shim.c");
    build.file("shims/smoke.c");

    build
        .include("../../vendor/box3d/include")
        .include(&src)
        .std("c17")
        // Assert path calls printf under !NDEBUG; force it off for all consumer profiles.
        .define("NDEBUG", None);

    // constants.h guards with #ifndef — pure flag override, no patch.
    // Upstream default 128; must stay < 65535 (physics_world.c static assert).
    let max_worlds = std::env::var("BOX3D_MAX_WORLDS").unwrap_or_else(|_| "1024".into());
    build.define("B3_MAX_WORLDS", max_worlds.as_str());

    // Env escape hatch: features are additive and the wrapper enables our
    // defaults, so scalar can only be forced from outside the feature system.
    let force_scalar = std::env::var("BOX3D_FORCE_SCALAR").is_ok_and(|v| v != "0");
    if std::env::var("CARGO_FEATURE_SIMD").is_ok() && !force_scalar {
        // Reach core.h's B3_CPU_WASM branch (only used in its SIMD cascade) and
        // satisfy <emmintrin.h> with emscripten's SSE2->wasm128 compat headers.
        build
            .define("B3_CPU_WASM", "")
            // the compat headers gate on these; emcc defines them for -msse2
            .define("__SSE__", None)
            .define("__SSE2__", None)
            .flag("-msimd128")
            .flag("-isystem")
            .flag("shims/include/wasm-sse2");
    } else {
        // Deterministic scalar path — upstream-supported flag (core.h:50).
        build.define("BOX3D_DISABLE_SIMD", None);
    }

    // Wasm-only crate: bare wasm32-unknown-unknown has no libc headers; always
    // use our minimal shim set.
    build.flag("-isystem").flag("shims/include").flag("-ffreestanding");

    build.compile("box3d");
    println!("cargo:rerun-if-changed=../../vendor/box3d/src");
    println!("cargo:rerun-if-changed=../../vendor/box3d/include");
    println!("cargo:rerun-if-changed=shims");
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=BOX3D_MAX_WORLDS");
    println!("cargo:rerun-if-env-changed=BOX3D_FORCE_SCALAR");
}
