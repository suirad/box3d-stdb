//! A bouncing-ball consumer of `box3d-stdb`, showing the full lifecycle on the keyed mirror API:
//!
//! - `create_world` — registers the world's definition and starts a scheduled 60 Hz `tick`; the
//!   first tick builds the C world, with `spawn_scene` (the rebuild closure) spawning a keyed
//!   ground box and dynamic ball.
//! - `tick` — steps the world each firing; the glue commits the ball's transform and velocity to
//!   its private mirror row.
//! - `ball_heights` — a public view projecting the private mirror down to key + height; this is
//!   what clients subscribe to.
//! - `jump` — game logic in a `with_world` closure: launch the ball, but only if it's resting
//!   on the ground.
//! - `teardown_world` — stops the tick and destroys the world.
//!
//! `spawn_scene` doubles as crash/republish recovery: `with_world` reruns it whenever the cached
//! world is lost or stale, and surviving mirror rows restore each body's position and velocity.
//!
//! Try it: `spacetime call <db> create_world 1`, watch `select * from ball_heights`, then
//! `spacetime call <db> jump 1`.

use box3d::{BodyDef, ShapeDef, Vec3};
// Brings the `.b3_body()` accessor into scope for view contexts (their read-only db handle uses
// a separate generated trait than ReducerContext's); the mirror table lives in box3d-stdb.
use box3d_stdb::b3_body__view;
use spacetimedb::{
    view, AnonymousViewContext, ReducerContext, ScheduleAt, SpacetimeType, Table, TimeDuration,
};

const GROUND_KEY: u64 = 1;
const BALL_KEY: u64 = 2;
const DT: f32 = 1.0 / 60.0;
const SUBSTEPS: i32 = 4;

#[spacetimedb::reducer(init)]
pub fn init(_ctx: &ReducerContext) {
    // box3d warnings (b3Log) -> module log; without this they vanish.
    box3d_stdb::install_box3d_logging();
}

#[spacetimedb::table(accessor = tick_timer, scheduled(tick))]
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
    // Fresh world: ball starts at z=5. On rebuild the glue overlays the surviving mirror row
    // (position AND velocity), so a mid-flight ball resumes its arc — no manual restore.
    let ball = w.spawn(BALL_KEY, BodyDef::dynamic_at(Vec3::new(0.0, 0.0, 5.0)))?;
    ball.create_sphere(
        Vec3::ZERO,
        0.5,
        ShapeDef {
            density: 1.0,
            ..ShapeDef::default()
        },
    );
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
        .map(|r| BallHeight {
            body_key: r.body_key,
            z: r.pz,
        })
        .collect()
}

/// Create world `world_key` and start ticking it at 60 Hz.
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

/// Scheduled step; wall-clock paced — runs however many fixed-DT steps real time owes (≤4).
#[spacetimedb::reducer]
pub fn tick(ctx: &ReducerContext, timer: TickTimer) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err("tick may only be called by the scheduler".into());
    }
    box3d_stdb::with_world_paced(ctx, timer.world_key, DT, SUBSTEPS, 4, spawn_scene, |_w| Ok(()))
        .map(|_| ())
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

/// Jump if the ball is resting on the ground.
#[spacetimedb::reducer]
pub fn jump(ctx: &ReducerContext, world_key: u64) -> Result<(), String> {
    box3d_stdb::with_world(
        ctx,
        world_key,
        DT,
        SUBSTEPS,
        spawn_scene,
        |w| {
            let id = w.body_id(BALL_KEY).ok_or("no ball")?;
            let t = id.transform().ok_or("ball body invalid")?;
            // Grounded = resting height (radius 0.5 on ground top z=0) + small tolerance; a
            // contact check replaces this when step events are consumed.
            if t.p.z <= 0.51 {
                // A resting ball is asleep and would ignore the velocity set.
                w.wake(BALL_KEY)?;
                id.set_linear_velocity(box3d::Vec3::new(0.0, 0.0, 6.0));
            }
            Ok(())
        },
    )
    .map(|_| ())
}
