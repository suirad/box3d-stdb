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

/// World construction definition, stored durably at [`create_world`] time. Rebuilds read it from
/// the table — callers can never accidentally rebuild with different physics.
pub struct WorldDef {
    pub gravity: Vec3,
    pub capacity: box3d::Capacity,
    pub persistence: Persistence,
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
}

thread_local! {
    static WORLDS: RefCell<HashMap<u64, WorldSlot>> = RefCell::new(HashMap::new());
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

/// Like [`with_world`], but runs as many fixed-`dt` steps as wall-clock time owes the world
/// (0..=max_catchup per call), so simulation time tracks real time regardless of scheduler
/// drift. Determinism is preserved: `dt` is fixed and the step count derives from the
/// replay-stable reducer timestamp.
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
    dt: f32,
    substeps: i32,
    max_catchup: u32,
    rebuild: impl FnOnce(&mut WorldCtx<'_>) -> Result<(), String>,
    game: impl FnOnce(&mut WorldCtx<'_>) -> Result<R, String>,
) -> Result<PacedResult<R>, String> {
    let pacing = Pacing::Realtime { max_catchup };
    step_world(ctx, world_key, dt, substeps, pacing, rebuild, game)
}

/// How one call advances simulation time: exactly one step, or as many fixed-`dt` steps as
/// wall-clock time owes (bounded by `max_catchup`).
enum Pacing {
    Single,
    Realtime { max_catchup: u32 },
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
        let cur = ctx.db.b3_world().world_key().find(world_key).unwrap();
        let new_gen = cur.generation + 1;
        ctx.db.b3_world().world_key().update(B3WorldRow {
            generation: new_gen,
            ..cur
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
            Pacing::Realtime { max_catchup } => {
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

                // Tick advances only now — after the steps and their mirror commit actually
                // happened. steps == 0 skips the write entirely: the eager bump already
                // committed the generation, and tick/stamp must not move.
                if steps > 0 {
                    ctx.db.b3_world().world_key().update(B3WorldRow {
                        generation: new_gen,
                        tick: cur.tick + u64::from(steps),
                        last_step_at_micros: new_stamp.unwrap_or(cur.last_step_at_micros),
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
