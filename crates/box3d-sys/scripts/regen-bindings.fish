#!/usr/bin/env fish
# Regenerate src/bindings.rs from the vendored box3d headers.
# Run after bumping vendor/box3d or wrapper.h. Requires bindgen-cli.
cd (dirname (status filename))/..
bindgen wrapper.h \
    --allowlist-function 'b3.*' \
    --allowlist-type 'b3.*' \
    --allowlist-var 'b3.*' \
    --allowlist-var 'B3.*' \
    --with-derive-default \
    --no-layout-tests \
    -o src/bindings.rs \
    -- --target=wasm32-unknown-unknown \
       -I ../../vendor/box3d/include \
       -isystem shims/include \
       -ffreestanding \
       -fvisibility=default
# -fvisibility=default: wasm32 clang defaults functions to hidden visibility
# and bindgen silently skips hidden functions — without this, zero fns emit.
or exit 1  # don't let the sed below mutate a stale bindings.rs

# libclang >= ~20 reports C enums as unsigned; the box3d wrapper crate was
# generated against older libclang (enum -> int). Rewrite enum aliases to c_int
# so wrapper 0.1.14 type-checks. Safe: no box3d enum value exceeds i32::MAX.
sed -i 's/^pub type \(b3[A-Za-z0-9_]*\) = ::std::os::raw::c_uint;$/pub type \1 = ::std::os::raw::c_int;/' src/bindings.rs
