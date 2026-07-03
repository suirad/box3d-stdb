//! Drops a sphere, steps the world, records where it lands.

use box3d::{BodyDef, ShapeDef, Vec3, World};
use spacetimedb::{ReducerContext, Table};

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

/// n³ spheres rain onto a ground box — contact/solver heavy.
#[spacetimedb::reducer]
pub fn bench(ctx: &ReducerContext, n: u32, steps: u32) {
    let world = World::new(Vec3::new(0.0, 0.0, -10.0));
    let ground = world.create_body(BodyDef::static_at(Vec3::new(0.0, 0.0, -1.0)));
    ground.id().create_box(Vec3::new(50.0, 50.0, 1.0), ShapeDef::default());

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

