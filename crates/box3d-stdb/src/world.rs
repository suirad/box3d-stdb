use std::{cell::RefCell, collections::HashMap, mem};

use box3d::{BodyDef, BodyId, Quat, Vec3};
use box3d_sys as sys;
use spacetimedb::Table;

/// Durable per-world stamp: the authority the in-memory world is validated against.
///
/// `generation` bumps on every [`with_world`] entry; `tick` counts committed steps. The table is
/// private — expose `tick` to clients through your own `#[view]` if they need it (the row type
/// and accessor trait are re-exported for exactly that).
#[spacetimedb::table(accessor = b3_world)]
pub struct B3WorldRow {
    #[primary_key]
    pub world_key: u64,
    pub generation: u64,
    pub tick: u64,
}

/// Per-body dynamic state mirror: transform, velocities, sleep. One row per spawned body.
///
/// Private by default — expose it through your own `#[view]` (row type + accessor trait are
/// re-exported), or enable the `public-mirror` feature for zero-boilerplate client subscription.
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

/// World construction + stepping parameters, passed on every [`with_world`] call.
///
/// `gravity`/`capacity` are only *used* when the world is (re)built — pass the same values for a
/// given `world_key` on every call, or a rebuild will silently produce a different world.
/// `dt`/`substeps` apply to every step; keep `dt` fixed for determinism.
pub struct WorldParams {
    pub gravity: box3d::Vec3,
    pub capacity: box3d::Capacity,
    pub dt: f32,
    pub substeps: i32,
}

impl Default for WorldParams {
    fn default() -> Self {
        WorldParams {
            gravity: Vec3{z: -10.0, ..<_>::default()},
            dt: 1.0 / 60.0,
            substeps: 4,
            capacity: Default::default()
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

/// Contact/sensor/hit events surfaced by one [`with_world`] step.
pub struct StepEvents {
    pub contact_begins: Vec<ContactTouch>,
    pub contact_ends: Vec<ContactTouch>,
    pub hits: Vec<ContactHit>,
    pub sensor_begins: Vec<ContactTouch>,
    pub sensor_ends: Vec<ContactTouch>,
}

/// The `game` closure's return value plus the events its step produced.
pub struct StepResult<R> {
    pub value: R,
    pub events: StepEvents,
}

struct WorldSlot {
    world: box3d::World,
    keys: HashMap<u64, u64>,      // consumer body_key → BodyId::to_bits()
    keys_rev: HashMap<u64, u64>,  // BodyId::to_bits() → consumer body_key
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
}

impl WorldCtx<'_> {
    /// Raw access to the underlying [`box3d::World`].
    ///
    /// Anything you create through it must be recreated by your `rebuild` closure after a cache
    /// drop — the glue only rebuilds what your `rebuild` puts back.
    pub fn world(&self) -> &box3d::World {
        self.world
    }

    /// Spawn a body under a stable consumer-chosen key. Attach shapes via the returned id.
    ///
    /// Keys are globally unique across worlds (the mirror's primary key) — reusing one in a
    /// second world is an error.
    ///
    /// During a rebuild, a surviving mirror row for `key` is applied to the fresh body
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
        // would otherwise restore a foreign world's dynamics onto this body.
        if let Some(row) = self.ctx.db.b3_body().body_key().find(key) {
            if row.world_key != self.world_key {
                return Err(format!(
                    "box3d-stdb: body key {key} already belongs to world {} (keys are global)",
                    row.world_key
                ));
            }
        }
        let body = self
            .world
            .try_create_body(def)
            .map_err(|e| format!("box3d-stdb: body {key} create failed: {e:?}"))?;
        let id = body.id();
        // World owns the C body; RAII drop would double-destroy it.
        mem::forget(body);

        if let Some(row) = self.ctx.db.b3_body().body_key().find(key) {
            // Restore saved state to fresh body; rebuild resumes where the last committed tick left off.
            id.set_transform(
                Vec3::new(row.px, row.py, row.pz),
                Quat { v: Vec3::new(row.qx, row.qy, row.qz), s: row.qw },
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

        let bits = id.to_bits();
        self.keys.insert(key, bits);
        self.keys_rev.insert(bits, key);
        Ok(id)
    }

    /// Destroy the body for `key` and delete its mirror row. Err on unknown key.
    pub fn destroy(&mut self, key: u64) -> Result<(), String> {
        let bits = self
            .keys
            .remove(&key)
            .ok_or_else(|| format!("box3d-stdb: unknown body key {key}"))?;
        BodyId::from_bits(bits).destroy();
        self.keys_rev.remove(&bits);
        self.ctx.db.b3_body().body_key().delete(key);
        Ok(())
    }

    /// Teleport: sets the C-side transform AND the mirror row (user moves never appear in
    /// move events, so the row must be written here). Err on unknown key.
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
}

thread_local! {
    static WORLDS: RefCell<HashMap<u64, WorldSlot>> = RefCell::new(HashMap::new());
}

/// Run one guarded physics tick for `world_key`: reconcile the cached world against its durable
/// stamp, run your game logic, then step.
///
/// Flow per call:
/// 1. **Reconcile** — missing/stale/poisoned cache → the world is (re)built from `params` and
///    your `rebuild` closure. A brand-new `world_key` is auto-created (logged), with `rebuild`
///    acting as the initial spawn.
/// 2. **Generation bump** — written to [`B3WorldRow`] *before* anything else; if this reducer
///    aborts, the rolled-back row no longer matches the cache and the next call rebuilds.
/// 3. **`game`** — your per-tick logic (inputs, impulses, spawns), applied *before* integration
///    so its effects are part of this tick. Runs in the reducer's tx: your own table writes
///    commit atomically with the step.
/// 4. **Step** — `dt`/`substeps` from `params`.
/// 5. **Post-step** — simulation moves are committed to the `b3_body` mirror (transform,
///    velocities, sleep flag); contact/sensor/hit events are returned in [`StepResult`] (body
///    keys `None` for bodies not spawned through the glue).
///
/// `rebuild` is construction-only and rare: recreate this world's bodies from *your* tables after
/// a cache drop (cold instance, republish, abort, poison). It must be deterministic for a given
/// table state.
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
    params: &WorldParams,
    rebuild: impl FnOnce(&mut WorldCtx<'_>) -> Result<(), String>,
    game: impl FnOnce(&mut WorldCtx<'_>) -> Result<R, String>,
) -> Result<StepResult<R>, String> {
    WORLDS.with(|cell| -> Result<StepResult<R>, String> {
        // Single-threaded module: the only way this borrow can fail is a reentrant call from
        // inside a with_world closure — a consumer bug, so panic rather than thread a Result.
        let mut map = cell
            .try_borrow_mut()
            .expect("box3d-stdb: with_world called from inside a with_world closure");

        let row = ctx.db.b3_world().world_key().find(world_key);
        let mut needs_rebuild = false;

        match &row {
            None => {
                // A slot can outlive its row: a first-use abort rolls the row back but leaves
                // the busy slot in memory. Reconcile before insert — map.insert would DROP the
                // old slot, running b3DestroyWorld on a possibly-torn world (UB in C).
                if let Some(old) = map.remove(&world_key) {
                    if old.busy {
                        mem::forget(old);
                        log::warn!(
                            "box3d-stdb: world {world_key} was poisoned (first-use abort), \
                             forcing cold rebuild"
                        );
                    }
                }
                let world = box3d::World::try_with_capacity(params.gravity, params.capacity)
                    .map_err(|e| format!("box3d-stdb: world {world_key} create failed: {e:?}"))?;
                ctx.db.b3_world().insert(B3WorldRow {
                    world_key,
                    generation: 1,
                    tick: 0,
                });
                log::info!("box3d-stdb: world {world_key} created");
                // busy until the step commits: a panic inside rebuild would otherwise leave a
                // half-built world that the generation check alone can't distinguish from clean.
                map.insert(
                    world_key,
                    WorldSlot {
                        world,
                        keys: HashMap::new(),
                        keys_rev: HashMap::new(),
                        mem_gen: 1,
                        busy: true,
                        warned_unknown: false,
                    },
                );
                needs_rebuild = true;
            }
            Some(row) => {
                if let Some(slot) = map.get(&world_key) {
                    if slot.busy {
                        // Deliberate leak: a prior panic/error may have left internal C state torn,
                        // so b3DestroyWorld would walk garbage (UB through the global allocator).
                        let s = map.remove(&world_key).unwrap();
                        mem::forget(s);
                        log::warn!(
                            "box3d-stdb: world {world_key} was poisoned, forcing cold rebuild"
                        );
                    } else if slot.mem_gen != row.generation {
                        // Stale slot (e.g. server restart with persisted row). Generation mismatch
                        // means this slot was never mid-mutation — safe to destroy via Drop.
                        map.remove(&world_key);
                    }
                    // else: warm hit — generation matches, slot is clean, use as-is
                }

                if !map.contains_key(&world_key) {
                    let world = box3d::World::try_with_capacity(params.gravity, params.capacity)
                        .map_err(|e| {
                            format!("box3d-stdb: world {world_key} create failed: {e:?}")
                        })?;
                    // busy until the step commits — see the creation path above.
                    map.insert(
                        world_key,
                        WorldSlot {
                            world,
                            keys: HashMap::new(),
                            keys_rev: HashMap::new(),
                            mem_gen: row.generation,
                            busy: true,
                            warned_unknown: false,
                        },
                    );
                    needs_rebuild = true;
                }
            }
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
                })
            };
            if let Err(e) = r {
                map.remove(&world_key);
                return Err(e);
            }
        }

        // Eager generation bump BEFORE game/step: if the reducer aborts (panic or Err), the DB
        // row already shows a new generation. Next entry finds a mismatch and forces cold rebuild
        // rather than reusing potentially-stale C state. Tick is NOT bumped here — a swallowed
        // game-Err would commit a tick that never stepped; it advances post-step.
        let cur = ctx.db.b3_world().world_key().find(world_key).unwrap();
        let new_gen = cur.generation + 1;
        ctx.db.b3_world().world_key().update(B3WorldRow {
            world_key,
            generation: new_gen,
            tick: cur.tick,
        });
        {
            let slot = map.get_mut(&world_key).unwrap();
            slot.mem_gen = new_gen;
            slot.busy = true;
        }

        let game_result = {
            let slot = map.get_mut(&world_key).unwrap();
            game(&mut WorldCtx {
                world: &slot.world,
                keys: &mut slot.keys,
                keys_rev: &mut slot.keys_rev,
                ctx,
                world_key,
            })
        };

        match game_result {
            // Leave busy = true: the C world may be half-mutated. The tx rollback restores tables
            // but not C heap state, so poisoning forces cold rebuild on next entry.
            Err(e) => Err(e),
            Ok(r) => {
                let slot = map.get_mut(&world_key).unwrap();
                slot.world.step(params.dt, params.substeps);

                // Commit simulation moves to the mirror. BodyEvents<'world> borrows the world, so
                // collect into owned data first — the borrow must end before we take
                // &mut slot.warned_unknown and write mirror rows.
                let moves: Vec<_> = slot.world.body_events().moves().collect();
                for ev in moves {
                    let bits = ev.body.to_bits();
                    let Some(body_key) = slot.keys_rev.get(&bits).copied() else {
                        // Warn once, not per-body-per-tick: a raw-created body (not spawned
                        // through the glue) has no mirror row, and the log would otherwise flood.
                        if !slot.warned_unknown {
                            log::warn!(
                                "box3d-stdb: world {world_key} moved a body not spawned through \
                                 the glue; its mirror row is not tracked"
                            );
                            slot.warned_unknown = true;
                        }
                        continue;
                    };
                    let raw = body_raw(bits);
                    let lin = unsafe { sys::b3Body_GetLinearVelocity(raw) };
                    let ang = unsafe { sys::b3Body_GetAngularVelocity(raw) };
                    if let Some(mut row) = ctx.db.b3_body().body_key().find(body_key) {
                        let t = ev.transform;
                        row.px = t.p.x;
                        row.py = t.p.y;
                        row.pz = t.p.z;
                        row.qx = t.q.v.x;
                        row.qy = t.q.v.y;
                        row.qz = t.q.v.z;
                        row.qw = t.q.s;
                        row.vx = lin.x;
                        row.vy = lin.y;
                        row.vz = lin.z;
                        row.wx = ang.x;
                        row.wy = ang.y;
                        row.wz = ang.z;
                        row.asleep = ev.fell_asleep;
                        ctx.db.b3_body().body_key().update(row);
                    }
                }

                // Event collection: shape → body → keys_rev → Option<u64>.
                let keys_rev = &slot.keys_rev;
                let contact = slot.world.contact_events();
                let sensor = slot.world.sensor_events();
                let events = StepEvents {
                    contact_begins: contact
                        .begins()
                        .map(|e| touch(keys_rev, e.shape_a.to_bits(), e.shape_b.to_bits()))
                        .collect(),
                    contact_ends: contact
                        .ends()
                        .map(|e| touch(keys_rev, e.shape_a.to_bits(), e.shape_b.to_bits()))
                        .collect(),
                    hits: contact
                        .hits()
                        .map(|e| ContactHit {
                            body_key_a: shape_body_key(keys_rev, e.shape_a.to_bits()),
                            body_key_b: shape_body_key(keys_rev, e.shape_b.to_bits()),
                            point: (e.point.x, e.point.y, e.point.z),
                            normal: (e.normal.x, e.normal.y, e.normal.z),
                            approach_speed: e.approach_speed,
                        })
                        .collect(),
                    // Sensor begin/end reuse ContactTouch: sensor→body_key_a, visitor→body_key_b.
                    sensor_begins: sensor
                        .begins()
                        .map(|e| touch(keys_rev, e.sensor.to_bits(), e.visitor.to_bits()))
                        .collect(),
                    sensor_ends: sensor
                        .ends()
                        .map(|e| touch(keys_rev, e.sensor.to_bits(), e.visitor.to_bits()))
                        .collect(),
                };

                // Tick advances only now — after the step and its mirror commit actually happened.
                ctx.db.b3_world().world_key().update(B3WorldRow {
                    world_key,
                    generation: new_gen,
                    tick: cur.tick + 1,
                });

                slot.busy = false;
                Ok(StepResult { value: r, events })
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
