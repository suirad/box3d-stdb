// Byte-compatible allow-set with Tebarem/box3d-rs src/lib.rs — keep in sync on version bumps.
#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
#![allow(
    clippy::approx_constant,
    clippy::missing_safety_doc,
    clippy::ptr_offset_with_cast,
    clippy::useless_transmute
)]

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
compile_error!("box3d-sys (stdb flavor) targets wasm32-unknown-unknown only; use upstream box3d-sys for native builds");

include!("bindings.rs");

mod stdb;
pub use stdb::smoke;
