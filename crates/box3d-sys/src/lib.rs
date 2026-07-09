#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
compile_error!("box3d-sys (stdb flavor) targets wasm32-unknown-unknown only; use upstream box3d-sys for native builds");

// Lint scope: allows live on this module so they cover ONLY the generated bindings —
// handwritten code (stdb.rs) is fully linted. Every entry is forced by bindgen output:
// - naming: C identifiers verbatim
// - approx_constant: B3_PI must match the C header's TRUNCATED 3.14159265359f bit-for-bit;
//   f64::consts::PI would diverge from what the C side computes with
// - useless_transmute / ptr_offset_with_cast / missing_safety_doc: bindgen's bitfield
//   accessors and prelude (regenerated on version bumps — hand-fixes would be clobbered)
mod bindings {
    #![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
    #![allow(
        clippy::approx_constant,
        clippy::missing_safety_doc,
        clippy::ptr_offset_with_cast,
        clippy::useless_transmute
    )]
    include!("bindings.rs");
}
pub use bindings::*;

mod stdb;
pub use stdb::smoke;
