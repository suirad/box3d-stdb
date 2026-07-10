use std::{
    cell::RefCell,
    collections::{hash_map::Entry, HashMap},
    mem,
};

use box3d::{BodyDef, BodyId, Quat, Vec3};
use box3d_sys as sys;
use spacetimedb::Table;

/// Durable per-world stamp: the authority the in-memory world is validated against. Also stores
/// the world's construction definition — rebuilds read it here.
///
/// `generation` bumps on every [`with_world`]/[`with_world_paced`] entry; `tick` counts committed
/// steps. The table is private — expose `tick` to clients through your own `#[view]` if they need
/// it (the row type and accessor trait are re-exported for exactly that).
#[spacetimedb::table(accessor = b3_world)]
pub struct B3WorldRow {
    #[primary_key]
    pub world_key: u64,
    pub generation: u64,
    pub tick: u64,
    /// [`with_world_paced`]'s simulated-time frontier, micros since the Unix epoch; 0 = never
    /// paced-stepped. [`with_world`] leaves it untouched.
    pub last_step_at_micros: i64,
    pub ephemeral: bool,
    pub gx: f32,
    pub gy: f32,
    pub gz: f32,
    pub cap_static_shape_count: i32,
    pub cap_dynamic_shape_count: i32,
    pub cap_static_body_count: i32,
    pub cap_dynamic_body_count: i32,
    pub cap_contact_count: i32,
    /// Activity-adaptive tick policy, written once at [`create_world`]. Columns on the world row:
    /// the adaptive state below rides the row write the tick already does; upgrading consumers
    /// republish with --delete-data or migrate (internal table, pre-1.0).
    pub tick_policy: TickPolicy,
    pub tick_tier: u8,
    pub slow_ticks: u32,
    pub quiet_ticks: u32,
    pub parked: bool,
}

/// Per-body dynamic state mirror: transform, velocities, sleep. One row per spawned body.
///
/// Private by default — expose it through your own `#[view]` (row type + accessor trait are
/// re-exported), or enable the `public-mirror` feature for zero-boilerplate client subscription.
/// Ephemeral worlds never write rows here.
#[cfg_attr(feature = "public-mirror", spacetimedb::table(accessor = b3_body, public))]
#[cfg_attr(not(feature = "public-mirror"), spacetimedb::table(accessor = b3_body))]
pub struct B3BodyRow {
    #[primary_key]
    pub body_key: u64,
    #[index(btree)]
    pub world_key: u64,
    pub px: f32,
    pub py: f32,
    pub pz: f32,
    pub qx: f32,
    pub qy: f32,
    pub qz: f32,
    pub qw: f32,
    pub vx: f32,
    pub vy: f32,
    pub vz: f32,
    pub wx: f32,
    pub wy: f32,
    pub wz: f32,
    pub asleep: bool,
}

/// Whether a world's body state is mirrored into the durable `b3_body` table.
///
/// `Mirrored` (default): full persistence — rebuilds resume where the world left off, and the
/// mirror is the client-visible surface. `Ephemeral`: no mirror I/O at all — cheapest stepping;
/// any cache drop (abort, republish, restart) resets the world to whatever `rebuild` spawns.
/// The generation guard applies to both: an aborted reducer always forces a rebuild.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Persistence {
    Mirrored,
    Ephemeral,
}

/// One rung of an adaptive tick ladder: the fixed integration dt and solver substeps to run while
/// the scene's activity sits at this tier.
#[derive(spacetimedb::SpacetimeType, Clone, Copy)]
pub struct Tier {
    pub dt: f32,
    pub substeps: i32,
}

/// How a world's scheduled tick behaves. Stored durably at [`create_world`] time (on the world
/// row) — like [`WorldDef`], the stored policy is the only source; callers can't diverge.
#[derive(spacetimedb::SpacetimeType, Clone)]
pub struct TickPolicy {
    /// index 0 = full rate; demotion walks down.
    pub tiers: Vec<Tier>,
    /// max speed below which a scene may demote.
    pub slow_v: f32,
    /// consecutive slow steps before demoting one tier.
    pub k_slow: u32,
    /// consecutive all-asleep steps before Park (0 = never park).
    pub k_quiet: u32,
    pub max_catchup: u32,
}

impl TickPolicy {
    /// Fixed-rate realtime pacing — exactly the pre-policy `with_world_paced(dt, substeps,
    /// max_catchup)`. Single tier, `k_quiet` 0, so the adaptive pass is skipped entirely.
    pub fn realtime(dt: f32, substeps: i32, max_catchup: u32) -> Self {
        TickPolicy {
            tiers: vec![Tier { dt, substeps }],
            slow_v: 0.0,
            k_slow: 0,
            k_quiet: 0,
            max_catchup,
        }
    }

    /// Full adaptive ladder with settle-park.
    pub fn adaptive(tiers: Vec<Tier>, slow_v: f32, k_slow: u32, k_quiet: u32, max_catchup: u32) -> Self {
        TickPolicy {
            tiers,
            slow_v,
            k_slow,
            k_quiet,
            max_catchup,
        }
    }
}

/// What a paced tick tells the caller to do with its scheduled timer.
///
/// `Park`: the world settled, its state won't move until a mutation — the crate can't delete your
/// timer (it's consumer-owned), so on Park you should stop firing it. On Park, if you have queued
/// per-tick work, keep the timer and try next tick — the crate parks purely on physics stillness
/// and can't see your queues.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TickDirective {
    Continue,
    Park,
}

/// World construction definition, stored durably at [`create_world`] time. Rebuilds read it from
/// the table — callers can never accidentally rebuild with different physics.
pub struct WorldDef {
    pub gravity: Vec3,
    pub capacity: box3d::Capacity,
    pub persistence: Persistence,
    pub tick: TickPolicy,
}

impl Default for WorldDef {
    fn default() -> Self {
        WorldDef {
            gravity: Vec3 {
                z: -10.0,
                ..<_>::default()
            },
            capacity: Default::default(),
            persistence: Persistence::Mirrored,
            tick: TickPolicy::realtime(1.0 / 60.0, 4, 4),
        }
    }
}

/// A contact/sensor begin or end. `None` marks a body not spawned through the glue (no mirror key).
pub struct ContactTouch {
    pub body_key_a: Option<u64>,
    pub body_key_b: Option<u64>,
}

/// A hit event: the two bodies, world-space contact point/normal, and closing speed.
pub struct ContactHit {
    pub body_key_a: Option<u64>,
    pub body_key_b: Option<u64>,
    pub point: (f32, f32, f32),
    pub normal: (f32, f32, f32),
    pub approach_speed: f32,
}

/// A body moved by the simulation this step (not by user calls).
pub struct BodyMove {
    pub body_key: u64,
    pub position: (f32, f32, f32),
    pub rotation: (f32, f32, f32, f32), // (x, y, z, w)
    pub linear_velocity: (f32, f32, f32),
    pub angular_velocity: (f32, f32, f32),
    pub fell_asleep: bool,
}

/// A joint whose force/torque exceeded its event threshold this step (box3d reports the joint,
/// not the magnitudes). `joint_bits` round-trips via `box3d::JointId::from_bits` — joints are
/// raw-API territory, so consumers correlate with their own tracking.
pub struct JointOverload {
    pub joint_bits: u64,
}

/// Contact/sensor/hit/joint events surfaced by one [`with_world`] step.
pub struct StepEvents {
    pub moves: Vec<BodyMove>,
    pub contact_begins: Vec<ContactTouch>,
    pub contact_ends: Vec<ContactTouch>,
    pub hits: Vec<ContactHit>,
    pub sensor_begins: Vec<ContactTouch>,
    pub sensor_ends: Vec<ContactTouch>,
    pub joint_overloads: Vec<JointOverload>,
}

/// The `game` closure's return value plus the events its step produced.
pub struct StepResult<R> {
    pub value: R,
    pub events: StepEvents,
}

/// [`with_world_paced`]'s result: the game value, merged events, and how many fixed-dt steps ran.
pub struct PacedResult<R> {
    pub value: R,
    pub events: StepEvents,
    pub steps_run: u32,
    /// Whether the scheduled timer should keep firing or park; see [`TickDirective`].
    pub directive: TickDirective,
    /// The tier that RAN this call (0 = full rate). Consumers meter cost with it.
    pub tier: u8,
    /// The substeps the tier that RAN used — pairs with `tier` for cost metering.
    pub substeps: i32,
}

/// A ray hit from [`WorldCtx::cast_ray_closest`], with the struck shape resolved to its body key.
///
/// `body_key` is `None` when the ray hit a body not spawned through the glue (raw-created, so it
/// has no mirror key) — the geometry (`point`/`normal`/`fraction`) is still valid, only the key
/// mapping is absent. `fraction` is the hit distance along `translation`, in `0.0..=1.0`.
pub struct KeyedRayHit {
    pub body_key: Option<u64>,
    pub point: Vec3,
    pub normal: Vec3,
    pub fraction: f32,
}

struct WorldSlot {
    world: box3d::World,
    keys: HashMap<u64, u64>,     // consumer body_key → BodyId::to_bits()
    keys_rev: HashMap<u64, u64>, // BodyId::to_bits() → consumer body_key
    mem_gen: u64,
    busy: bool,
    warned_unknown: bool,
}

/// Handle to the live world inside [`with_world`] closures.
pub struct WorldCtx<'a> {
    world: &'a box3d::World,
    keys: &'a mut HashMap<u64, u64>,
    keys_rev: &'a mut HashMap<u64, u64>,
    ctx: &'a spacetimedb::ReducerContext,
    world_key: u64,
    persistence: Persistence,
}

impl WorldCtx<'_> {
    /// Raw access to the underlying [`box3d::World`].
    ///
    /// Anything you create through it must be recreated by your `rebuild` closure after a cache
    /// drop — the glue only rebuilds what your `rebuild` puts back.
    pub fn world(&self) -> &box3d::World {
        self.world
    }

    /// The live `BodyId` for a spawned key, if any. Valid for this closure and, immediately after
    /// `with_world` returns, for the remainder of the same reducer call.
    pub fn body_id(&self, key: u64) -> Option<BodyId> {
        self.keys.get(&key).copied().map(BodyId::from_bits)
    }

    /// Spawn a body under a stable consumer-chosen key. Attach shapes via the returned id.
    ///
    /// Keys are globally unique across worlds (the mirror's primary key) — reusing one in a
    /// second world is an error, enforced among mirrored worlds; ephemeral worlds never write
    /// rows, so their keys only need to be unique within their own world.
    ///
    /// If not Ephemeral, during a rebuild a surviving mirror row for `key` is applied to the fresh body
    /// (transform + velocities restored) instead of being overwritten — that restore is what makes
    /// rebuilds resume where the world left off.
    pub fn spawn(&mut self, key: u64, def: BodyDef) -> Result<BodyId, String> {
        if self.keys.contains_key(&key) {
            return Err(format!(
                "box3d-stdb: body {key} already spawned in world {}",
                self.world_key
            ));
        }
        // The in-map guard above is per-world; the row check catches cross-world reuse, which
        // would otherwise restore a foreign world's dynamics onto this body. Ephemeral worlds
        // never touch rows, so there is nothing to collide with or restore.
        if self.persistence == Persistence::Mirrored {
            if let Some(row) = self.ctx.db.b3_body().body_key().find(key) {
                if row.world_key != self.world_key {
                    return Err(format!(
                        "box3d-stdb: body key {key} already belongs to world {} (keys are global)",
                        row.world_key
                    ));
                }
            }
        }
        let body = self
            .world
            .try_create_body(def)
            .map_err(|e| format!("box3d-stdb: body {key} create failed: {e:?}"))?;
        let id = body.id();
        // World owns the C body; RAII drop would double-destroy it.
        mem::forget(body);

        if self.persistence == Persistence::Mirrored {
            if let Some(row) = self.ctx.db.b3_body().body_key().find(key) {
                // Restore saved state to fresh body; rebuild resumes where the last committed tick left off.
                id.set_transform(
                    Vec3::new(row.px, row.py, row.pz),
                    Quat {
                        v: Vec3::new(row.qx, row.qy, row.qz),
                        s: row.qw,
                    },
                );
                id.set_linear_velocity(Vec3::new(row.vx, row.vy, row.vz));
                id.set_angular_velocity(Vec3::new(row.wx, row.wy, row.wz));
            } else {
                self.ctx.db.b3_body().insert(B3BodyRow {
                    body_key: key,
                    world_key: self.world_key,
                    px: def.position.x,
                    py: def.position.y,
                    pz: def.position.z,
                    qx: 0.0,
                    qy: 0.0,
                    qz: 0.0,
                    qw: 1.0,
                    vx: 0.0,
                    vy: 0.0,
                    vz: 0.0,
                    wx: 0.0,
                    wy: 0.0,
                    wz: 0.0,
                    asleep: false,
                });
            }
        }

        let bits = id.to_bits();
        self.keys.insert(key, bits);
        self.keys_rev.insert(bits, key);
        Ok(id)
    }

    /// Wake the body for `key`. Err on unknown key.
    ///
    /// Setting velocities/impulses on a sleeping body has no effect — wake it first. (The
    /// wrapper's id tier exposes no wake, hence this raw-sys helper.)
    pub fn wake(&mut self, key: u64) -> Result<(), String> {
        let bits = *self
            .keys
            .get(&key)
            .ok_or_else(|| format!("box3d-stdb: unknown body key {key}"))?;
        unsafe { sys::b3Body_SetAwake(body_raw(bits), true) };
        // User mutations never reach the event stream; reflect the wake in the mirror now
        // rather than waiting a step for the first move event.
        if self.persistence == Persistence::Mirrored {
            if let Some(mut row) = self.ctx.db.b3_body().body_key().find(key) {
                row.asleep = false;
                self.ctx.db.b3_body().body_key().update(row);
            }
        }
        Ok(())
    }

    /// Destroy the body for `key` and delete its mirror row (mirrored worlds only). Err on
    /// unknown key.
    pub fn destroy(&mut self, key: u64) -> Result<(), String> {
        let bits = self
            .keys
            .remove(&key)
            .ok_or_else(|| format!("box3d-stdb: unknown body key {key}"))?;
        BodyId::from_bits(bits).destroy();
        self.keys_rev.remove(&bits);
        if self.persistence == Persistence::Mirrored {
            self.ctx.db.b3_body().body_key().delete(key);
        }
        Ok(())
    }

    /// Teleport: sets the C-side transform AND, in mirrored worlds, the mirror row (user moves
    /// never appear in move events, so the row must be written here). Err on unknown key.
    pub fn set_transform(
        &mut self,
        key: u64,
        position: Vec3,
        rotation: Quat,
    ) -> Result<(), String> {
        let bits = *self
            .keys
            .get(&key)
            .ok_or_else(|| format!("box3d-stdb: unknown body key {key}"))?;
        let id = BodyId::from_bits(bits);
        id.set_transform(position, rotation);
        if self.persistence == Persistence::Ephemeral {
            return Ok(());
        }
        if let Some(mut row) = self.ctx.db.b3_body().body_key().find(key) {
            row.px = position.x;
            row.py = position.y;
            row.pz = position.z;
            row.qx = rotation.v.x;
            row.qy = rotation.v.y;
            row.qz = rotation.v.z;
            row.qw = rotation.s;
            self.ctx.db.b3_body().body_key().update(row);
        }
        Ok(())
    }

    /// Cast a ray from `origin` along `translation` (its direction *and* length) and return the
    /// closest hit, or `None` if the ray reached its end without striking anything.
    ///
    /// The hit shape is resolved to its owning body key; see [`KeyedRayHit`] for what a `None`
    /// `body_key` means. Uses box3d's default query filter (every category is hittable).
    pub fn cast_ray_closest(&self, origin: Vec3, translation: Vec3) -> Option<KeyedRayHit> {
        let filter = unsafe { sys::b3DefaultQueryFilter() };
        let result = unsafe {
            sys::b3World_CastRayClosest(
                world_raw(self.world),
                origin.into(),
                translation.into(),
                filter,
            )
        };
        // Upstream contract: "if hit is false, all other data is invalid" — bail before shapeId.
        if !result.hit {
            return None;
        }
        Some(KeyedRayHit {
            body_key: shape_body_key(self.keys_rev, shape_bits(result.shapeId)),
            point: result.point.into(),
            normal: result.normal.into(),
            fraction: result.fraction,
        })
    }

    /// Apply a world-space linear impulse at a world-space `point`. Err on unknown key.
    ///
    /// Wakes the body: a sleeping body silently ignores impulses, so passing `false` would make the
    /// call a no-op exactly when a reaction is expected (same rationale as `wake`).
    pub fn apply_impulse(&mut self, key: u64, impulse: Vec3, point: Vec3) -> Result<(), String> {
        let bits = *self
            .keys
            .get(&key)
            .ok_or_else(|| format!("box3d-stdb: unknown body key {key}"))?;
        unsafe { sys::b3Body_ApplyLinearImpulse(body_raw(bits), impulse.into(), point.into(), true) };
        Ok(())
    }

    /// Apply a world-space linear impulse at the body's center of mass (no induced spin). Err on
    /// unknown key. Wakes the body, for the reason given on [`WorldCtx::apply_impulse`].
    pub fn apply_impulse_to_center(&mut self, key: u64, impulse: Vec3) -> Result<(), String> {
        let bits = *self
            .keys
            .get(&key)
            .ok_or_else(|| format!("box3d-stdb: unknown body key {key}"))?;
        unsafe { sys::b3Body_ApplyLinearImpulseToCenter(body_raw(bits), impulse.into(), true) };
        Ok(())
    }

    /// Apply a continuous world-space force at a world-space `point`, integrated over the next
    /// step's `dt`. Err on unknown key. Wakes the body ([`WorldCtx::apply_impulse`]).
    pub fn apply_force(&mut self, key: u64, force: Vec3, point: Vec3) -> Result<(), String> {
        let bits = *self
            .keys
            .get(&key)
            .ok_or_else(|| format!("box3d-stdb: unknown body key {key}"))?;
        unsafe { sys::b3Body_ApplyForce(body_raw(bits), force.into(), point.into(), true) };
        Ok(())
    }

    /// Apply a continuous world-space force at the body's center of mass (no induced spin),
    /// integrated over the next step's `dt`. Err on unknown key. Wakes the body.
    pub fn apply_force_to_center(&mut self, key: u64, force: Vec3) -> Result<(), String> {
        let bits = *self
            .keys
            .get(&key)
            .ok_or_else(|| format!("box3d-stdb: unknown body key {key}"))?;
        unsafe { sys::b3Body_ApplyForceToCenter(body_raw(bits), force.into(), true) };
        Ok(())
    }

    /// Apply a continuous world-space torque, integrated over the next step's `dt`. Err on unknown
    /// key. Wakes the body.
    pub fn apply_torque(&mut self, key: u64, torque: Vec3) -> Result<(), String> {
        let bits = *self
            .keys
            .get(&key)
            .ok_or_else(|| format!("box3d-stdb: unknown body key {key}"))?;
        unsafe { sys::b3Body_ApplyTorque(body_raw(bits), torque.into(), true) };
        Ok(())
    }

    /// Apply a one-shot world-space angular impulse (immediate spin change). Err on unknown key.
    /// Wakes the body, for the reason given on [`WorldCtx::apply_impulse`].
    pub fn apply_angular_impulse(&mut self, key: u64, impulse: Vec3) -> Result<(), String> {
        let bits = *self
            .keys
            .get(&key)
            .ok_or_else(|| format!("box3d-stdb: unknown body key {key}"))?;
        unsafe { sys::b3Body_ApplyAngularImpulse(body_raw(bits), impulse.into(), true) };
        Ok(())
    }

    /// The body's mass in kilograms. Err on unknown key. Handy for scaling an impulse to a target
    /// launch speed (`impulse = mass * velocity`).
    pub fn mass(&self, key: u64) -> Result<f32, String> {
        let bits = *self
            .keys
            .get(&key)
            .ok_or_else(|| format!("box3d-stdb: unknown body key {key}"))?;
        Ok(unsafe { sys::b3Body_GetMass(body_raw(bits)) })
    }

    /// The body's current world-space linear velocity. Err on unknown key.
    ///
    /// The wrapper's id tier exposes no velocity getter, hence this raw-sys read (the same reason
    /// the post-step loop reaches through `sys` for velocities).
    pub fn linear_velocity(&self, key: u64) -> Result<Vec3, String> {
        let bits = *self
            .keys
            .get(&key)
            .ok_or_else(|| format!("box3d-stdb: unknown body key {key}"))?;
        Ok(unsafe { sys::b3Body_GetLinearVelocity(body_raw(bits)) }.into())
    }

    /// The body's current world-space angular velocity (axis * radians/sec). Err on unknown key.
    /// Raw-sys read for the same reason as [`WorldCtx::linear_velocity`].
    pub fn angular_velocity(&self, key: u64) -> Result<Vec3, String> {
        let bits = *self
            .keys
            .get(&key)
            .ok_or_else(|| format!("box3d-stdb: unknown body key {key}"))?;
        Ok(unsafe { sys::b3Body_GetAngularVelocity(body_raw(bits)) }.into())
    }

    /// Every glue-spawned body with at least one shape overlapping the sphere at `center` of
    /// `radius`.
    ///
    /// Keys are deduped: box3d yields one result per overlapping *shape*, so a multi-shape body
    /// would otherwise appear once per shape. Bodies not spawned through the glue (no mirror key)
    /// are dropped — the returned keys are exactly the ones the caller can act on. Order is the
    /// query's traversal order, not stable across steps.
    pub fn overlap_sphere(&self, center: Vec3, radius: f32) -> Vec<u64> {
        // Proxy points are relative to the `origin` arg, so a single (0,0,0) point + radius is a
        // sphere centered on `center`.
        let point: sys::b3Vec3 = Vec3::default().into();
        let proxy = sys::b3ShapeProxy {
            points: &point as *const sys::b3Vec3,
            count: 1,
            radius,
        };
        let filter = unsafe { sys::b3DefaultQueryFilter() };
        let mut shapes: Vec<sys::b3ShapeId> = Vec::new();
        unsafe {
            sys::b3World_OverlapShape(
                world_raw(self.world),
                center.into(),
                &proxy,
                filter,
                Some(overlap_collect),
                (&mut shapes as *mut Vec<sys::b3ShapeId>).cast::<core::ffi::c_void>(),
            )
        };
        let mut keys: Vec<u64> = Vec::new();
        for s in shapes {
            if let Some(k) = shape_body_key(self.keys_rev, shape_bits(s)) {
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
        }
        keys
    }

    /// Number of awake bodies in the world (asleep bodies cost ~nothing to step).
    ///
    /// The live signal for activity-adaptive ticking: 0 for several consecutive ticks means the
    /// world is settled and its scheduled tick can safely stop — a fully settled world never
    /// self-wakes; only a mutation (spawn, impulse, wake) can move it again.
    pub fn awake_count(&self) -> u32 {
        unsafe { sys::b3World_GetAwakeBodyCount(world_raw(self.world)) }.max(0) as u32
    }
}

thread_local! {
    static WORLDS: RefCell<HashMap<u64, WorldSlot>> = RefCell::new(HashMap::new());
}

/// Reject a [`TickPolicy`] that would wedge the scheduled tick: empty tier ladder (nothing to
/// index), non-finite/sub-µs dt (division by zero in pacing), substeps < 1, or a broken slow_v.
fn validate_policy(p: &TickPolicy) -> Result<(), String> {
    if p.tiers.is_empty() {
        return Err("box3d-stdb: TickPolicy.tiers must not be empty".into());
    }
    for t in &p.tiers {
        if !t.dt.is_finite() || t.dt < 1e-6 {
            return Err(format!(
                "box3d-stdb: TickPolicy tier dt must be finite and >= 1µs (got {})",
                t.dt
            ));
        }
        if t.substeps < 1 {
            return Err(format!(
                "box3d-stdb: TickPolicy tier substeps must be >= 1 (got {})",
                t.substeps
            ));
        }
    }
    if !p.slow_v.is_finite() || p.slow_v < 0.0 {
        return Err(format!(
            "box3d-stdb: TickPolicy.slow_v must be finite and >= 0 (got {})",
            p.slow_v
        ));
    }
    Ok(())
}

/// Register world `key`: validates and stores its definition. The C world itself is built lazily
/// by the first [`with_world`] call. Err if the key already exists.
pub fn create_world(
    ctx: &spacetimedb::ReducerContext,
    world_key: u64,
    def: &WorldDef,
) -> Result<(), String> {
    if ctx.db.b3_world().world_key().find(world_key).is_some() {
        return Err(format!("box3d-stdb: world {world_key} already exists"));
    }
    // Validate here, not at tick time: a bad policy discovered inside the scheduled tick is a
    // permanent error loop that poisons (and leaks) the cache once per firing. Rejecting the
    // create is a one-time consumer-visible error instead.
    validate_policy(&def.tick)?;
    ctx.db.b3_world().insert(B3WorldRow {
        world_key,
        generation: 0,
        tick: 0,
        last_step_at_micros: 0,
        ephemeral: def.persistence == Persistence::Ephemeral,
        gx: def.gravity.x,
        gy: def.gravity.y,
        gz: def.gravity.z,
        cap_static_shape_count: def.capacity.static_shape_count,
        cap_dynamic_shape_count: def.capacity.dynamic_shape_count,
        cap_static_body_count: def.capacity.static_body_count,
        cap_dynamic_body_count: def.capacity.dynamic_body_count,
        cap_contact_count: def.capacity.contact_count,
        tick_policy: def.tick.clone(),
        tick_tier: 0,
        slow_ticks: 0,
        quiet_ticks: 0,
        parked: false,
    });
    Ok(())
}

/// Run one guarded physics tick for `world_key`: reconcile the cached world against its durable
/// stamp, run your game logic, then step.
///
/// Flow per call:
/// 1. **Reconcile** — the world must have been registered with [`create_world`]; missing/stale/
///    poisoned cache → rebuilt from the stored definition and your `rebuild` closure.
/// 2. **Generation bump** — written to [`B3WorldRow`] *before* anything else; if this reducer
///    aborts, the rolled-back row no longer matches the cache and the next call rebuilds.
/// 3. **`game`** — your per-tick logic (inputs, impulses, spawns), applied *before* integration
///    so its effects are part of this tick. Runs in the reducer's tx: your own table writes
///    commit atomically with the step.
/// 4. **Step** — the `dt`/`substeps` arguments; keep `dt` fixed for determinism.
/// 5. **Post-step** — simulation moves are committed to the `b3_body` mirror (transform,
///    velocities, sleep flag); contact/sensor/hit events are returned in [`StepResult`] (body
///    keys `None` for bodies not spawned through the glue); simulation moves are also returned
///    in `StepResult.events.moves` for all worlds. Ephemeral worlds skip every mirror write —
///    their only per-tick output is the [`StepResult`].
///
/// `rebuild` is construction-only and rare: recreate this world's bodies from *your* tables after
/// a cache drop (cold instance, republish, abort, poison). It must be deterministic for a given
/// table state. That includes spawn *order* — the solver is creation-order sensitive, so never
/// drive spawns from an unordered map; iterate your tables in a stable key order.
///
/// Neither closure may edit this world's `b3_world` row (including [`resume_full_rate`]): the
/// post-step write works from a snapshot taken at entry and would silently overwrite such edits.
/// Resume/park manipulation belongs in *other* reducers (actions, connects), never inside the
/// tick's own closures.
///
/// On a world driven by [`with_world_paced`], each `with_world` call injects one `dt` of
/// simulation the pacing clock never accounts for — sim time drifts ahead of real time by `dt`
/// per call. Fine for occasional event reducers; don't mix the two as peers.
///
/// # Errors
/// An `Err` from `game` **poisons the world** — the closure may have half-applied its mutations,
/// so the cached world is abandoned (deliberately leaked, never destroyed) and rebuilt on the
/// next call. Treat `Err` as "this tick is invalid", not control flow; signal game-level outcomes
/// through `Ok(R)`. An `Err` from `rebuild` discards the half-built world (cleanly destroyed) and
/// is returned.
///
/// # Panics
/// If called from inside another `with_world` closure (reentrancy is a consumer bug; the module
/// is single-threaded, so nothing else can hold the world cache).
pub fn with_world<R>(
    ctx: &spacetimedb::ReducerContext,
    world_key: u64,
    dt: f32,
    substeps: i32,
    rebuild: impl FnOnce(&mut WorldCtx<'_>) -> Result<(), String>,
    game: impl FnOnce(&mut WorldCtx<'_>) -> Result<R, String>,
) -> Result<StepResult<R>, String> {
    step_world(ctx, world_key, dt, substeps, Pacing::Single, rebuild, game).map(|p| StepResult {
        value: p.value,
        events: p.events,
    })
}

/// Like [`with_world`], but policy-driven: runs as many fixed-`dt` steps as wall-clock time owes
/// the world (0..=max_catchup per call), at the `(dt, substeps)` of the world's current activity
/// tier, so simulation time tracks real time regardless of scheduler drift. Determinism is
/// preserved: `dt` is fixed per tier and the step count derives from the replay-stable reducer
/// timestamp.
///
/// The tick policy is the one stored at [`create_world`] (world row) — callers can't diverge.
/// A [`TickPolicy::realtime`] policy is exactly the pre-policy fixed-rate paced path (zero state
/// I/O). A [`TickPolicy::adaptive`] policy demotes settled scenes down its tier ladder and, once
/// fully quiet, returns [`TickDirective::Park`] so the caller can stop its timer.
///
/// `game` runs ONCE before the catch-up loop — inputs arrive via tables, so re-applying them
/// per step would double-apply. If no full `dt` has elapsed, `game` still runs (and may mutate)
/// but no step, mirror commit, or tick advance happens (`steps_run == 0`).
///
/// When the backlog exceeds `max_catchup` steps, the excess time is DROPPED (logged at debug) —
/// carrying it forward would death-spiral an overloaded scheduler.
pub fn with_world_paced<R>(
    ctx: &spacetimedb::ReducerContext,
    world_key: u64,
    rebuild: impl FnOnce(&mut WorldCtx<'_>) -> Result<(), String>,
    game: impl FnOnce(&mut WorldCtx<'_>) -> Result<R, String>,
) -> Result<PacedResult<R>, String> {
    // Resolve the pre-step tier from the stored policy; step_world re-reads these same columns and
    // folds the adaptive updates into the row write it already does. A missing row falls through
    // with harmless defaults so step_world runs its orphan-slot reconcile and reports the error.
    let (dt, substeps, max_catchup, adaptive) = match ctx.db.b3_world().world_key().find(world_key) {
        None => (1.0 / 60.0, 4, 4, false),
        Some(row) => {
            // create_world validates, but a hand-edited row bypasses it — Err cleanly instead of
            // panicking on tiers[0] (a panicking scheduled tick is a permanent crash loop).
            if row.tick_policy.tiers.is_empty() {
                return Err(format!(
                    "box3d-stdb: world {world_key} has an empty TickPolicy.tiers (row edited by hand?)"
                ));
            }
            let tiers_len = row.tick_policy.tiers.len().max(1);
            // Clamp defensively; a parked row also resumes at tier 0 (self-heal). step_world's row
            // write persists both back.
            let tier = if row.parked || usize::from(row.tick_tier) >= tiers_len {
                0
            } else {
                row.tick_tier
            };
            let Tier { dt, substeps } = row.tick_policy.tiers[usize::from(tier)];
            // Realtime policy (single tier, never parks): the adaptive pass is a no-op, so skip it
            // — columns ride step_world's row write unchanged, identical cost to the old paced path.
            let adaptive = !(row.tick_policy.tiers.len() == 1 && row.tick_policy.k_quiet == 0);
            (dt, substeps, row.tick_policy.max_catchup, adaptive)
        }
    };
    step_world(
        ctx,
        world_key,
        dt,
        substeps,
        Pacing::Realtime {
            max_catchup,
            adaptive,
        },
        rebuild,
        game,
    )
}

/// Snap a world's tick back to full rate instantly — call at the top of any reducer that may wake
/// or add bodies (spawns, impulses, player actions). Hysteresis only governs slowing *down*, so
/// speeding back up is unconditional.
///
/// Returns `true` if the world was parked, i.e. the caller must re-arm its tick timer. Err if the
/// world doesn't exist.
///
/// Never call this from inside a `rebuild`/`game` closure of the same world — the tick's
/// post-step write would overwrite it from its entry snapshot (see [`with_world`]'s contract).
pub fn resume_full_rate(ctx: &spacetimedb::ReducerContext, world_key: u64) -> Result<bool, String> {
    let mut row = ctx
        .db
        .b3_world()
        .world_key()
        .find(world_key)
        .ok_or_else(|| format!("box3d-stdb: world {world_key} does not exist"))?;
    let was_parked = row.parked;
    // Already full and awake → no row write (the common hot-path call).
    if !row.parked && row.tick_tier == 0 && row.slow_ticks == 0 && row.quiet_ticks == 0 {
        return Ok(false);
    }
    row.parked = false;
    row.tick_tier = 0;
    row.slow_ticks = 0;
    row.quiet_ticks = 0;
    ctx.db.b3_world().world_key().update(row);
    Ok(was_parked)
}

/// Replace a live world's [`TickPolicy`] — e.g. drop a match from 60 Hz to 30 Hz between rounds.
/// Validated like [`create_world`]; takes effect on the next firing. Tier and hysteresis counters
/// reset to full rate (the old tier may not exist in the new ladder); `parked` is left alone —
/// it pairs with the consumer's timer state, and the next firing self-heals it anyway. The
/// generation is untouched, so the cached world stays valid: this changes *time*, not *physics*.
///
/// Same closure rule as [`resume_full_rate`]: never call it from inside the same world's
/// `rebuild`/`game` closures.
pub fn set_tick_policy(
    ctx: &spacetimedb::ReducerContext,
    world_key: u64,
    policy: &TickPolicy,
) -> Result<(), String> {
    validate_policy(policy)?;
    let mut row = ctx
        .db
        .b3_world()
        .world_key()
        .find(world_key)
        .ok_or_else(|| format!("box3d-stdb: world {world_key} does not exist"))?;
    row.tick_policy = policy.clone();
    row.tick_tier = 0;
    row.slow_ticks = 0;
    row.quiet_ticks = 0;
    ctx.db.b3_world().world_key().update(row);
    Ok(())
}

/// How one call advances simulation time: exactly one step, or as many fixed-`dt` steps as
/// wall-clock time owes (bounded by `max_catchup`).
///
/// `Realtime` also carries whether the adaptive pass runs. `adaptive: true` folds tier/counter
/// updates into the post-step row write step_world already does — free on a stepping tick — and
/// returns the resulting tier/directive.
enum Pacing {
    Single,
    Realtime { max_catchup: u32, adaptive: bool },
}

/// Shared body of [`with_world`] and [`with_world_paced`]; only the step count/stamp handling
/// differs, everything else must stay behaviorally identical between the two.
fn step_world<R>(
    ctx: &spacetimedb::ReducerContext,
    world_key: u64,
    dt: f32,
    substeps: i32,
    pacing: Pacing,
    rebuild: impl FnOnce(&mut WorldCtx<'_>) -> Result<(), String>,
    game: impl FnOnce(&mut WorldCtx<'_>) -> Result<R, String>,
) -> Result<PacedResult<R>, String> {
    WORLDS.with(|cell| -> Result<PacedResult<R>, String> {
        // Single-threaded module: the only way this borrow can fail is a reentrant call from
        // inside a with_world closure — a consumer bug, so panic rather than thread a Result.
        let mut map = cell
            .try_borrow_mut()
            .expect("box3d-stdb: with_world called from inside a with_world closure");

        let Some(row) = ctx.db.b3_world().world_key().find(world_key) else {
            // A slot can outlive its row: a create_world+step reducer that aborted rolls the
            // row back but leaves the slot in memory. Reconcile before erroring — a busy slot
            // may be torn C state (b3DestroyWorld would be UB), so leak it; a clean one Drops.
            if let Some(old) = map.remove(&world_key) {
                if old.busy {
                    mem::forget(old);
                    log::warn!(
                        "box3d-stdb: world {world_key} slot outlived its row (aborted \
                         create/step); abandoning poisoned slot"
                    );
                }
            }
            return Err(format!(
                "box3d-stdb: world {world_key} does not exist; call create_world first"
            ));
        };
        let persistence = if row.ephemeral {
            Persistence::Ephemeral
        } else {
            Persistence::Mirrored
        };
        let mut needs_rebuild = false;

        if let Some(slot) = map.get(&world_key) {
            if slot.busy {
                // Deliberate leak: a prior panic/error may have left internal C state torn,
                // so b3DestroyWorld would walk garbage (UB through the global allocator).
                let s = map.remove(&world_key).unwrap();
                mem::forget(s);
                log::warn!("box3d-stdb: world {world_key} was poisoned, forcing cold rebuild");
            } else if slot.mem_gen != row.generation {
                // Stale slot (e.g. server restart with persisted row). Generation mismatch
                // means this slot was never mid-mutation — safe to destroy via Drop.
                map.remove(&world_key);
            }
            // else: warm hit — generation matches, slot is clean, use as-is
        }

        if let Entry::Vacant(entry) = map.entry(world_key) {
            // The stored definition is the only construction source — callers can't diverge.
            let world = box3d::World::try_with_capacity(
                Vec3::new(row.gx, row.gy, row.gz),
                box3d::Capacity {
                    static_shape_count: row.cap_static_shape_count,
                    dynamic_shape_count: row.cap_dynamic_shape_count,
                    static_body_count: row.cap_static_body_count,
                    dynamic_body_count: row.cap_dynamic_body_count,
                    contact_count: row.cap_contact_count,
                },
            )
            .map_err(|e| format!("box3d-stdb: world {world_key} create failed: {e:?}"))?;
            // busy until the step commits: a panic inside rebuild would otherwise leave a
            // half-built world that the generation check alone can't distinguish from clean.
            entry.insert(WorldSlot {
                world,
                keys: HashMap::new(),
                keys_rev: HashMap::new(),
                mem_gen: row.generation,
                busy: true,
                warned_unknown: false,
            });
            needs_rebuild = true;
        }

        if needs_rebuild {
            let r = {
                let slot = map.get_mut(&world_key).unwrap();
                rebuild(&mut WorldCtx {
                    world: &slot.world,
                    keys: &mut slot.keys,
                    keys_rev: &mut slot.keys_rev,
                    ctx,
                    world_key,
                    persistence,
                })
            };
            if let Err(e) = r {
                map.remove(&world_key);
                return Err(e);
            }
            // Sweep mirror rows the rebuild did not respawn (consumer state shrank while the
            // mirror row survived). Self-healing by design — a hard error here would deadlock
            // the reducer in a rebuild-fail-retry loop. Ephemeral worlds have no rows to sweep.
            if persistence == Persistence::Mirrored {
                let slot = map.get(&world_key).unwrap();
                let stale: Vec<u64> = ctx.db.b3_body().world_key().filter(world_key)
                    .filter(|r| !slot.keys.contains_key(&r.body_key))
                    .map(|r| r.body_key).collect();
                if !stale.is_empty() {
                    log::warn!("box3d-stdb: world {world_key} rebuild left {} stale mirror row(s); deleting", stale.len());
                    for k in &stale { ctx.db.b3_body().body_key().delete(*k); }
                }
            }
        }

        // Eager generation bump BEFORE game/step: if the reducer aborts (panic or Err), the DB
        // row already shows a new generation. Next entry finds a mismatch and forces cold rebuild
        // rather than reusing potentially-stale C state. Tick is NOT bumped here — a swallowed
        // game-Err would commit a tick that never stepped; it advances post-step.
        // Read once; the row isn't Copy (tick_policy is a Vec), so a single owned `cur`
        // threads through the eager bump, the adaptive pass, and the post-step write.
        let cur = ctx.db.b3_world().world_key().find(world_key).unwrap();
        let new_gen = cur.generation + 1;
        ctx.db.b3_world().world_key().update(B3WorldRow {
            generation: new_gen,
            world_key: cur.world_key,
            tick: cur.tick,
            last_step_at_micros: cur.last_step_at_micros,
            ephemeral: cur.ephemeral,
            gx: cur.gx,
            gy: cur.gy,
            gz: cur.gz,
            cap_static_shape_count: cur.cap_static_shape_count,
            cap_dynamic_shape_count: cur.cap_dynamic_shape_count,
            cap_static_body_count: cur.cap_static_body_count,
            cap_dynamic_body_count: cur.cap_dynamic_body_count,
            cap_contact_count: cur.cap_contact_count,
            tick_policy: cur.tick_policy.clone(),
            tick_tier: cur.tick_tier,
            slow_ticks: cur.slow_ticks,
            quiet_ticks: cur.quiet_ticks,
            parked: cur.parked,
        });
        {
            let slot = map.get_mut(&world_key).unwrap();
            slot.mem_gen = new_gen;
            slot.busy = true;
        }

        // Step plan. `new_stamp: None` = leave `last_step_at_micros` untouched (unpaced calls
        // must not disturb a paced schedule on the same world).
        let (steps, new_stamp): (u32, Option<i64>) = match pacing {
            Pacing::Single => (1, None),
            Pacing::Realtime { max_catchup, .. } => {
                let now = ctx.timestamp.to_micros_since_unix_epoch();
                // Truncating dt to whole micros loses <1µs per step; for a fixed dt that is a
                // constant, replay-stable rounding — negligible next to scheduler jitter.
                let dt_micros = (f64::from(dt) * 1e6) as i64;
                // dt of 0/NaN/negative would divide by zero below — on a scheduled reducer
                // that's a permanent panic loop, so reject loudly instead.
                if dt_micros <= 0 {
                    return Err(format!(
                        "box3d-stdb: with_world_paced requires dt >= 1µs (got {dt})"
                    ));
                }
                // max_catchup 0 would freeze the world: the backlog-drop stamp write is gated
                // on steps > 0, so the stamp could never advance again.
                let max_catchup = max_catchup.max(1);
                let stamp = cur.last_step_at_micros;
                if stamp == 0 {
                    // First firing starts the clock.
                    (1, Some(now))
                } else {
                    let elapsed = now - stamp;
                    let steps = (elapsed / dt_micros).max(0);
                    if steps <= i64::from(max_catchup) {
                        // Stamp advances by whole steps only — the sub-dt remainder carries.
                        (steps as u32, Some(stamp + steps * dt_micros))
                    } else {
                        // Excess backlog is dropped, not carried: carrying it would death-spiral
                        // an overloaded scheduler.
                        log::debug!(
                            "box3d-stdb: world {world_key} step backlog exceeds \
                             max_catchup={max_catchup}; dropping {} µs",
                            elapsed - i64::from(max_catchup) * dt_micros
                        );
                        (max_catchup, Some(now))
                    }
                }
            }
        };

        let game_result = {
            let slot = map.get_mut(&world_key).unwrap();
            game(&mut WorldCtx {
                world: &slot.world,
                keys: &mut slot.keys,
                keys_rev: &mut slot.keys_rev,
                ctx,
                world_key,
                persistence,
            })
        };

        match game_result {
            // Leave busy = true: the C world may be half-mutated. The tx rollback restores tables
            // but not C heap state, so poisoning forces cold rebuild on next entry.
            Err(e) => Err(e),
            Ok(r) => {
                let slot = map.get_mut(&world_key).unwrap();

                // Moves merge per body with latest-write-wins: a body that moves in step 1 then
                // sleeps in step 2 of 3 must reach the single mirror commit with its resting
                // state and fell_asleep flag. Contacts/sensors/hits just accumulate.
                let mut merged: HashMap<u64, BodyMove> = HashMap::new();
                // Adaptive-pass speed signal, tracked per STEP: `merged` keeps only each body's
                // final state, so a body fast in early catch-up steps but slow by the last would
                // read as slow and let the ladder demote through real activity.
                let mut v_max_sq: f32 = 0.0;
                let mut contact_begins = Vec::new();
                let mut contact_ends = Vec::new();
                let mut hits = Vec::new();
                let mut sensor_begins = Vec::new();
                let mut sensor_ends = Vec::new();
                let mut joint_overloads = Vec::new();

                // Events are collected after EVERY step — box3d clears its event arrays on step,
                // so anything not harvested inside the loop is lost.
                for _ in 0..steps {
                    slot.world.step(dt, substeps);

                    // BodyEvents<'world> borrows the world, so collect into owned data first —
                    // the borrow must end before we take &mut slot.warned_unknown.
                    let moves: Vec<_> = slot.world.body_events().moves().collect();
                    for ev in moves {
                        let bits = ev.body.to_bits();
                        let Some(body_key) = slot.keys_rev.get(&bits).copied() else {
                            // Warn once, not per-body-per-tick: a raw-created body (not spawned
                            // through the glue) has no mirror row, and the log would otherwise
                            // flood.
                            if !slot.warned_unknown {
                                log::warn!(
                                    "box3d-stdb: world {world_key} moved a body not spawned \
                                     through the glue; it is absent from move events and the \
                                     mirror"
                                );
                                slot.warned_unknown = true;
                            }
                            continue;
                        };
                        let t = ev.transform;
                        // Fetched per step, for every mode: the id-tier API has no velocity
                        // getters, so the move event is the only velocity channel consumers
                        // have — and the merged entry must carry each body's LATEST velocities.
                        let raw = body_raw(bits);
                        let lin = unsafe { sys::b3Body_GetLinearVelocity(raw) };
                        let ang = unsafe { sys::b3Body_GetAngularVelocity(raw) };
                        v_max_sq = v_max_sq.max(lin.x * lin.x + lin.y * lin.y + lin.z * lin.z);
                        merged.insert(
                            body_key,
                            BodyMove {
                                body_key,
                                position: (t.p.x, t.p.y, t.p.z),
                                rotation: (t.q.v.x, t.q.v.y, t.q.v.z, t.q.s),
                                linear_velocity: (lin.x, lin.y, lin.z),
                                angular_velocity: (ang.x, ang.y, ang.z),
                                fell_asleep: ev.fell_asleep,
                            },
                        );
                    }

                    // Event collection: shape → body → keys_rev → Option<u64>.
                    let keys_rev = &slot.keys_rev;
                    let contact = slot.world.contact_events();
                    let sensor = slot.world.sensor_events();
                    contact_begins.extend(
                        contact
                            .begins()
                            .map(|e| touch(keys_rev, e.shape_a.to_bits(), e.shape_b.to_bits())),
                    );
                    contact_ends.extend(
                        contact
                            .ends()
                            .map(|e| touch(keys_rev, e.shape_a.to_bits(), e.shape_b.to_bits())),
                    );
                    hits.extend(contact.hits().map(|e| ContactHit {
                        body_key_a: shape_body_key(keys_rev, e.shape_a.to_bits()),
                        body_key_b: shape_body_key(keys_rev, e.shape_b.to_bits()),
                        point: (e.point.x, e.point.y, e.point.z),
                        normal: (e.normal.x, e.normal.y, e.normal.z),
                        approach_speed: e.approach_speed,
                    }));
                    // Sensor begin/end reuse ContactTouch: sensor→body_key_a, visitor→body_key_b.
                    sensor_begins.extend(
                        sensor
                            .begins()
                            .map(|e| touch(keys_rev, e.sensor.to_bits(), e.visitor.to_bits())),
                    );
                    sensor_ends.extend(
                        sensor
                            .ends()
                            .map(|e| touch(keys_rev, e.sensor.to_bits(), e.visitor.to_bits())),
                    );
                    joint_overloads.extend(slot.world.joint_events().iter().map(|e| {
                        JointOverload {
                            joint_bits: e.joint.to_bits(),
                        }
                    }));
                }

                // ONE mirror commit from the merged final states (Mirrored only) — intermediate
                // catch-up transforms are never observable, so writing them would be wasted I/O.
                if persistence == Persistence::Mirrored {
                    for m in merged.values() {
                        if let Some(mut row) = ctx.db.b3_body().body_key().find(m.body_key) {
                            row.px = m.position.0;
                            row.py = m.position.1;
                            row.pz = m.position.2;
                            row.qx = m.rotation.0;
                            row.qy = m.rotation.1;
                            row.qz = m.rotation.2;
                            row.qw = m.rotation.3;
                            row.vx = m.linear_velocity.0;
                            row.vy = m.linear_velocity.1;
                            row.vz = m.linear_velocity.2;
                            row.wx = m.angular_velocity.0;
                            row.wy = m.angular_velocity.1;
                            row.wz = m.angular_velocity.2;
                            row.asleep = m.fell_asleep;
                            ctx.db.b3_body().body_key().update(row);
                        }
                    }
                }

                // Adaptive pass — runs when the policy is adaptive AND steps ran (a 0-step firing
                // carries no activity signal, so skip it and let the columns ride unchanged). The
                // resulting tier/counters/parked fold into the ONE row write below, free on the
                // write the tick already does. Awake is read here while `slot` is still borrowed.
                let mut directive = TickDirective::Continue;
                // Default RAN tier: for a realtime policy the single tier is 0; step_world resolved
                // dt/substeps from it in with_world_paced. The adaptive branch overwrites it with
                // the healed tier that actually ran.
                let mut ran_tier = cur.tick_tier;
                let (mut tier, mut slow_ticks, mut quiet_ticks, mut parked) =
                    (cur.tick_tier, cur.slow_ticks, cur.quiet_ticks, cur.parked);
                let adaptive = matches!(pacing, Pacing::Realtime { adaptive: true, .. });
                if adaptive && steps > 0 {
                    let policy = &cur.tick_policy;
                    let tiers_len = policy.tiers.len().max(1);
                    // Parked self-heal / clamp: a firing here despite parked=true means the
                    // consumer's timer outlived the Park directive or a re-arm raced — snap to full
                    // rate and clear rather than trust stale tier/counter state. Same reset for an
                    // out-of-range tier after a policy edit. This mirrors with_world_paced's
                    // pre-step resolution, so `tier` here is the one dt/substeps ran at.
                    if parked || usize::from(tier) >= tiers_len {
                        tier = 0;
                        slow_ticks = 0;
                        quiet_ticks = 0;
                        parked = false;
                    }
                    ran_tier = tier;
                    // Per-step max (tracked in the harvest loop), not merged's final states —
                    // see the v_max_sq declaration.
                    let v_max = v_max_sq.sqrt();
                    let awake =
                        unsafe { sys::b3World_GetAwakeBodyCount(world_raw(&slot.world)) }.max(0) as u32;

                    if v_max >= policy.slow_v && tier != 0 {
                        tier = 0;
                        slow_ticks = 0;
                        quiet_ticks = 0;
                    } else if v_max < policy.slow_v {
                        slow_ticks = slow_ticks.saturating_add(steps);
                        if slow_ticks >= policy.k_slow && usize::from(tier) < tiers_len - 1 {
                            tier += 1;
                            slow_ticks = 0;
                        }
                    }

                    // Park: fully settled — awake == 0 AND a moveless batch. The moves condition
                    // is load-bearing: a body can move in early catch-up steps yet sleep by the
                    // last, so its final batch carries events the consumer's post hook may turn
                    // into queued work; parking on that same tick would strand the work until the
                    // next wake. Requiring a moveless batch guarantees at least one more tick runs
                    // after the last event. The crate can't delete the timer — parked=true + the
                    // Park directive IS the stop contract.
                    if policy.k_quiet > 0 && awake == 0 && merged.is_empty() {
                        quiet_ticks = quiet_ticks.saturating_add(steps);
                        if quiet_ticks >= policy.k_quiet {
                            parked = true;
                            tier = 0;
                            slow_ticks = 0;
                            quiet_ticks = 0;
                            directive = TickDirective::Park;
                        }
                    } else {
                        quiet_ticks = 0;
                    }
                }

                // Tick advances only now — after the steps and their mirror commit actually
                // happened. steps == 0 skips the write entirely: the eager bump already
                // committed the generation, and tick/stamp must not move. The adaptive columns
                // ride this same write.
                if steps > 0 {
                    ctx.db.b3_world().world_key().update(B3WorldRow {
                        generation: new_gen,
                        tick: cur.tick + u64::from(steps),
                        last_step_at_micros: new_stamp.unwrap_or(cur.last_step_at_micros),
                        tick_tier: tier,
                        slow_ticks,
                        quiet_ticks,
                        parked,
                        ..cur
                    });
                }

                slot.busy = false;
                Ok(PacedResult {
                    value: r,
                    events: StepEvents {
                        moves: merged.into_values().collect(),
                        contact_begins,
                        contact_ends,
                        hits,
                        sensor_begins,
                        sensor_ends,
                        joint_overloads,
                    },
                    steps_run: steps,
                    directive,
                    // The tier that RAN this call (what dt/substeps were resolved from), not the
                    // post-pass next tier — consumers meter this call's cost with it.
                    tier: ran_tier,
                    substeps,
                })
            }
        }
    })
}

// The wrapper hides its raw ids and the id-tier exposes no velocity/shape→body accessors, so
// reconstruct raw handles from the stable to_bits() layout and go straight to sys.
fn body_raw(bits: u64) -> sys::b3BodyId {
    sys::b3BodyId {
        index1: (bits >> 32) as i32,
        world0: (bits >> 16) as u16,
        generation: bits as u16,
    }
}

// Queries take the raw world id; the wrapper hides it, so reconstruct from World::id()'s stable
// to_bits() layout (index1 in the high 16, generation in the low 16) — same trick as body_raw.
fn world_raw(world: &box3d::World) -> sys::b3WorldId {
    let bits = world.id().to_bits();
    sys::b3WorldId {
        index1: (bits >> 16) as u16,
        generation: bits as u16,
    }
}

fn shape_body_key(keys_rev: &HashMap<u64, u64>, shape_bits: u64) -> Option<u64> {
    let raw = sys::b3ShapeId {
        index1: (shape_bits >> 32) as i32,
        world0: (shape_bits >> 16) as u16,
        generation: shape_bits as u16,
    };
    // End events may reference shapes destroyed before this step (upstream doc: always confirm
    // with b3Shape_IsValid); GetBody on a dead id would read a freed slot under NDEBUG.
    if !unsafe { sys::b3Shape_IsValid(raw) } {
        return None;
    }
    let body = unsafe { sys::b3Shape_GetBody(raw) };
    let bits = ((body.index1 as u64) << 32) | ((body.world0 as u64) << 16) | body.generation as u64;
    keys_rev.get(&bits).copied()
}

// Pack a raw shape id into the u64 layout shape_body_key expects (mirrors ShapeId::to_bits). The
// event path gets these bits from the wrapper's to_bits(); query results hand back raw ids instead.
fn shape_bits(s: sys::b3ShapeId) -> u64 {
    ((s.index1 as u64) << 32) | ((s.world0 as u64) << 16) | s.generation as u64
}

// Trampoline for b3World_OverlapShape: pushes each overlapping shape id into the Vec behind
// `context`. Returning true keeps the query collecting every overlap rather than stopping at the
// first. The push is caught: a panic unwinding across this C frame would be UB, so swallow it
// (the query then just yields fewer keys) — same guard the box3d wrapper's own callbacks use.
unsafe extern "C" fn overlap_collect(shape_id: sys::b3ShapeId, context: *mut core::ffi::c_void) -> bool {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let out = unsafe { &mut *context.cast::<Vec<sys::b3ShapeId>>() };
        out.push(shape_id);
    }));
    true
}

fn touch(keys_rev: &HashMap<u64, u64>, shape_a_bits: u64, shape_b_bits: u64) -> ContactTouch {
    ContactTouch {
        body_key_a: shape_body_key(keys_rev, shape_a_bits),
        body_key_b: shape_body_key(keys_rev, shape_b_bits),
    }
}

/// Destroy `world_key`'s cached world and delete its durable stamp. Idempotent.
///
/// A healthy cached world is destroyed via `b3DestroyWorld`; a poisoned one is abandoned
/// (leaked) instead — see [`with_world`]'s error contract.
pub fn destroy_world(ctx: &spacetimedb::ReducerContext, world_key: u64) -> Result<(), String> {
    WORLDS.with(|cell| {
        // Same reentrancy-is-a-bug stance as with_world.
        let mut map = cell
            .try_borrow_mut()
            .expect("box3d-stdb: destroy_world called from inside a with_world closure");
        if let Some(slot) = map.remove(&world_key) {
            if slot.busy {
                mem::forget(slot);
            }
            // else: drop = clean b3DestroyWorld via box3d::World::Drop
        }
    });
    // Idempotent: delete returns false for a missing row; we ignore the result.
    ctx.db.b3_world().world_key().delete(world_key);
    // Delete all mirror rows for this world.
    let stale: Vec<u64> = ctx
        .db
        .b3_body()
        .world_key()
        .filter(world_key)
        .map(|r| r.body_key)
        .collect();
    for body_key in stale {
        ctx.db.b3_body().body_key().delete(body_key);
    }
    Ok(())
}
