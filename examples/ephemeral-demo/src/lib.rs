//! The `demo-module` bouncing ball, rebuilt on the opt-out-of-mirror-cost path:
//!
//! - **Ephemeral world** — `Persistence::Ephemeral` means no durable physics state: the glue
//!   never writes `b3_body` mirror rows, so stepping does no mirror I/O. The trade: any cache
//!   drop (abort, republish, restart) resets the world to whatever `spawn_scene` spawns — here,
//!   the ball back at z=5.
//! - **Event-table broadcast** — with no mirror to subscribe to, per-tick positions go out
//!   through `ball_pos`, an event table: rows are never stored, subscribers just receive
//!   `onInsert` for each. Subscribe to `ball_pos` and render the stream.
//!
//! Try it: `spacetime call <db> create_world 1`, subscribe to `ball_pos`, then
//! `spacetime call <db> jump 1`.

use box3d::{BodyDef, ShapeDef, Vec3};
// Brings the `.b3_world()` accessor into scope; the glue's world stamp is private but readable
// from module code, and its tick numbers the broadcast stream.
use box3d_stdb::b3_world;
use spacetimedb::{ReducerContext, ScheduleAt, Table, TimeDuration};

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

/// Broadcast-only position ticks: rows are never stored — subscribers receive onInsert and
/// the row vanishes. The transient-data channel for ephemeral worlds.
#[spacetimedb::table(accessor = ball_pos, public, event)]
pub struct BallPos {
    pub world_key: u64,
    pub body_key: u64,
    pub tick: u64,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

fn spawn_scene(w: &mut box3d_stdb::WorldCtx<'_>) -> Result<(), String> {
    // Disable sleeping world-wide: the id-tier API has no wake call yet, and a slept ball would
    // ignore set_linear_velocity — revisit when a wake helper lands.
    w.world().set_sleeping_enabled(false);
    let ground = w.spawn(GROUND_KEY, BodyDef::static_at(Vec3::new(0.0, 0.0, -1.0)))?;
    ground.create_box(Vec3::new(50.0, 50.0, 1.0), ShapeDef::default());
    // No mirror overlay on rebuild (ephemeral): the ball always restarts at z=5.
    let ball = w.spawn(BALL_KEY, BodyDef::dynamic_at(Vec3::new(0.0, 0.0, 5.0)))?;
    ball.create_sphere(Vec3::ZERO, 0.5, ShapeDef { density: 1.0, ..ShapeDef::default() });
    Ok(())
}

/// Create world `world_key` and start ticking it at 60 Hz.
#[spacetimedb::reducer]
pub fn create_world(ctx: &ReducerContext, world_key: u64) -> Result<(), String> {
    if ctx.db.tick_timer().iter().any(|t| t.world_key == world_key) {
        return Err(format!("world {world_key} already ticking"));
    }
    box3d_stdb::create_world(
        ctx,
        world_key,
        &box3d_stdb::WorldDef {
            persistence: box3d_stdb::Persistence::Ephemeral,
            ..Default::default()
        },
    )?;
    ctx.db.tick_timer().insert(TickTimer {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(16_667)),
        world_key,
    });
    Ok(())
}

/// Scheduled step: one physics tick per firing, moves piped to the `ball_pos` event table.
#[spacetimedb::reducer]
pub fn tick(ctx: &ReducerContext, timer: TickTimer) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err("tick may only be called by the scheduler".into());
    }
    let res =
        box3d_stdb::with_world(ctx, timer.world_key, DT, SUBSTEPS, spawn_scene, |_w| Ok(()))?;
    let tick = ctx.db.b3_world().world_key().find(timer.world_key).map_or(0, |w| w.tick);
    for m in &res.events.moves {
        ctx.db.ball_pos().insert(BallPos {
            world_key: timer.world_key,
            body_key: m.body_key,
            tick,
            x: m.position.0,
            y: m.position.1,
            z: m.position.2,
        });
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
                id.set_linear_velocity(box3d::Vec3::new(0.0, 0.0, 6.0));
            }
            Ok(())
        },
    )
    .map(|_| ())
}
