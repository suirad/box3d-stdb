import * as THREE from 'three';
import { OrbitControls } from 'three/addons/controls/OrbitControls.js';
import { DbConnection, tables } from './module_bindings/index.js';

// ── Config ────────────────────────────────────────────────────────────────────
const params = new URLSearchParams(location.search);
// Set MAINCLOUD_DB to the name you published to Maincloud (names are global, so pick a unique one).
const MAINCLOUD_DB = 'box3d-sandbox';
// Default target: a local server when the page itself is served from localhost (dev / preview),
// otherwise the Maincloud deployment over wss (a ws:// socket is mixed-content blocked on an https
// page). `?server=`/`?db=` override either — e.g. to reach a local module from another machine.
const isLocalHost = location.hostname === 'localhost' || location.hostname === '127.0.0.1';
const SERVER = params.get('server') ?? (isLocalHost ? 'ws://localhost:3000' : 'wss://maincloud.spacetimedb.com');
const DB_NAME = params.get('db') ?? (isLocalHost ? 'box3d-sandbox' : MAINCLOUD_DB);
const WORLD_KEY = 1n;

// Energy cost, calibrated against the Maincloud usage dashboard (2026-07-06). Real billing rate:
// 1 CPU-second = 1 TeV (2e9 fuel/TeV) — the OSS host's "1 eV = 1 fuel" is an internal placeholder,
// the true rate is 500x higher. Step fuel was MEASURED with a dedicated no-sleep body sweep
// (~7.3k fuel empty, ~20k fuel per awake body, stable from 8 bodies up); asleep bodies are ~4x
// cheaper (the solver skips them). Each awake body also drives one mirror-row write/tick — priced
// from the dashboard's write/scan/seek rates (~1.2e-5 TeV), about doubling the per-body cost.
const SECONDS_PER_MONTH = 2_592_000; // 30 days
const FREE_TIER_TEV_MO = 2500; // SpacetimeDB free tier allotment
const TEV_PER_DOLLAR = 2592; // marginal overage rate
const TICK_HZ = 60;
const BASE_STEP_TEV = 3.65e-6; // empty step: ~7.3k fuel / 2e9
const PER_AWAKE_TEV = 2.18e-5; // per awake body/tick: ~20k-fuel compute + one mirror write (dashboard IO)

// ── Three.js scene ────────────────────────────────────────────────────────────
const renderer = new THREE.WebGLRenderer({ antialias: true });
renderer.setPixelRatio(devicePixelRatio);
renderer.setSize(innerWidth, innerHeight);
document.body.appendChild(renderer.domElement);

const scene = new THREE.Scene();
scene.background = new THREE.Color(0x1a1a2e);

// Z-up camera to match physics coordinate system
const camera = new THREE.PerspectiveCamera(60, innerWidth / innerHeight, 0.1, 500);
camera.up.set(0, 0, 1);
camera.position.set(20, -20, 12);
camera.lookAt(0, 0, 0);

const controls = new OrbitControls(camera, renderer.domElement);
controls.target.set(0, 0, 0);
controls.update();

scene.add(new THREE.AmbientLight(0xffffff, 0.6));
const sun = new THREE.DirectionalLight(0xffffff, 1.2);
sun.position.set(10, -10, 20);
scene.add(sun);

// Visual ground at z=0 (physics ground is server-side)
scene.add(new THREE.Mesh(
  new THREE.PlaneGeometry(30, 30),
  new THREE.MeshLambertMaterial({ color: 0x222244 })
));

// GridHelper is XZ-plane by default; rotate to XY to match Z-up.
const grid = new THREE.GridHelper(30, 15, 0x444466, 0x333355);
grid.rotation.x = Math.PI / 2;
scene.add(grid);

window.addEventListener('resize', () => {
  camera.aspect = innerWidth / innerHeight;
  camera.updateProjectionMatrix();
  renderer.setSize(innerWidth, innerHeight);
});

// Poses applied in table callbacks; render loop just draws whatever the scene holds
renderer.setAnimationLoop(() => {
  controls.update();
  renderer.render(scene, camera);
});

// ── State ─────────────────────────────────────────────────────────────────────
interface Pose {
  px: number; py: number; pz: number;
  qx: number; qy: number; qz: number; qw: number;
  asleep: boolean;
}
const meshes = new Map<bigint, THREE.Mesh>();
// Buffers poses so a b3_body row arriving before its game_body row isn't lost.
const latestPose = new Map<bigint, Pose>();

// ── HUD ───────────────────────────────────────────────────────────────────────
const MAX_BODIES = 64; // mirrors the module's cap; used for the worst-case "max" figure
const hud = document.getElementById('hud')!;
// Split the bar so the cost can be a clickable link that opens the methodology popup.
const infoSpan = document.createElement('span');
const costLink = document.createElement('a');
costLink.href = '#';
costLink.style.cssText = 'color:#8cf;text-decoration:underline;cursor:pointer;margin-left:8px';
hud.textContent = '';
hud.append(infoSpan, costLink);

let connStatus = 'connecting…';
let pitCount = 0n;
let userCount = 0;
let shootImpulse = 0;
let launchSpeed = 0;
let estNowMo = 0;
let estAvgMo = 0;
let estMaxMo = 0;
let awakeCount = 0;
let estTotalTev = 0;
let connectedSecs = 0;
let lastAccrue = performance.now();
type Mode = 'Spawn' | 'Shoot' | 'Launch';
let activeMode: Mode = 'Spawn';
let activeKind = 0; // 0=box, 1=ball

function awakeBodies(): number {
  // Only game bodies have meshes; the mirror also holds the static ground/pit, which never sleep
  // and would otherwise inflate the count.
  let n = 0;
  for (const [key, p] of latestPose) if (!p.asleep && meshes.has(key)) n++;
  return n;
}

function refreshHUD() {
  const kind = activeKind === 0 ? 'Box' : 'Ball';
  infoSpan.textContent =
    `${connStatus} | users: ${userCount} | pit: ${pitCount} | bodies: ${meshes.size} (${awakeCount} awake)` +
    ` | ${activeMode} ${kind} | forces  shoot ${shootImpulse}  launch ${launchSpeed} |`;
  costLink.textContent =
    ` est/mo: now $${estNowMo.toFixed(2)} · avg $${estAvgMo.toFixed(2)} · max $${estMaxMo.toFixed(2)} ⓘ`;
}
refreshHUD();

// Accrue estimated energy over real elapsed time. Ticking only runs while a client is connected,
// and this one being connected guarantees it — so gate accrual on the connection, not a timer alone.
const dollarsMo = (tevPerSec: number) => (tevPerSec * SECONDS_PER_MONTH) / TEV_PER_DOLLAR;
setInterval(() => {
  const now = performance.now();
  const dt = (now - lastAccrue) / 1000;
  lastAccrue = now;
  const connected = connStatus === 'connected';
  awakeCount = connected ? awakeBodies() : 0;
  const nowTevPerSec = connected ? TICK_HZ * (BASE_STEP_TEV + PER_AWAKE_TEV * awakeCount) : 0;
  if (connected) {
    estTotalTev += nowTevPerSec * dt;
    connectedSecs += dt;
  }
  estNowMo = dollarsMo(nowTevPerSec);
  // avg = this session's real usage (bodies sleep → far below the ceiling); max = full arena awake.
  estAvgMo = connectedSecs > 0 ? dollarsMo(estTotalTev / connectedSecs) : 0;
  estMaxMo = dollarsMo(TICK_HZ * (BASE_STEP_TEV + PER_AWAKE_TEV * MAX_BODIES));
  refreshHUD();
}, 250);

// ── Cost methodology popup ──────────────────────────────────────────────────────
const costModal = document.createElement('div');
costModal.style.cssText =
  'position:fixed;inset:0;background:rgba(0,0,0,0.7);display:none;align-items:center;' +
  'justify-content:center;z-index:10;font:13px/1.5 system-ui,sans-serif';
costModal.innerHTML =
  `<div style="max-width:560px;background:#1a1a2e;color:#ddd;border:1px solid #445;border-radius:8px;padding:20px 24px">
    <h3 style="margin:0 0 8px">How this cost is estimated</h3>
    <p>An <b>estimate</b>, not a bill — SpacetimeDB exposes no live per-reducer energy to a client.
       The billing rates below were obtained by generating a known workload <b>on Maincloud</b> and
       reading the energy it actually consumed from the usage dashboard.</p>
    <ul style="margin:8px 0;padding-left:18px">
      <li><b>Unit:</b> energy is metered in TeV. Real rate: <b>1 CPU-second = 1 TeV</b>
          (2×10⁹ WASM instructions), and <b>2,592 TeV = $1</b>.</li>
      <li><b>Per body:</b> the 60&nbsp;Hz physics tick was measured at ~20,000 instructions per
          <i>awake</i> body per step (a dedicated no-sleep sweep read off the reducer
          <code>spacetime-energy-used</code> header). Asleep bodies cost ~4× less — the solver skips
          them — so cost tracks the awake count, not the total. Each awake body also drives one
          mirror-row write/tick, priced from the dashboard's write/scan/seek rates.</li>
      <li><b>now</b> = the current awake bodies, projected to a full month at 60&nbsp;Hz, 24/7.</li>
      <li><b>avg</b> = this session's <i>actual</i> usage projected out — bodies settle and sleep, so
          this is the honest typical cost and sits far below the ceiling.</li>
      <li><b>max</b> = the whole ${MAX_BODIES}-body arena awake 24/7 — the absolute worst case.</li>
    </ul>
    <p style="color:#9ab">The compute figure is measured; the per-body write cost is estimated from
       the dashboard rates. Storage and bandwidth are negligible here. Click anywhere to close.</p>
  </div>`;
document.body.appendChild(costModal);
costLink.addEventListener('click', (e) => { e.preventDefault(); costModal.style.display = 'flex'; });
costModal.addEventListener('click', () => { costModal.style.display = 'none'; });

// ── Toolbar ───────────────────────────────────────────────────────────────────
const toolbar = document.querySelector<HTMLElement>('.toolbar-btns')!;
const modeBtns: HTMLButtonElement[] = [];

for (const m of ['Spawn', 'Shoot', 'Launch'] as Mode[]) {
  const btn = document.createElement('button');
  btn.textContent = m;
  btn.addEventListener('click', () => {
    activeMode = m;
    modeBtns.forEach(b => b.classList.remove('active'));
    btn.classList.add('active');
    refreshHUD();
  });
  toolbar.appendChild(btn);
  modeBtns.push(btn);
}
modeBtns[0].classList.add('active'); // Spawn is the default mode

const kindBtn = document.createElement('button');
kindBtn.textContent = 'Box';
kindBtn.classList.add('active');
kindBtn.addEventListener('click', () => {
  activeKind = activeKind === 0 ? 1 : 0;
  kindBtn.textContent = activeKind === 0 ? 'Box' : 'Ball';
  refreshHUD();
});
toolbar.appendChild(kindBtn);

// ── Pose helper ───────────────────────────────────────────────────────────────
function applyPose(mesh: THREE.Mesh, pose: Pose) {
  mesh.position.set(pose.px, pose.py, pose.pz);
  mesh.quaternion.set(pose.qx, pose.qy, pose.qz, pose.qw);
  // kind stored in userData on mesh create; asleep drives color
  const mat = mesh.material as THREE.MeshLambertMaterial;
  mat.color.setHex(pose.asleep ? 0x888888 : mesh.userData.kind === 0 ? 0x4488ff : 0xff8844);
}

// ── Connection ────────────────────────────────────────────────────────────────
const conn = DbConnection.builder()
  .withUri(SERVER)
  .withDatabaseName(DB_NAME)
  .onConnect((connection) => {
    connStatus = 'connected';
    refreshHUD();
    connection.subscriptionBuilder()
      .onApplied(() => refreshHUD())
      .subscribe([tables.b3_body, tables.game_body, tables.score, tables.connected, tables.tuning]);
  })
  .onConnectError((_ctx, err) => {
    connStatus = `error: ${err.message}`;
    refreshHUD();
  })
  .onDisconnect(() => {
    connStatus = 'disconnected';
    refreshHUD();
  })
  .build();

// ── Table callbacks ───────────────────────────────────────────────────────────
conn.db.game_body.onInsert((_ctx, row) => {
  if (meshes.has(row.bodyKey)) return; // resubscribe replays inserts; keep the live mesh
  const h2 = row.half * 2;
  const mesh = new THREE.Mesh(
    row.kind === 0
      ? new THREE.BoxGeometry(h2, h2, h2)
      : new THREE.SphereGeometry(row.half, 16, 12),
    new THREE.MeshLambertMaterial({ color: row.kind === 0 ? 0x4488ff : 0xff8844 })
  );
  mesh.userData.kind = row.kind;

  // Apply pose if b3_body arrived first
  const pose = latestPose.get(row.bodyKey);
  if (pose) applyPose(mesh, pose);

  scene.add(mesh);
  meshes.set(row.bodyKey, mesh);
  refreshHUD();
});

conn.db.game_body.onDelete((_ctx, row) => {
  const mesh = meshes.get(row.bodyKey);
  if (!mesh) return;
  scene.remove(mesh);
  mesh.geometry.dispose();
  (mesh.material as THREE.Material).dispose();
  meshes.delete(row.bodyKey);
  latestPose.delete(row.bodyKey);
  refreshHUD();
});

conn.db.b3_body.onInsert((_ctx, row) => {
  if (row.worldKey !== WORLD_KEY) return;
  latestPose.set(row.bodyKey, row);
  const mesh = meshes.get(row.bodyKey);
  if (mesh) applyPose(mesh, row);
});

conn.db.b3_body.onUpdate((_ctx, _old, row) => {
  if (row.worldKey !== WORLD_KEY) return;
  latestPose.set(row.bodyKey, row);
  const mesh = meshes.get(row.bodyKey);
  if (mesh) applyPose(mesh, row);
});

conn.db.b3_body.onDelete((_ctx, row) => {
  if (row.worldKey !== WORLD_KEY) return;
  latestPose.delete(row.bodyKey);
});

conn.db.score.onInsert((_ctx, row) => { pitCount = row.pitCount; refreshHUD(); });
conn.db.score.onUpdate((_ctx, _old, row) => { pitCount = row.pitCount; refreshHUD(); });

// Live user count = live connection rows. Read the cache count so a resubscribe replay can't drift it.
const countUsers = () => { userCount = Number(conn.db.connected.count()); refreshHUD(); };
conn.db.connected.onInsert(countUsers);
conn.db.connected.onDelete(countUsers);

const readTuning = (row: { shootImpulse: number; launchSpeed: number }) => {
  shootImpulse = row.shootImpulse;
  launchSpeed = row.launchSpeed;
  refreshHUD();
};
conn.db.tuning.onInsert((_ctx, row) => readTuning(row));
conn.db.tuning.onUpdate((_ctx, _old, row) => readTuning(row));

// ── Interaction ───────────────────────────────────────────────────────────────
const raycaster = new THREE.Raycaster();
const groundPlane = new THREE.Plane(new THREE.Vector3(0, 0, 1), 0);
let pdPos = { x: 0, y: 0 };
let pdTime = 0;

renderer.domElement.addEventListener('pointerdown', (e) => {
  pdPos = { x: e.clientX, y: e.clientY };
  pdTime = Date.now();
});

renderer.domElement.addEventListener('pointerup', (e) => {
  const ddx = e.clientX - pdPos.x;
  const ddy = e.clientY - pdPos.y;
  // Below 5 px / 300 ms it's a click action; anything larger belongs to OrbitControls.
  if (ddx * ddx + ddy * ddy > 25 || Date.now() - pdTime >= 300) return;

  const ndc = new THREE.Vector2(
    (e.clientX / innerWidth) * 2 - 1,
    -(e.clientY / innerHeight) * 2 + 1
  );
  raycaster.setFromCamera(ndc, camera);

  if (activeMode === 'Spawn') {
    const hit = new THREE.Vector3();
    if (raycaster.ray.intersectPlane(groundPlane, hit)) {
      conn.reducers.spawnBody({ kind: activeKind, x: hit.x, y: hit.y });
    }
  } else if (activeMode === 'Shoot') {
    const o = raycaster.ray.origin;
    const d = raycaster.ray.direction;
    conn.reducers.shoot({ ox: o.x, oy: o.y, oz: o.z, dx: d.x, dy: d.y, dz: d.z });
  } else {
    // Launch: spawn at camera position, fire along ray direction
    const o = camera.position;
    const d = raycaster.ray.direction;
    conn.reducers.launch({ kind: activeKind, ox: o.x, oy: o.y, oz: o.z, dx: d.x, dy: d.y, dz: d.z });
  }
});
