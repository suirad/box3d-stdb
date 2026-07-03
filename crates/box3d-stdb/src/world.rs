use std::{cell::RefCell, collections::HashMap, mem};

use box3d::Vec3;
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

/// Handle to the live world inside [`with_world`] closures.
///
/// Mutation helpers that keep the durable mirror consistent are planned; until then [`world`]
/// (see [`WorldCtx::world`]) is the raw escape hatch.
pub struct WorldCtx<'a> {
    world: &'a box3d::World,
}

impl WorldCtx<'_> {
    /// Raw access to the underlying [`box3d::World`].
    ///
    /// Anything you create through it must be recreated by your `rebuild` closure after a cache
    /// drop — the glue only rebuilds what your `rebuild` puts back.
    pub fn world(&self) -> &box3d::World {
        self.world
    }
}

struct WorldSlot {
    world: box3d::World,
    mem_gen: u64,
    busy: bool,
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
) -> Result<R, String> {
    WORLDS.with(|cell| -> Result<R, String> {
        // Single-threaded module: the only way this borrow can fail is a reentrant call from
        // inside a with_world closure — a consumer bug, so panic rather than thread a Result.
        let mut map = cell
            .try_borrow_mut()
            .expect("box3d-stdb: with_world called from inside a with_world closure");

        let row = ctx.db.b3_world().world_key().find(world_key);
        let mut needs_rebuild = false;

        match &row {
            None => {
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
                        mem_gen: 1,
                        busy: true,
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
                            mem_gen: row.generation,
                            busy: true,
                        },
                    );
                    needs_rebuild = true;
                }
            }
        }

        if needs_rebuild {
            let r = rebuild(&mut WorldCtx {
                world: &map[&world_key].world,
            });
            if let Err(e) = r {
                map.remove(&world_key);
                return Err(e);
            }
        }

        // Eager generation bump BEFORE game/step: if the reducer aborts (panic or Err), the DB
        // row already shows a new generation. Next entry finds a mismatch and forces cold rebuild
        // rather than reusing potentially-stale C state (invariant I1).
        let cur = ctx.db.b3_world().world_key().find(world_key).unwrap();
        let new_gen = cur.generation + 1;
        ctx.db.b3_world().world_key().update(B3WorldRow {
            world_key,
            generation: new_gen,
            tick: cur.tick + 1,
        });
        {
            let slot = map.get_mut(&world_key).unwrap();
            slot.mem_gen = new_gen;
            slot.busy = true;
        }

        let game_result = game(&mut WorldCtx {
            world: &map[&world_key].world,
        });

        match game_result {
            // Leave busy = true: the C world may be half-mutated. The tx rollback restores tables
            // but not C heap state, so poisoning forces cold rebuild on next entry.
            Err(e) => Err(e),
            Ok(r) => {
                map[&world_key].world.step(params.dt, params.substeps);
                map.get_mut(&world_key).unwrap().busy = false;
                Ok(r)
            }
        }
    })
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
    Ok(())
}
