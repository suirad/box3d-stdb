//! Drops a sphere, steps the world, records where it lands.

use box3d::{BodyDef, Quat, ShapeDef, Vec3, World};
// Brings the `.b3_body()` accessor into scope; the mirror table lives in box3d-stdb.
use box3d_stdb::b3_body;
use spacetimedb::{ReducerContext, Table};

const DT: f32 = 1.0 / 60.0;
const SUBSTEPS: i32 = 4;

#[spacetimedb::table(accessor = drop_result, public)]
pub struct DropResult {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub steps: u32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub vz: f32,
    pub mass: f32,
    pub awake: bool,
    pub gravity_z: f32,
    pub awake_bodies: i32,
}

#[spacetimedb::reducer(init)]
pub fn init(_ctx: &ReducerContext) {
    // box3d warnings (b3Log) -> module log; without this they vanish.
    box3d_stdb::install_box3d_logging();
}

#[spacetimedb::table(accessor = bench_result, public)]
pub struct BenchResult {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub bodies: u32,
    pub steps: u32,
    /// Position checksum — cross-variant determinism probe.
    pub sum_x: f64,
    pub sum_y: f64,
    pub sum_z: f64,
    pub awake_bodies: i32,
}

#[spacetimedb::table(accessor = mem_probe, public)]
pub struct MemProbe {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub label: String,
    pub world_count: i32,
    pub byte_count: i32,
}

#[spacetimedb::reducer]
pub fn make_world(ctx: &ReducerContext, world_key: u64) -> Result<(), String> {
    box3d_stdb::create_world(ctx, world_key, &box3d_stdb::WorldDef::default())
}

#[spacetimedb::reducer]
pub fn make_ephemeral(ctx: &ReducerContext, world_key: u64) -> Result<(), String> {
    box3d_stdb::create_world(
        ctx,
        world_key,
        &box3d_stdb::WorldDef {
            persistence: box3d_stdb::Persistence::Ephemeral,
            ..Default::default()
        },
    )
}

#[spacetimedb::reducer]
pub fn step_world(ctx: &ReducerContext, world_key: u64) -> Result<(), String> {
    box3d_stdb::with_world(ctx, world_key, DT, SUBSTEPS, |_w| Ok(()), |_w| Ok(())).map(|_| ())
}

#[spacetimedb::reducer]
pub fn probe_mem(ctx: &ReducerContext, label: String) {
    ctx.db.mem_probe().insert(MemProbe {
        id: 0,
        label,
        world_count: box3d::world_count(),
        byte_count: box3d_stdb::c_byte_count(),
    });
}

#[spacetimedb::reducer]
pub fn fail_rebuild(ctx: &ReducerContext, world_key: u64) -> Result<(), String> {
    box3d_stdb::with_world(
        ctx,
        world_key,
        DT,
        SUBSTEPS,
        |_| Err("intentional rebuild failure".into()),
        |_| Ok(()),
    )
    .map(|_| ())
}

#[spacetimedb::reducer]
pub fn poison_world(ctx: &ReducerContext, world_key: u64) {
    // Swallow the Err so the tx commits — the in-memory poison must persist for the next call.
    if let Err(e) = box3d_stdb::with_world(
        ctx,
        world_key,
        DT,
        SUBSTEPS,
        |_| Ok(()),
        |_| Err::<(), String>("intentional poison".into()),
    ) {
        log::warn!("poison_world {world_key}: {e}");
    }
}

#[spacetimedb::reducer]
pub fn remove_world(ctx: &ReducerContext, world_key: u64) -> Result<(), String> {
    box3d_stdb::destroy_world(ctx, world_key)
}

/// Smoke test to verify everything works
#[spacetimedb::reducer]
pub fn drop_sphere(ctx: &ReducerContext, steps: u32) {
    let world = World::new(Vec3::new(0.0, 0.0, -10.0));
    let body = world.create_body(BodyDef::dynamic_at(Vec3::new(0.0, 0.0, 10.0)));
    // ShapeDef::default() is density 0.0 => zero-mass body that ignores gravity.
    let def = ShapeDef {
        density: 1.0,
        ..ShapeDef::default()
    };
    // Shape is RAII — dropping it destroys the C shape (and zeroes body mass),
    // so it must outlive the steps.
    let _sphere = body.create_sphere(Vec3::ZERO, 0.5, def);
    for _ in 0..steps {
        world.step(1.0 / 60.0, 4);
    }
    let p = body.position();
    let v = body.linear_velocity();
    ctx.db.drop_result().insert(DropResult {
        id: 0,
        steps,
        x: p.x,
        y: p.y,
        z: p.z,
        vz: v.z,
        mass: body.mass(),
        awake: body.is_awake(),
        gravity_z: world.gravity().z,
        awake_bodies: world.awake_body_count(),
    });
}

#[spacetimedb::reducer]
pub fn mirror_spawn(ctx: &ReducerContext, world_key: u64, body_key: u64, z: f32) -> Result<(), String> {
    box3d_stdb::with_world(ctx, world_key, DT, SUBSTEPS, |_| Ok(()), |w| {
        let id = w.spawn(body_key, BodyDef::dynamic_at(Vec3::new(0.0, 0.0, z)))?;
        id.create_sphere(Vec3::ZERO, 0.5, ShapeDef { density: 1.0, ..ShapeDef::default() });
        Ok(())
    })
    .map(|_| ())
}

/// create_world + a failing step in ONE tx: the abort rolls the row back but the busy slot
/// survives in memory — exercises with_world's missing-row orphan reconcile on the next call.
#[spacetimedb::reducer]
pub fn make_and_fail(ctx: &ReducerContext, world_key: u64) -> Result<(), String> {
    box3d_stdb::create_world(ctx, world_key, &box3d_stdb::WorldDef::default())?;
    box3d_stdb::with_world(ctx, world_key, DT, SUBSTEPS, |_| Ok(()), |_| {
        Err::<(), String>("intentional failure after create".into())
    })
    .map(|_| ())
}

/// Game-closure Err PROPAGATED (unlike poison_world's swallow): tx aborts, slot stays poisoned.
#[spacetimedb::reducer]
pub fn fail_game(ctx: &ReducerContext, world_key: u64) -> Result<(), String> {
    box3d_stdb::with_world(ctx, world_key, DT, SUBSTEPS, |_| Ok(()), |_| {
        Err::<(), String>("intentional game failure".into())
    })
    .map(|_| ())
}

#[spacetimedb::reducer]
pub fn mirror_ground(ctx: &ReducerContext, world_key: u64, body_key: u64) -> Result<(), String> {
    box3d_stdb::with_world(ctx, world_key, DT, SUBSTEPS, |_| Ok(()), |w| {
        let id = w.spawn(body_key, BodyDef::static_at(Vec3::new(0.0, 0.0, -1.0)))?;
        id.create_box(Vec3::new(50.0, 50.0, 1.0), ShapeDef::default());
        Ok(())
    })
    .map(|_| ())
}

#[spacetimedb::reducer]
pub fn mirror_teleport(ctx: &ReducerContext, world_key: u64, body_key: u64, z: f32) -> Result<(), String> {
    box3d_stdb::with_world(ctx, world_key, DT, SUBSTEPS, |_| Ok(()), |w| {
        w.set_transform(body_key, Vec3::new(0.0, 0.0, z), Quat::IDENTITY)
    })
    .map(|_| ())
}

#[spacetimedb::reducer]
pub fn mirror_destroy(ctx: &ReducerContext, world_key: u64, body_key: u64) -> Result<(), String> {
    box3d_stdb::with_world(ctx, world_key, DT, SUBSTEPS, |_| Ok(()), |w| w.destroy(body_key)).map(|_| ())
}

#[spacetimedb::table(accessor = settle_result, public)]
pub struct SettleResult {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub steps_taken: u32,
    pub fell_asleep: bool,
}

/// Steps until the spawned mirror body falls asleep or `max_steps` is hit; records how many steps
/// it took. Exercises the post-step delta commit (the mirror's `asleep` flag).
#[spacetimedb::reducer]
pub fn settle(ctx: &ReducerContext, world_key: u64, max_steps: u32) -> Result<(), String> {
    let mut steps = 0;
    let mut asleep = false;
    while steps < max_steps && !asleep {
        box3d_stdb::with_world(ctx, world_key, DT, SUBSTEPS, |_| Ok(()), |_| Ok(()))?;
        steps += 1;
        asleep = ctx.db.b3_body().world_key().filter(world_key).any(|r| r.asleep);
    }
    ctx.db.settle_result().insert(SettleResult {
        id: 0,
        steps_taken: steps,
        fell_asleep: asleep,
    });
    Ok(())
}

/// Mirror-path bench: n³ keyed spheres over a ground box, `steps` with_world calls (one step +
/// delta commit each). Compare wall-clock against `bench` (raw world, no tables) from the CLI.
/// Re-running with the same `world_key` errors at create_world — `remove_world` between runs.
#[spacetimedb::reducer]
pub fn bench_mirror(ctx: &ReducerContext, world_key: u64, n: u32, steps: u32) -> Result<(), String> {
    // Body keys occupy [world_key*1e6, world_key*1e6 + n³]; past 1e6 they'd collide with the
    // next world's block (spawn would Err cleanly, but the bench would just fail).
    if u64::from(n).pow(3) >= 1_000_000 {
        return Err("n too large: n^3 must stay under 1,000,000".into());
    }
    box3d_stdb::create_world(ctx, world_key, &box3d_stdb::WorldDef::default())?;
    // Rebuild spawns the scene on the first call (cold world); warm iterations skip it.
    for _ in 0..steps {
        box3d_stdb::with_world(ctx, world_key, DT, SUBSTEPS, |w| spawn_scene_bench(w, world_key, n), |_| Ok(()))?;
    }
    Ok(())
}

fn spawn_scene_bench(
    w: &mut box3d_stdb::WorldCtx<'_>,
    world_key: u64,
    n: u32,
) -> Result<(), String> {
    let g = w.spawn(world_key * 1_000_000, BodyDef::static_at(Vec3::new(0.0, 0.0, -1.0)))?;
    g.create_box(Vec3::new(50.0, 50.0, 1.0), ShapeDef::default());
    let def = ShapeDef { density: 1.0, ..ShapeDef::default() };
    let spacing = 1.05;
    let off = (n as f32 - 1.0) * spacing * 0.5;
    let mut key = world_key * 1_000_000 + 1;
    for i in 0..n { for j in 0..n { for k in 0..n {
        let pos = Vec3::new(i as f32 * spacing - off, j as f32 * spacing - off, 1.0 + k as f32 * spacing);
        let b = w.spawn(key, BodyDef::dynamic_at(pos))?;
        b.create_sphere(Vec3::ZERO, 0.5, def);
        key += 1;
    }}}
    Ok(())
}

/// bench_mirror's scene + stepping, but ephemeral: measures pure guard overhead (no mirror I/O).
#[spacetimedb::reducer]
pub fn bench_ephemeral(ctx: &ReducerContext, world_key: u64, n: u32, steps: u32) -> Result<(), String> {
    if u64::from(n).pow(3) >= 1_000_000 {
        return Err("n too large: n^3 must stay under 1,000,000".into());
    }
    box3d_stdb::create_world(
        ctx,
        world_key,
        &box3d_stdb::WorldDef {
            persistence: box3d_stdb::Persistence::Ephemeral,
            ..Default::default()
        },
    )?;
    for _ in 0..steps {
        box3d_stdb::with_world(ctx, world_key, DT, SUBSTEPS, |w| spawn_scene_bench(w, world_key, n), |_| Ok(()))?;
    }
    Ok(())
}

/// Steps an existing bench world (create via bench_mirror/bench_ephemeral first). The rebuild
/// closure respawns the bench scene, so a cold call after a republish measures rebuild+overlay
/// at scale.
#[spacetimedb::reducer]
pub fn bench_continue(ctx: &ReducerContext, world_key: u64, n: u32, steps: u32) -> Result<(), String> {
    for _ in 0..steps {
        box3d_stdb::with_world(ctx, world_key, DT, SUBSTEPS, |w| spawn_scene_bench(w, world_key, n), |_| Ok(()))?;
    }
    Ok(())
}

#[spacetimedb::table(accessor = bench_timer, scheduled(bench_tick))]
pub struct BenchTimer {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: spacetimedb::ScheduleAt,
    pub world_key: u64,
    pub n: u32,
}

/// 60 Hz paced tick for a bench world.
#[spacetimedb::reducer]
pub fn bench_tick(ctx: &ReducerContext, timer: BenchTimer) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err("bench_tick may only be called by the scheduler".into());
    }
    box3d_stdb::with_world_paced(ctx, timer.world_key,
        |w| spawn_scene_bench(w, timer.world_key, timer.n), |_| Ok(()))
    .map(|_| ())
}

/// Create a bench world (mirrored or ephemeral) with an n³ scene and start ticking it at 60 Hz.
#[spacetimedb::reducer]
pub fn start_ticking(ctx: &ReducerContext, world_key: u64, n: u32, ephemeral: bool) -> Result<(), String> {
    if u64::from(n).pow(3) >= 1_000_000 {
        return Err("n too large: n^3 must stay under 1,000,000".into());
    }
    if ctx.db.bench_timer().iter().any(|t| t.world_key == world_key) {
        return Err(format!("world {world_key} already ticking"));
    }
    let def = if ephemeral {
        box3d_stdb::WorldDef { persistence: box3d_stdb::Persistence::Ephemeral, ..Default::default() }
    } else {
        box3d_stdb::WorldDef::default()
    };
    box3d_stdb::create_world(ctx, world_key, &def)?;
    ctx.db.bench_timer().insert(BenchTimer {
        scheduled_id: 0,
        scheduled_at: spacetimedb::ScheduleAt::Interval(spacetimedb::TimeDuration::from_micros(16_667)),
        world_key,
        n,
    });
    Ok(())
}

/// Stop ticking and destroy a bench world.
#[spacetimedb::reducer]
pub fn stop_ticking(ctx: &ReducerContext, world_key: u64) -> Result<(), String> {
    let stale: Vec<u64> = ctx.db.bench_timer().iter()
        .filter(|t| t.world_key == world_key)
        .map(|t| t.scheduled_id)
        .collect();
    for id in stale {
        ctx.db.bench_timer().scheduled_id().delete(id);
    }
    box3d_stdb::destroy_world(ctx, world_key)
}

/// n³ spheres rain onto a ground box — contact/solver heavy.
#[spacetimedb::reducer]
pub fn bench(ctx: &ReducerContext, n: u32, steps: u32) {
    let world = World::new(Vec3::new(0.0, 0.0, -10.0));
    let ground = world.create_body(BodyDef::static_at(Vec3::new(0.0, 0.0, -1.0)));
    ground
        .id()
        .create_box(Vec3::new(50.0, 50.0, 1.0), ShapeDef::default());

    let def = ShapeDef {
        density: 1.0,
        ..ShapeDef::default()
    };
    let mut bodies = Vec::new();
    let spacing = 1.05; // just under 2r contact-free start, collides on landing
    let off = (n as f32 - 1.0) * spacing * 0.5;
    for i in 0..n {
        for j in 0..n {
            for k in 0..n {
                let pos = Vec3::new(
                    i as f32 * spacing - off,
                    j as f32 * spacing - off,
                    1.0 + k as f32 * spacing,
                );
                let body = world.create_body(BodyDef::dynamic_at(pos));
                body.id().create_sphere(Vec3::ZERO, 0.5, def);
                bodies.push(body);
            }
        }
    }

    for _ in 0..steps {
        world.step(1.0 / 60.0, 4);
    }

    let (mut sx, mut sy, mut sz) = (0.0f64, 0.0f64, 0.0f64);
    for b in &bodies {
        let p = b.position();
        sx += p.x as f64;
        sy += p.y as f64;
        sz += p.z as f64;
    }
    ctx.db.bench_result().insert(BenchResult {
        id: 0,
        bodies: n * n * n,
        steps,
        sum_x: sx,
        sum_y: sy,
        sum_z: sz,
        awake_bodies: world.awake_body_count(),
    });
}
