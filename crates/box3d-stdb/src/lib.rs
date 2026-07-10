//! [box3d](https://github.com/erincatto/box3d) 3D physics for SpacetimeDB modules — worlds that
//! survive reducer aborts, republishes, and region migration.
//!
//! A SpacetimeDB module can't keep a physics world in a static: a panic rolls back your tables
//! but **not** wasm linear memory, instance memory dies on republish, and the cloud may move a
//! database to a machine with empty memory. `box3d-stdb` makes **your tables the record of
//! truth** and treats the live `b3World` as a generation-guarded cache — validated against a
//! durable stamp on every tick, rebuilt from tables the moment trust is lost.
//!
//! Consumed as a git dependency plus `[patch.crates-io]` for the wasm-ready `box3d-sys` — see
//! the [repository README](https://github.com/suirad/box3d-stdb) for the two `Cargo.toml` lines.
//!
//! # One world, one tick
//!
//! ```ignore
//! use box3d::{BodyDef, ShapeDef, Vec3};
//! use box3d_stdb::{create_world, with_world_paced, WorldDef};
//!
//! // Once, e.g. match setup — the definition (physics AND tick policy: dt, substeps,
//! // catch-up) is stored durably; rebuilds and callers can never diverge from it.
//! create_world(ctx, ARENA, &WorldDef::default())?;
//!
//! // 60 Hz scheduled reducer: reconcile → your logic → step → commit, one transaction.
//! #[spacetimedb::reducer]
//! fn tick(ctx: &ReducerContext, t: TickTimer) -> Result<(), String> {
//!     let res = with_world_paced(ctx, t.world_key,
//!         // rebuild: construction only, replayed from YOUR tables after any cache drop
//!         |w| {
//!             let ball = w.spawn(BALL, BodyDef::dynamic_at(Vec3::new(0.0, 0.0, 5.0)))?;
//!             ball.create_sphere(Vec3::ZERO, 0.5, ShapeDef { density: 1.0, ..<_>::default() });
//!             Ok(())
//!         },
//!         // game: per-tick logic, before integration
//!         |w| Ok(()),
//!     )?;
//!     for hit in &res.events.hits { /* contact point, normal, approach speed */ }
//!     Ok(())
//! }
//! ```
//!
//! The `tick_schedule!` macro generates the scheduled table's reducer + arm helper, so the
//! boilerplate above collapses to one invocation plus three named fns.
//!
//! Body transforms land in the `b3_body` mirror table, delta-committed from move events —
//! sleeping bodies cost nothing. Expose it through your own `#[view]` (or the `public-mirror`
//! feature) and clients render straight off a subscription.
//!
//! # Shoot things
//!
//! [`WorldCtx`]'s keyed gameplay tier — consumer keys in, consumer keys out, no raw ids:
//!
//! ```ignore
//! // Raycast shot: closest hit arrives with the body key already resolved.
//! if let Some(hit) = w.cast_ray_closest(muzzle, aim * 100.0) {
//!     if let Some(key) = hit.body_key {
//!         w.apply_impulse(key, aim * 50.0, hit.point)?; // wakes the sleeper first — never a no-op
//!     }
//! }
//!
//! // AOE: every glue-spawned body in the blast radius, deduped.
//! for key in w.overlap_sphere(blast, 5.0) {
//!     let v = launch_speed * w.mass(key)?;              // impulse = mass * target velocity
//!     w.apply_impulse_to_center(key, Vec3::new(0.0, 0.0, v))?;
//! }
//! ```
//!
//! # Realtime that stays realtime
//!
//! SpacetimeDB's scheduler under-fires (~8% measured) — a naive step-per-firing tick silently
//! runs slow. [`with_world_paced`] repays wall-clock time in whole fixed-`dt` steps: fixed-step
//! determinism, 0.3% sim-vs-wall deviation measured, and overload degrades to the maximum
//! sustainable rate then self-recovers instead of death-spiraling.
//!
//! # Pay for activity, not for existing
//!
//! An adaptive [`TickPolicy`] clocks slow scenes down a tier ladder (fewer substeps, then bigger
//! fixed `dt` behind a velocity gate that doubles as the tunneling-safety proof) and, once fully
//! asleep, returns [`TickDirective::Park`] — stop the timer, the world costs zero until a
//! mutation wakes it ([`resume_full_rate`] + re-arm, both wrapped by `tick_schedule!`'s `ensure`
//! helper). Measured: 20 steps/s at the bottom tier vs 60 at full, 6× less metered work on a
//! slow-creeping pile, instant promotion on fast movers.
//!
//! # Ephemeral worlds
//!
//! ```ignore
//! create_world(ctx, LOBBY, &WorldDef { persistence: Persistence::Ephemeral, ..<_>::default() })?;
//! ```
//!
//! No mirror I/O at all — stepping cost equals raw box3d (measured). Positions stream per tick
//! via `StepResult.events.moves`; the abort-safety guard still applies, a cache drop just
//! respawns fresh. For lobby matches and cosmetic sims.
//!
//! # Ticking from a procedure
//!
//! `TxContext` derefs to `ReducerContext`, so the same API runs inside a `#[procedure]`:
//!
//! ```ignore
//! ctx.try_with_tx(|tx| with_world_paced(tx, wk, rebuild, game).map(|_| ()))
//! ```
//!
//! `try_with_tx`, not `with_tx`: a guard `Err` must abort the transaction, never commit.
//!
//! # What survives what
//!
//! | event | outcome |
//! |---|---|
//! | reducer abort / panic | tables roll back; the cache is discarded and rebuilt next tick |
//! | republish / restart | rebuild from tables — a mid-flight ball resumes its arc, velocity intact |
//! | region migration (empty memory) | same cold rebuild; the cache is never trusted without the stamp |
//!
//! The construction contract: the glue persists *dynamics* (pose/velocity/sleep). Shapes,
//! densities, and joints are yours to persist in your own tables and re-apply in `rebuild` —
//! exactly like spawns.
//!
//! # Tables this crate adds to your module
//!
//! Two, both **private by default**:
//!
//! - `b3_world` ([`B3WorldRow`]) — one row per world: the generation stamp the cache is
//!   validated against, the committed `tick` count, the pacing frontier, and the stored
//!   [`WorldDef`]. Written by [`create_world`] and every tick.
//! - `b3_body` ([`B3BodyRow`]) — one row per spawned body (mirrored worlds only; ephemeral
//!   worlds never write here): transform, velocities, sleep flag.
//!
//! Their lifecycle is owned by [`create_world`] / [`destroy_world`] — don't insert or edit rows
//! by hand (a hand-bumped generation just forces a rebuild; a hand-edited body row is
//! overwritten by the next committed step or restored on rebuild, whichever comes first).
//! `destroy_world` deletes a world's rows; orphans from aborted transactions are swept
//! automatically. Reading is fine and encouraged: project `b3_body` through a `#[view]` for
//! clients (row types and accessor traits are re-exported for exactly that), or expose `tick`
//! from `b3_world` as a match clock.
//!
//! Measurements, tuning, and full walkthroughs: the
//! [repository README](https://github.com/suirad/box3d-stdb) and `examples/` (mirrored demo,
//! ephemeral broadcast, procedure tick, and a multiplayer sandbox with a three.js web client).

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
compile_error!("box3d-stdb targets wasm32-unknown-unknown only (SpacetimeDB modules)");

pub use box3d;

mod world;
// Glob is deliberate: consumer `#[view]`s need the row types and the
// macro-generated accessor traits, whose names aren't stable API to enumerate.
pub use world::*;

mod tick_macro;

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
