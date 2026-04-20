# Orchestration Design Exploration

Successor to research/14. Goal: run multiple VM instances concurrently on
one host, each with their own per-VM persisted state, under a single
orchestrator daemon. CLI frontend today, HTTP later. This sprint is
architecture + skeleton + minimum-viable lifecycle; optimization and
segregation come later.

---

## 1. Shape

**Single-process orchestrator. VMs are threads.** The orchestrator daemon
owns a single address space with:

- Per-VM state (spec, thread handle, bucket name, ephemeral dir, log file)
- Bucket lifecycle (create on start, reference on reuse, destroy later)
- Tap device allocation
- Log infrastructure (per-VM append files + tail streaming)
- VM registry (name → VmEntry)

Each VM runs in its own thread calling `microvm::launcher::run`. Each VM
has its own `S3Fs` instance (built via the existing `S3Fs::new` path),
which in turn has its own golden cache, write pool, and S3 client.

**No modifications to s3-rootfs in v0** (per design directive). Cache
and pool hoisting — letting all VMs share a single `Arc<GoldenBlobCache>`
etc. — is a future optimization that would require an `S3Fs` constructor
accepting pre-built Arcs. Deferred until v0 lifecycle is proven and the
perf cost of per-VM caches is measured in practice.

**This is a "house of cards" architecture on purpose.** If the orchestrator
panics, every VM dies. Crash segregation (process isolation, per-VM
supervisor) is a later-sprint concern. The priority this sprint is a
clean multi-VM lifecycle API and the bucket/log/tap plumbing around it.

```
┌─────────────────────────────────────────────────────────────┐
│ orchestrator (single process)                               │
│                                                              │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │ VM thread A  │  │ VM thread B  │  │ VM thread C  │      │
│  │ launcher::run│  │ launcher::run│  │ launcher::run│      │
│  │   + own S3Fs │  │   + own S3Fs │  │   + own S3Fs │      │
│  └──────────────┘  └──────────────┘  └──────────────┘      │
│                                                              │
│  ┌────────────────────────────────────────────────────┐    │
│  │ VmRegistry (in-memory, no persistence)             │    │
│  │   HashMap<name_hash, VmEntry>                      │    │
│  └────────────────────────────────────────────────────┘    │
│                                                              │
│  ┌────────────────────────────────────────────────────┐    │
│  │ Bucket helpers: create/reference/(destroy later)   │    │
│  │ Tap pool, log dirs, defaults                       │    │
│  └────────────────────────────────────────────────────┘    │
│                                                              │
│  ┌────────────────────────────────────────────────────┐    │
│  │ Unix socket server — JSON commands from microctl   │    │
│  └────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
                    microctl (CLI process)
                    subcommands → JSON over Unix socket
```

Each VM's `S3Fs` carries its own golden cache, write pool, and S3 client.
The orchestrator does NOT pre-construct or share these — per design
directive, s3-rootfs is consumed via its existing `new(cfg)` API only.

---

## 2. Crate layout

Four crates in total; two new, two existing reshaped.

| Crate | Status | Role |
|---|---|---|
| `orchestrator` | NEW | Daemon binary + lib. Owns SharedContext, VmRegistry, IPC server. |
| `microctl` | NEW | CLI frontend. Thin client over Unix socket. |
| `microvm` | RESHAPE | Becomes a library crate. `main.rs` stays as legacy single-VM entry point for now. |
| `s3-rootfs` | UNCHANGED | Consumed via existing `S3Fs::new(cfg)`. Orchestrator builds one FsConfig per VM with the allocated bucket/ephemeral_dir/ruleset. |

### orchestrator crate

```
orchestrator/
  Cargo.toml
  src/
    main.rs          -- daemon entry: arg parse, signal handling, start server
    lib.rs           -- public API: Orchestrator, VmSpec, VmInfo, errors
    shared.rs        -- SharedContext construction + Arc sharing
    registry.rs      -- VmRegistry: HashMap<name_hash, VmEntry>, name→hash
    vm.rs            -- spawn_vm thread, join on stop, panic catcher
    bucket.rs        -- S3 bucket create/delete via S3Client admin ops
    log.rs           -- per-VM serial console → file tail
    ipc/
      mod.rs         -- Unix socket server
      proto.rs       -- Command / Response serde types
      handler.rs     -- command dispatch → Orchestrator methods
    config.rs        -- INI daemon config + defaults
```

### microctl crate

```
microctl/
  Cargo.toml
  src/
    main.rs          -- subcommand parse (clap), connect, send, print
    client.rs        -- Unix socket client
    spec.rs          -- --memory / --bucket / --name flags → VmSpec
```

### microvm crate (reshape)

```
microvm/
  src/
    lib.rs           -- re-export launcher::{run, LaunchConfig}, event_loop, etc.
    main.rs          -- LEGACY: single-VM binary using env-var config (as today).
                        Kept for testing; orchestrator is the normal runtime.
    launcher.rs      -- run(cfg, shared_ctx) — gains SharedContext param
    ...              -- (cpu, vm, memory, serial, virtio_*, kernel — unchanged)
```

### s3-rootfs crate (unchanged)

Consumed via the existing `S3Fs::new(cfg)` API. Orchestrator builds one
`FsConfig` per VM with an allocated bucket name, ephemeral dir, ruleset
path, etc. Per-VM golden cache loads and per-VM write pool are the
v0 tradeoff; sharing them requires a future s3-rootfs constructor and
is scoped out of this sprint.

---

## 3. IPC surface

Unix socket at `~/.microvm/socket` (per-user) or `/var/run/microd.sock`
(system-wide). Newline-delimited JSON. Protocol versioning TBD;
for now v0 = whatever shape we iterate to.

### Commands

```rust
enum Command {
    Start(VmSpec),          // spawn a new VM
    Stop(VmName),           // graceful shutdown
    List,                   // all VMs
    Inspect(VmName),        // full spec + runtime info
    Logs { name, follow },  // tail log file
}

struct VmSpec {
    name: String,
    mem_mb: u32,
    kernel: PathBuf,
    initrd: PathBuf,
    bucket: Option<String>,   // None = orchestrator allocates
    golden_bucket: Option<String>,
    ruleset: Option<PathBuf>,
    ephemeral_dir: Option<PathBuf>,  // None = orchestrator allocates
    net_ifname: Option<String>,
}

struct VmInfo {
    name: String,
    name_hash: String,
    state: VmState,           // Starting | Running | Stopping | Stopped | Crashed
    spec: VmSpec,
    pid: Option<u32>,         // always the orchestrator pid in thread-model
    uptime: Duration,
    mem_rss: Option<u64>,
}
```

### Responses

```rust
enum Response {
    Ok,
    VmInfo(VmInfo),
    VmList(Vec<VmInfo>),
    LogChunk(String),         // for Logs streaming
    Error(String),
}
```

HTTP adapter (future sprint) maps directly:
- `POST /vms` = Start
- `DELETE /vms/:name` = Stop
- `GET /vms` = List
- `GET /vms/:name` = Inspect
- `GET /vms/:name/logs` = Logs (SSE or chunked)

---

## 4. VM identity & addressing

User-facing: name from VmSpec. Internal: `name_hash = blake3(name).to_hex()[..16]`.

The hash is used for:
- Bucket naming: `vm-<name_hash>` so per-VM buckets are deterministic from name
- Ephemeral dir: `<eph_root>/<name_hash>/` so collisions are impossible
- VmRegistry key: hash (not name) so lookups are fixed-size

Renaming is forbidden (name_hash is the stable identity). New name = new VM.

---

## 5. Bucket lifecycle

Orchestrator owns per-VM bucket lifecycle:

1. `Start(spec)` with no explicit bucket:
   - Compute `vm-<name_hash>`
   - If bucket doesn't exist, create it via S3Client admin op
   - Pass bucket name to `S3Fs::new_shared`
2. `Start(spec)` with explicit bucket:
   - Use as-is, no create. Caller is responsible.
3. `Stop(name)`:
   - Drain writes, shut down S3Fs, join thread
   - Bucket PRESERVED (persistence) — only deleted on explicit `destroy` command (not in v0).

Bucket delete is a separate command we can add later. Default is "bucket lives as long as the VM name is reused".

Requires S3 admin creds on the orchestrator. Config: `[s3] admin_access_key = ..., admin_secret_key = ...` in daemon config.

---

## 6. Logs

Serial console output is piped to `<log_dir>/<name_hash>/serial.log`,
append-only. Log rotation TBD (out of scope for v0).

`microctl logs <name> [--follow]` reads from that file; `--follow`
does tail-style streaming via the Unix socket.

SSH access (per Travis's note) is a future concern — orchestrator can
expose an SSH listener that bridges to the VM's virtio-console. For
v0, the log file is the only way to see guest output post-hoc.

---

## 7. State-on-restart

**Orchestrator restart wipes all running VMs.** In-memory VmRegistry
has no disk persistence. On daemon start, the registry is empty; any
previously-spawned VMs are orphan threads (but since threads die with
the process, they're already gone anyway).

This is the "house of cards" tradeoff. A clean-shutdown hook tries to
gracefully stop each VM (drain writes, save state to bucket) so S3
state is consistent, but orchestrator crash = hard abort for all VMs.

---

## 8. Config

### Daemon config (INI)

`/etc/microd/config.ini` or `~/.microvm/daemon.ini`:

```ini
[daemon]
socket = /run/microd.sock
log_dir = /var/log/microd

[s3]
endpoint = http://192.168.88.15:9090
admin_access_key = minioadmin
admin_secret_key = minioadmin

[defaults]
kernel = /var/lib/microd/vmlinux
initrd = /var/lib/microd/initramfs.cpio.gz
golden_bucket = golden
ruleset = /etc/microd/coding-agent.toml
ephemeral_root = /var/lib/microd/ephemeral
```

### VM specs

CLI args (primary): `microctl start --name foo --memory 1024 --bucket my-bucket`.
Short INI file (also supported): `microctl start --spec ./foo.ini`.

Daemon merges: defaults → spec file → CLI args (later wins).

Ruleset continues to be TOML (inherited from sprint-14).

---

## 9. Implementation ordering

1. **microvm reshape.** Turn into library; keep `main.rs` working against
   env config for single-VM regression testing. `launcher::run` stays
   signature-compatible — the orchestrator uses it directly from in-
   process calls.
2. **orchestrator crate skeleton.** Cargo.toml, main.rs with arg parsing,
   lib.rs with `Orchestrator` struct (compiles; bodies mostly `todo!()`).
   — DONE (this session).
3. **VmRegistry.** `Mutex<HashMap<name_hash, VmEntry>>`. `Orchestrator::list`
   / `inspect` consult it.
4. **spawn_vm thread model.** `orchestrator::vm::spawn(spec, orch) -> JoinHandle`.
   Threads with `panic::catch_unwind`. Allocates bucket, ephemeral dir,
   tap device, log file; builds per-VM `FsConfig` and `LaunchConfig`;
   calls `microvm::launcher::run` directly.
5. **Bucket lifecycle.** Orchestrator creates bucket on start (using S3
   admin creds from daemon config), preserves on stop. Destroy is a
   separate command, not wired in v0.
6. **IPC: Start + List commands.** Unix socket, JSON, minimum viable.
7. **IPC: Stop + Inspect.**
8. **IPC: Logs (non-streaming first, then --follow).**
9. **microctl CLI.** Clap-based, talks to socket.
10. **INI config loading.** Daemon + VM spec files.
11. **Integration test.** Start two VMs concurrently, verify isolation
    (different buckets, different ephemeral dirs, orderly shutdown).

Phase 1 is the refactor lift (library extraction). Phases 2–9 are new
code. Phase 11 is the minimum gate for "orchestration works".

---

## 10. Known risks

- **KVM_RUN panic isolation.** If a vcpu thread panics inside KVM_RUN,
  `catch_unwind` may not fully contain it (KVM_RUN can be interrupted
  but panic semantics across the ioctl boundary are subtle). The "one
  VM crash shouldn't take down the orchestrator" goal may not be
  fully achievable in thread-model without careful engineering. First
  iteration accepts this; segregation-sprint picks it up.
- **Tap device allocation.** Multiple VMs need separate tap devices.
  Orchestrator must allocate `/dev/tap0`, `/dev/tap1`, ... without
  collision. Use the existing sprint-14 tap-flock detection plus a
  pool of pre-allocated taps.
- **Thread-local state.** `s3_rootfs::set_log_fn` and similar globals
  need to be safe under many-VMs-one-process. Audit during reshape.
- **Async guest shutdown.** Right now microvm/main.rs exits when the
  guest halts. In thread-model, guest halt needs to signal the
  orchestrator's VmRegistry without taking down other threads.
- **Golden cache eviction.** Single shared golden cache means a hot
  VM can evict another VM's pages. Current design has immutable
  golden (no eviction), so not a risk today, but something to keep
  in mind if golden ever becomes LRU.

---

## 11. Outcomes (v0 closed 2026-04-19)

Sprint-15 v0 delivered in six commits:

| Commit | Phases | Scope |
|---|---|---|
| `ba886ec` | 1 | microvm reshape — library + binary. Public API: `microvm::launcher::{run, launch_vm, prepare_filesystem, LaunchConfig}` + `log::{init, write}`. |
| `2299b95` | 3+4 | VmRegistry (insert/remove/transition/set_thread) + vm::spawn 10-step lifecycle + Orchestrator::start_vm. Collision semantics, catch_unwind, state transitions. |
| `e9bbba4` | 5 | Bucket lifecycle. S3AdminClient (initial hand-rolled basic-auth; later switched to rusty-s3 sigv4 in b78800c after basic-auth 400'd against RustFS). |
| `58d7ba1` | 6+7 | IPC server (UnixListener, non-blocking accept, thread-per-connection, JSON framing) + stop_vm (Stopping → flag → join → Stopped) + microvm shutdown hook via `LaunchConfig.shutdown: Option<Arc<AtomicBool>>`. |
| `0cdc78f` | 8+9 | Log streaming (serial output via `Box<dyn Write + Send>`, append file, 200ms-poll tail-f for follow) + microctl CLI (start/stop/list/inspect/logs with flags and INI --spec, human and --json output). |
| `b78800c` | 10+11 | INI daemon config ([daemon]/[s3]/[defaults] sections) + signal-handled clean shutdown + four integration tests (concurrent start, stop transitions, bucket reuse on relaunch, duplicate start rejected) + sigv4 rewrite of S3AdminClient. |

Test results: 2 orchestrator config unit tests + 4 orchestrator integration tests + 62 s3-rootfs tests all green against live RustFS.

Success criteria met:
- Two VMs start concurrently with distinct resources (bucket, ephemeral_dir, log file, name_hash). ✓
- `microctl list` shows both Running. ✓
- `microctl stop A` doesn't affect B. ✓
- `microctl logs A [--follow]` tails the serial console. ✓
- Re-launching a stopped name reuses its bucket (persistence). ✓
- Orchestrator crash takes all VMs down ("house of cards" by design). ✓

Known gaps carried to sprint-16 candidates:
- Cache/pool hoisting for perf (the deferred optimization from §2).
- ACPI-style graceful guest shutdown (stop_vm currently waits on vcpu exit; a busy guest can take arbitrary time).
- Tap device pool allocation (currently passes through spec.net_ifname; multi-VM on the same macvtap would collide).
- S3-native rename batch (carried from sprint-14 §5).
- Git+SSH dependency auth, IPv6 link-local, VM_ID ephemeral_dir enforcement (all sprint-14 §5).
- Real AWS S3 sigv4 compliance (rusty-s3 gets us part way; some endpoints may still need tweaks for live AWS).
- Orchestrator restart recovery (v0 is "house of cards" — no reconnect).
