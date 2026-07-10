//! `demo-module` bouncing-ball lifecycle, with the 60 Hz tick driven by a `#[procedure]` instead
//! of a reducer. The world is mirrored (default `WorldDef`); to switch to ephemeral, pass:
//!     WorldDef { persistence: Persistence::Ephemeral, ..Default::default() }
//! to `create_world` (see `ephemeral-demo` for the cost/trade-off rationale).
//!
//! - `create_world` — registers the world, spawns the scene on the first procedure tick, arms the
//!   scheduled `tick_proc`.
//! - `create_world_untick` — registers a world WITHOUT the scheduled tick, so `burst_sim` can drive
//!   it by hand without the 60 Hz `tick_proc` stepping it concurrently.
//! - `tick_proc` — `#[procedure]`, NOT auto-transactional; each firing wraps one `try_with_tx`
//!   call so an Err from the generation guard aborts rather than commits.
//! - `burst_sim` — a bounded 30-step self-pacing loop using `ctx.sleep_until`, demonstrating the
//!   procedure-native loop mode without a scheduler table. Run it on a `create_world_untick` world.
//! - `jump` — player-action reducer (reducers remain the right tool for client requests).
//! - `ball_heights` — public view projecting the private mirror.
//! - `teardown_world` — stops the tick and destroys the world.

use box3d::{BodyDef, ShapeDef, Vec3};
// Brings `.b3_body()` into scope for view contexts; the mirror accessor trait is separate from
// ReducerContext's.
use box3d_stdb::b3_body__view;
use spacetimedb::{
    procedure, view, AnonymousViewContext, ProcedureContext, ReducerContext, ScheduleAt,
    SpacetimeType, Table, TimeDuration,
};

const GROUND_KEY: u64 = 1;
const BALL_KEY: u64 = 2;
const DT: f32 = 1.0 / 60.0;
const SUBSTEPS: i32 = 4;

#[spacetimedb::reducer(init)]
pub fn init(_ctx: &ReducerContext) {
    box3d_stdb::install_box3d_logging();
}

// `scheduled(tick_proc)` names a `#[procedure]`, not a reducer — the table macro accepts either (FnKind).
#[spacetimedb::table(accessor = tick_timer, scheduled(tick_proc))]
pub struct TickTimer {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
    pub world_key: u64,
}

fn spawn_scene(w: &mut box3d_stdb::WorldCtx<'_>) -> Result<(), String> {
    let ground = w.spawn(GROUND_KEY, BodyDef::static_at(Vec3::new(0.0, 0.0, -1.0)))?;
    ground.create_box(Vec3::new(50.0, 50.0, 1.0), ShapeDef::default());
    // Fresh world: ball at z=5. On rebuild the glue overlays the surviving mirror row
    // (position + velocity) so a mid-flight ball resumes its arc.
    let ball = w.spawn(BALL_KEY, BodyDef::dynamic_at(Vec3::new(0.0, 0.0, 5.0)))?;
    ball.create_sphere(Vec3::ZERO, 0.5, ShapeDef { density: 1.0, ..ShapeDef::default() });
    Ok(())
}

#[derive(SpacetimeType)]
pub struct BallHeight {
    pub body_key: u64,
    pub z: f32,
}

/// Clients subscribe to this instead of the (private) mirror: a projected column subset.
#[view(accessor = ball_heights, public)]
fn ball_heights(ctx: &AnonymousViewContext) -> Vec<BallHeight> {
    ctx.db
        .b3_body()
        .world_key()
        .filter(0u64..)
        .filter(|r| r.body_key == BALL_KEY)
        .map(|r| BallHeight { body_key: r.body_key, z: r.pz })
        .collect()
}

/// Create world `world_key` and start ticking it at 60 Hz via a scheduled procedure.
#[spacetimedb::reducer]
pub fn create_world(ctx: &ReducerContext, world_key: u64) -> Result<(), String> {
    if ctx.db.tick_timer().iter().any(|t| t.world_key == world_key) {
        return Err(format!("world {world_key} already ticking"));
    }
    box3d_stdb::create_world(ctx, world_key, &box3d_stdb::WorldDef::default())?;
    ctx.db.tick_timer().insert(TickTimer {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(16_667)),
        world_key,
    });
    Ok(())
}

/// Register a world WITHOUT arming the scheduled tick, so it can be driven by hand (e.g.
/// `burst_sim`) without the 60 Hz `tick_proc` stepping it concurrently.
#[spacetimedb::reducer]
pub fn create_world_untick(ctx: &ReducerContext, world_key: u64) -> Result<(), String> {
    box3d_stdb::create_world(ctx, world_key, &box3d_stdb::WorldDef::default())
}

// `try_with_tx` rather than `with_tx`: an Err from the generation guard must ABORT the tx,
// not commit it. `with_tx` always commits on return, so a guard Err would be persisted — wrong.
// `wk` is copied out before the Fn closure: `try_with_tx` may re-invoke the closure on a tx
// conflict, so the closure must be callable twice (no moved non-Copy captures).
#[procedure]
pub fn tick_proc(ctx: &mut ProcedureContext, timer: TickTimer) -> Result<(), String> {
    let wk = timer.world_key;
    ctx.try_with_tx(|tx| {
        // `spawn_scene` is a named fn (no captures) → trivially Fn, safe to pass directly.
        box3d_stdb::with_world_paced(tx, wk, spawn_scene, |_w| Ok(()))
            .map(|_| ())
    })
}

/// Step the world 30 times, self-paced via `ctx.sleep_until` — one tx per step, no scheduler
/// table. Demonstrates the procedure-native loop mode: acquire tx → step → release tx → sleep.
/// No tx is held across the sleep, which is required (`sleep_until` suspends the procedure).
/// Drive a `create_world_untick` world so the scheduled `tick_proc` isn't also stepping it.
#[procedure]
pub fn burst_sim(ctx: &mut ProcedureContext, world_key: u64) -> Result<(), String> {
    let wk = world_key;
    for _ in 0..30 {
        ctx.try_with_tx(|tx| {
            box3d_stdb::with_world(tx, wk, DT, SUBSTEPS, spawn_scene, |_w| Ok(()))
                .map(|_| ())
        })?;
        // Pace the next step to one DT after the current procedure timestamp; sleep_until
        // updates ctx.timestamp to the actual wake time so each target accumulates correctly.
        let next = ctx.timestamp + TimeDuration::from_micros(16_667);
        ctx.sleep_until(next);
    }
    Ok(())
}

/// Stop ticking and destroy the world.
#[spacetimedb::reducer]
pub fn teardown_world(ctx: &ReducerContext, world_key: u64) -> Result<(), String> {
    let stale: Vec<u64> = ctx
        .db
        .tick_timer()
        .iter()
        .filter(|t| t.world_key == world_key)
        .map(|t| t.scheduled_id)
        .collect();
    for id in stale {
        ctx.db.tick_timer().scheduled_id().delete(id);
    }
    box3d_stdb::destroy_world(ctx, world_key)
}

/// Jump if the ball is resting on the ground. Player actions stay reducers — auto-transactional
/// is exactly right here; only the tick needs procedure-mode abort control.
#[spacetimedb::reducer]
pub fn jump(ctx: &ReducerContext, world_key: u64) -> Result<(), String> {
    box3d_stdb::with_world(ctx, world_key, DT, SUBSTEPS, spawn_scene, |w| {
        let id = w.body_id(BALL_KEY).ok_or("no ball")?;
        let t = id.transform().ok_or("ball body invalid")?;
        // Grounded = resting height (radius 0.5 on ground top z=0) + small tolerance.
        if t.p.z <= 0.51 {
            // A resting ball is asleep and would ignore the velocity set.
            w.wake(BALL_KEY)?;
            id.set_linear_velocity(box3d::Vec3::new(0.0, 0.0, 6.0));
        }
        Ok(())
    })
    .map(|_| ())
}
