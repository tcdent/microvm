# Workload Tuning Sprint

Successor to research/13. That sprint landed deferred writes + cross-handle
read cache + golden-FS boot work. This one targets the shape of real
workloads — installers, package managers, source control, build systems —
and the infrastructure that lets us tune per-workload without hardcoding
heuristics.

Scope is deliberately broad but ordered: infrastructure first, then
first consumers of that infrastructure, then independent wins, then the
pre-existing correctness/bootstrap work that's been queued.

---

## 1. Per-workload ruleset engine

The single biggest architectural lever in the sprint. Everything in §2
and §3 depends on it.

**Shape.** TOML-driven path→behavior config, loaded at mount. Globs
compile to a trie/DFA so resolution is O(path depth), not O(rules).
Resolve once per `open()`; cache the result on the file handle so every
subsequent read/write/release is a pointer deref, not a trie walk.

```toml
# Example: coding-agent workload
[[rule]]
glob = "**/.npm/_cacache/**"
durability = "ephemeral"
readahead = "off"

[[rule]]
glob = "**/node_modules/**"
durability = "local"
coalesce_ms = 250

[[rule]]
glob = "**/.git/objects/**"
durability = "durable"
readahead = "aggressive"
```

**Defaults.** Any path not matching a rule gets `durability = durable`,
`readahead = auto` (i.e. dynamic, §4), no coalescing. Conservative —
opt-in behavior, not opt-out.

**File handle carries:**
```rust
struct HandleRules {
    durability: Durability,   // Ephemeral | Local | Durable
    readahead:  ReadAhead,    // Off | Auto | Aggressive
    coalesce:   Option<Duration>,
}
```

Populated once at `open()`/`create()` time from the compiled ruleset,
then consulted without further trie walks.

---

## 2. Durability class routing

Consumes §1. Extends `WritePool::enqueue` to branch on the durability
class of the path.

| Class | Behavior |
|---|---|
| **Ephemeral** | Never touches S3. Inflight map holds the Arc; on `release()` complete + ruleset timeout, drop. Guest reads still see bytes via the inflight consult path; evictions are RAM-pressure driven. |
| **Local** | Drains to a local journal (NVMe on host) for async drain to S3. fsync() hits the journal, not S3. Fast sync semantics, S3 propagation in background. |
| **Durable** | Current behavior: drain to S3 via worker pool with exponential backoff. fsync() blocks until the inflight entry clears. |

Ephemeral is the single biggest PUT-volume reduction for cache-heavy
workloads. For coding agents: `~/.npm/_cacache`, `~/.cache/pip`,
`node_modules/.cache`, `target/` — all legitimate ephemeral candidates.
Expected drop: substantial, measure pre/post.

Local durable journal is §2a — carve out as its own work item since it
introduces host-side disk state and a recovery story.

### 2a. Local durable journal (deferred from §2)

Only matters if we care about host-crash durability (vs VM-crash
durability, which the current RAM-buffered inflight map already
handles). For coding agents, state of truth is git/S3 — agent state
surviving host crash is nice-to-have, not critical.

**Defer until** a concrete workload asks for host-crash survival.
Until then, `Local` class = `Durable` class + a larger coalesce
window. Once a workload needs it, land the NVMe journal as a
follow-up.

---

## 3. Coalesce windows

Consumes §1 (rules specify the window per path). Extends the write
pool with a debounce layer.

**Current behavior.** Inflight-map-replace gives partial coalescing:
- B arrives before worker dequeues A → collapsed to 1 PUT ✓
- B arrives mid-PUT of A → A's PUT wasted, B PUT separately ✗
- B arrives after A's PUT → 2 PUTs (correct, distinct in time)

**New behavior.** A write to a path with a `coalesce_ms` rule enters
inflight but does NOT enqueue immediately. A per-path timer starts;
subsequent writes restart the timer. Timer expiry enqueues for PUT.
This collapses rapid-fire writes to a single PUT and eliminates the
mid-PUT race entirely.

Debounce timer lives in the write pool's own scheduler thread (not
a per-path thread — cost would explode). Single `BTreeMap<Instant,
String>` sorted by timer expiry, wake-sleep on the next expiry.

**Correctness note.** Longer debounce = larger VM-crash loss window.
Rules set per workload class:
- `-wal`, `.journal`, `.tmp` staging files: 500–1000ms (many rapid
  rewrites, last-one-wins is correct)
- `node_modules` internals: 250ms
- Source files, user data: 0 (no debounce — the current immediate-
  enqueue behavior)

fsync() honors in-flight debounce: flushes the timer, forces enqueue,
blocks until PUT completes.

---

## 4. Dynamic read-ahead

Replaces the current static `read_ahead_size` default + "whole file if
small, read-ahead chunks if large" heuristic.

**Per-fd state.**
```rust
struct ReadAheadState {
    last_offset: u64,
    last_size:   u32,
    streak:      u32,     // contiguous reads
    window:      usize,   // current prefetch size
}
```

On each `read(offset, size)`:
- If `offset == last_offset + last_size`: streak++, grow window
  (doubling, capped at `MAX_WINDOW = 256 MB`).
- If `offset != last_offset + last_size`: reset streak, shrink window
  (e.g. `window / 2`, floored at `MIN_WINDOW = 64 KB`).

Path rules (§1) become MODIFIERS, not the primary mechanism:
- `readahead = "off"`: skip read-ahead entirely (SQLite b-tree,
  random-access files)
- `readahead = "auto"` (default): dynamic detection as above
- `readahead = "aggressive"`: start with larger initial window, grow
  faster (archives, binaries, streaming reads)

**Obviates the static tuning.** The 128 MB default we just bumped in
13 becomes irrelevant — dynamic detection reaches that window naturally
on sequential reads and stays small on random-access files. The large-
file read-cache bypass from research/13 §5 is solved by this: a 99 MB
binary that's read sequentially (execve page-ins) hits the MAX_WINDOW
and is fetched in 1–2 GETs, not 234.

---

## 5. Pre-existing correctness + bootstrap queue

These items carried over from research/13 §5 and are sprint-scope:

- **Rename non-atomicity + throughput.** Dir-rename as recursive
  copy+delete is O(n) S3 ops. PARTIALLY addressed in 90b92ec
  (wait_for_all + obj.size from LIST fixed the cache-size corruption
  that was masquerading as an atomicity bug). The O(n) per-child
  COPY cost remains. S3-native rename approach + fault-injection
  test infrastructure still needed for the throughput angle and
  partial-failure correctness under concurrent writes.
- **Bootstrap rootfs cluster.** sources.list empty, gpgv purged,
  sqv/fido2/cbor purged (breaks apt solver 3.0 on trixie), DNS
  dhclient race with resolv.conf. One coherent fix to
  `dist/bootstrap-rootfs.sh` + `dhclient.conf` supersede.
- **setattr persistence.** research/07 §10c, still broken. chmod
  during install silently no-ops. Separate fix, interacts with write
  performance if batched.
- **`/etc/apt/sources.list` 0-bytes post-bootstrap** (vm:2.5
  2026-04-18). File exists in `/etc/apt/` but is empty. Not a symlink
  issue, not `sources.list.d/` shadowing. Something in the debootstrap
  invocation or the s3-rootfs upload pipeline is truncating it.
  Adds color to the bootstrap cluster above; may need its own
  investigation depending on where the truncation happens.
- **Duplicate VM launch on same macvtap** (vm:2.5 2026-04-18).
  Operational footgun: if `bootstrap.sh` is launched twice against
  the same macvtap (e.g. via `&` + tmux send-keys), two microvm
  processes share `/dev/tap<N>` and both fail silently at network
  level — DHCP yields no IPv4, pings return "Network is unreachable".
  Fix direction: detect tap ownership collision at launcher startup
  and refuse to start, OR document as "one VM per macvtap". Detection
  is cheap (flock on `/dev/tap<N>` or check `ip link`'s bound state)
  and prevents a confusing silent-failure mode.
- **Ephemeral RAM eviction policy** (surfaced during §2 landing
  2026-04-18). `WritePool::enqueue` for Ephemeral bypasses the PUT-
  transit budget, correctly — that budget is for writes waiting on
  S3, not for data whose storage IS the inflight map. But Ephemeral
  currently has NO ceiling: a rogue workload (pathological cache
  growth, runaway test, adversarial input) can accumulate
  unboundedly in host RAM. Need: separate Ephemeral budget +
  eviction policy (LRU by `submitted_at`? Path-class weights? Drop
  oldest-per-path first?). Not a correctness bug for any workload
  we've seen, but a host-stability bug waiting for the wrong
  workload. First-class follow-up, size unknown.
- **Ephemeral VM isolation (docs-only)** (2026-04-19, eae6934).
  `VM_EPHEMERAL_DIR` used as-is; commit documents per-VM-subdir
  convention but doesn't enforce it. `microvm/main.rs` already parses
  `VM_ID` — appending it to ephemeral_dir would prevent "user sets
  VM_ID expecting isolation, collision still happens". Latent
  footgun for orchestrator-hoist; not a correctness bug today.
  Two-line fix when orchestrator work starts.
- **Hardlink on ephemeral paths** (2026-04-19, deferred in 835f710).
  link() issues an S3 COPY to a `.hardlinks/<uuid>` ref body from
  the source key — which, for ephemeral paths, was never PUT, so
  COPY 404s. nvm/npm don't hardlink into their cache paths so this
  doesn't block any current workload. Fix when a workload needs it:
  for ephemeral sources, read the local disk file and write a
  `.hardlinks/<uuid>` file on local disk (or keep in inflight map
  as a special entry), same semantic as the durable hardlink ref
  but routed to the ephemeral storage tier.
- **Git+SSH dependency auth** (vm:2.5 2026-04-18). npm/cargo/pip
  packages with SSH git URLs (observed: openclaw's libsignal-node
  using `ssh://git@github.com/...`) fail inside the VM without SSH
  known_hosts + auth strategy. Not our bug, but relevant to "agents
  can install anything" ambitions. Directions: ship minimal SSH
  known_hosts (github.com, gitlab.com) in the rootfs + document a
  host-side credential-injection path (e.g. via a mounted auth
  socket or short-lived token). Research thread, not code work.

---

## 6. Deferred — considered, not this sprint

- **Re-enable kernel page cache as L1.** `auto_cache=true`,
  `writeback_cache=false`, attr/entry timeouts 30–60s, kernel
  satisfies hot reads in-guest without crossing virtio-fs. Directly
  inverts the TTL=0 fix from late-research/13; the right framing is
  TTL=0 is correct near-term, re-enable with active invalidation
  later. Needs §7 (invalidation plumbing) as prerequisite. Measurement
  story: expect ~10× reduction in FUSE op count reaching userspace,
  internal cache hit rate will LOOK worse because wins moved one layer
  up. Own design doc + own sprint.
- **FUSE invalidation plumbing.** `fuse_lowlevel_notify_inval_inode`
  / `inval_entry`. Enabler for kernel-cache-as-L1. Also useful for
  orchestrator-pushed cache updates (API upload path). Not needed in
  hot path given single-writer invariant. Defer until L1-cache work
  starts.
- **DAX shared read-only.** Two-mount model (system RO DAX-shared,
  scratch RW private). Ten VMs sharing one Node binary in host page
  memory. Conceptually overlaps with golden FS (also shares immutable
  bytes across VMs, via different mechanism). Needs its own design
  doc comparing DAX vs golden-blob-cache-hoisted-to-host. Research
  thread, not sprint work.

---

## 7. Implementation ordering (proposed)

1. **§1 Ruleset engine.** Infrastructure — nothing else lands without
   this. Start with TOML parse + glob trie compile + handle-carries-
   rules shape. Default ruleset = empty (all files get defaults).
2. **§2 Durability class routing.** Plumb Ephemeral / Local / Durable
   through `WritePool`. Ephemeral first (biggest PUT win, simplest to
   implement — just skip enqueue). Local = Durable+coalesce placeholder
   until §2a is justified.
3. **First ruleset consumer: coding-agent defaults.** Ship a default
   TOML that marks `~/.npm/_cacache`, `~/.cache/pip`, `node_modules/
   .cache`, `target/`, etc. as ephemeral. Measure PUT-volume drop on
   `npm install`, `cargo build`, `pip install` — same methodology as
   research/13's nvm bench.
4. **§3 Coalesce windows.** Debounce scheduler + ruleset wiring.
   Measure PUT-volume drop on workloads that rewrite the same files
   rapidly (git operations, editor saves, build systems writing
   intermediate files).
5. **§4 Dynamic read-ahead.** Per-fd streak detection. Independent
   of 1–4; can run in parallel to §2/§3 implementation if capacity.
   Measure GET-volume drop on binaries + GET-volume on SQLite-shaped
   workloads (should be lower than current 16 MB / 128 MB defaults).
6. **§5 Correctness + bootstrap queue.** Rename non-atomicity is the
   highest-impact item here. Bootstrap cluster is smaller surface but
   unblocks actual in-VM dev workflow.

Bench discipline: N=5 median+range for each headline change, same as
research/13. Don't push on benchmarking between steps — one run at
commit time per change.

---

## 8. Outcomes (sprint closed 2026-04-19)

Final bench at HEAD (835f710), workload = `nvm install 22 + npm install
-g typescript` (bounded, completable, ephemeral-write-heavy):

| Metric | Baseline (pre-sprint) | HEAD | Delta |
|---|---|---|---|
| PUT volume | 6478 | 579 | **-91%** |
| GET volume | 836 | 10 | **-99%** |
| Wall clock (typescript workload) | 4m 13s | 3m 33s | **-16%** |
| Wall clock (nvm binary alone) | 3m 39s | 1m 47s | **-51%** |

Correctness milestone: `nvm install 22 + npm install -g openclaw`
(previously could not complete under any configuration) now runs
through the entire nvm + npm dep tree until it hits openclaw's own
`ssh://git@github.com/...` dependency — a 3rd-party repo-auth issue,
not our filesystem. Every FS correctness bug that previously gated
this workload is resolved.

Items from §5 resolved this sprint:

- Setattr persistence (research/07 §10c) — fixed in 40b5e3c,
  corrected in 2db774b. test_permissions 4/5 → 5/5 after 4 sprints.
- Rename throughput — parallel COPY+DELETE (b7b9e5e) 17s → 9.5s on
  500-file test. S3-native batch approach still open for larger wins.
- Bootstrap rootfs cluster (cb0e301) — apt update + install + external
  curl work end-to-end.
- Duplicate VM launch on same macvtap — tap flock detection (e70cb56).
- Ephemeral RAM eviction — obsoleted by disk migration (5b952f6):
  ephemeral writes now go to local disk (`VM_EPHEMERAL_DIR`), no RAM
  ceiling, no eviction, no data loss under budget pressure.
- Large-file read-cache bypass — solved by §4 dynamic read-ahead
  (6f1069e); window grows naturally on sequential reads.
- Write-then-read visibility — resolved during sprint-13 close by
  TTL=0 (90b92ec); confirmed not a regression in sprint-14.
- Sources.list 0-bytes — folded into bootstrap cluster fix.

Items remaining in §5 for a follow-up sprint:

- S3-native dir-rename batch (true O(1) instead of parallelized O(n)).
- Git+SSH dependency auth (host-side credential injection).
- VM_ID-based ephemeral_dir enforcement (two-line fix when
  orchestrator hoists to host scope).
- Hardlink on ephemeral paths (no current workload exercises it).
- IPv6 link-local without gateway (curl -4 workaround).
