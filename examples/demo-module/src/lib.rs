//! A bouncing-ball consumer of `box3d-stdb`, showing the full lifecycle:
//!
//! - `create_world` — first `with_world` call auto-creates the world; `spawn_scene` (the rebuild
//!   closure) builds a ground box and a dynamic ball, then a scheduled 60 Hz `tick` starts.
//! - `tick` — steps the world each firing and publishes the ball's height to the public `ball`
//!   table for clients to subscribe to.
//! - `jump` — game logic in a `with_world` closure: launch the ball, but only if it's resting
//!   on the ground.
//! - `teardown_world` — stops the tick and destroys the world.
//!
//! `spawn_scene` doubles as crash/republish recovery: `with_world` reruns it whenever the cached
//! world is lost or stale, and the ball resumes from its last committed height.
//!
//! Try it: `spacetime call <db> create_world 1`, watch `select * from ball`, then
//! `spacetime call <db> jump 1`.

use box3d::{BodyDef, ShapeDef, Vec3};
use spacetimedb::{ReducerContext, ScheduleAt, Table, TimeDuration};

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

#[spacetimedb::table(accessor = ball, public)]
pub struct Ball {
    #[primary_key]
    pub world_key: u64,
    pub body_bits: u64,
    pub z: f32,
}

fn spawn_scene(
    ctx: &ReducerContext,
    w: &mut box3d_stdb::WorldCtx<'_>,
    world_key: u64,
) -> Result<(), String> {
    // Disable sleeping world-wide: the id-tier API has no wake call yet, and a slept ball would
    // ignore set_linear_velocity — revisit when glue mutator helpers land.
    w.world().set_sleeping_enabled(false);

    // Ground
    let ground = w
        .world()
        .create_body(BodyDef::static_at(Vec3::new(0.0, 0.0, -1.0)));
    ground
        .id()
        .create_box(Vec3::new(50.0, 50.0, 1.0), ShapeDef::default());
    // RAII Drop would destroy the C body; forget is leak-free here — Body is a thin id wrapper,
    // the C object lives in the world.
    std::mem::forget(ground);

    // Ball: resume from surviving row's height on rebuild, otherwise start at z=5.
    let z = ctx
        .db
        .ball()
        .world_key()
        .find(world_key)
        .map(|b| b.z)
        .unwrap_or(5.0);
    let body = w
        .world()
        .create_body(BodyDef::dynamic_at(Vec3::new(0.0, 0.0, z)));
    body.id().create_sphere(
        Vec3::ZERO,
        0.5,
        ShapeDef {
            density: 1.0,
            ..ShapeDef::default()
        },
    );
    let body_bits = body.id().to_bits();
    std::mem::forget(body);

    // Upsert ball row with fresh body_bits (ids don't survive rebuilds) and spawn z.
    // Velocity is lost across rebuilds (id-tier has no velocity getter) — a resting ball resumes
    // cleanly, a mid-flight one restarts from its height with zero velocity; the coming mirror
    // layer fixes this properly.
    match ctx.db.ball().world_key().find(world_key) {
        Some(row) => {
            ctx.db.ball().world_key().update(Ball {
                body_bits,
                z,
                ..row
            });
        }
        None => {
            ctx.db.ball().insert(Ball {
                world_key,
                body_bits,
                z,
            });
        }
    }

    Ok(())
}

/// Create world `world_key` and start ticking it at 60 Hz.
#[spacetimedb::reducer]
pub fn create_world(ctx: &ReducerContext, world_key: u64) -> Result<(), String> {
    if ctx.db.tick_timer().iter().any(|t| t.world_key == world_key) {
        return Err(format!("world {world_key} already ticking"));
    }
    box3d_stdb::with_world(
        ctx,
        world_key,
        &box3d_stdb::WorldParams::default(),
        |w| spawn_scene(ctx, w, world_key),
        |_w| Ok(()),
    )?;
    ctx.db.tick_timer().insert(TickTimer {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(16_667)),
        world_key,
    });
    Ok(())
}

/// Scheduled step; one physics tick per firing.
#[spacetimedb::reducer]
pub fn tick(ctx: &ReducerContext, timer: TickTimer) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err("tick may only be called by the scheduler".into());
    }
    box3d_stdb::with_world(
        ctx,
        timer.world_key,
        &box3d_stdb::WorldParams::default(),
        |w| spawn_scene(ctx, w, timer.world_key),
        |_w| Ok(()),
    )?;
    // Deliberately AFTER with_world: the game closure runs pre-step, so reading there would be
    // one tick stale. Post-return id-tier reads are valid only right here — same reducer, cache
    // still live. This hand-rolled height mirror disappears once the glue commits deltas itself.
    if let Some(ball) = ctx.db.ball().world_key().find(timer.world_key) {
        if let Some(t) = box3d::BodyId::from_bits(ball.body_bits).transform() {
            ctx.db.ball().world_key().update(Ball { z: t.p.z, ..ball });
        }
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
        &box3d_stdb::WorldParams::default(),
        |w| spawn_scene(ctx, w, world_key),
        |_w| {
            let ball = ctx.db.ball().world_key().find(world_key).ok_or("no ball")?;
            let id = box3d::BodyId::from_bits(ball.body_bits);
            let t = id.transform().ok_or("ball body invalid")?;
            // Grounded = resting height (radius 0.5 on ground top z=0) + small tolerance; a contact
            // check replaces this when step events are surfaced.
            if t.p.z <= 0.51 {
                id.set_linear_velocity(box3d::Vec3::new(0.0, 0.0, 6.0));
            }
            Ok(())
        },
    )
    .map(|_| ())
}
