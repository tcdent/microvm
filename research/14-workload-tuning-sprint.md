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
  copy+delete is O(n) S3 ops. Observed live blocking nvm install
  (2026-04-17). S3-native rename approach + fault-injection test
  infrastructure. Promoted from "correctness concern" to
  "throughput bottleneck" this sprint.
- **Bootstrap rootfs cluster.** sources.list empty, gpgv purged,
  sqv/fido2/cbor purged (breaks apt solver 3.0 on trixie), DNS
  dhclient race with resolv.conf. One coherent fix to
  `dist/bootstrap-rootfs.sh` + `dhclient.conf` supersede.
- **setattr persistence.** research/07 §10c, still broken. chmod
  during install silently no-ops. Separate fix, interacts with write
  performance if batched.

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

## 8. Success criteria

- **`npm install` wall clock** on a medium project: further reduction
  vs research/13 baseline (which was already -19.6% vs pre-sprint).
- **PUT volume** on the same workload: substantial reduction from
  ephemeral path filtering.
- **`cargo build` first-build wall clock**: measure and establish
  baseline if not already captured.
- **`git clone` + operations** inside VM: no longer broken by rename
  non-atomicity on real-world repos.
- **apt install** inside a running VM: works end-to-end without manual
  sources.list / gpgv workarounds.

Details TBD once §1+§2 land and we see concrete numbers.
