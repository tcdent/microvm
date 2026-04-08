# S3 Filesystem Design — Key Format, Caching, and Request Budget

How we map POSIX filesystem semantics onto S3 objects. Covers the key
format, lookup strategy, caching architecture, and the full FUSE→S3
request map. Companion to `03-filesystem-architecture.md` (which covers
the high-level why); this document covers the detailed how.

---

## The Problem: S3 Has No Directories or Metadata

S3 is a flat key-value store. It doesn't know what a directory is, what
permissions are, or what a symlink is. But the guest kernel expects all
of these from its root filesystem.

Every S3 request is an HTTP round-trip — TCP, TLS (if not local), request
signing, serialization, response parsing. Even against local MinIO, each
request is on the order of 1-5ms. The guest experiences this as filesystem
latency, so every redundant request is directly felt.

The design goal: **minimize S3 round-trips per filesystem operation** while
keeping the implementation simple and correct.

---

## Key Format: `path\x01<mode_hex><uid_hex><gid_hex>`

### Motivation

The original approach used `x-amz-meta-*` headers for POSIX metadata
(mode, uid, gid, symlink target). This required HEAD requests to discover
metadata — LIST only returns keys and sizes, not custom headers. A single
directory lookup cost 3 requests (HEAD + LIST + HEAD) because we had to
guess whether a path was a file or directory.

Worse, LIST can't identify symlinks without metadata. The kernel needs
`S_IFLNK` in `st_mode` at lookup time — if we return `S_IFREG`, it
never calls `readlink`. A typical rootfs has hundreds of symlinks.
We can't defer this; we need the type at lookup time.

The fix: embed metadata directly in S3 keys where LIST can see it.

### Format

```
<path>\x01<mode_hex><uid_hex><gid_hex>
```

- `\x01` (SOH, 0x01) separates the filesystem path from metadata
- All fields are fixed-width lowercase hex, no delimiters between them
- Total metadata: 20 bytes (4 mode + 8 uid + 8 gid), always

| Field | Size | Bits | Range |
|-------|------|------|-------|
| mode | 4 hex chars | 16-bit | Full st_mode (type + perms) |
| uid | 8 hex chars | 32-bit | 0 – 4294967295 |
| gid | 8 hex chars | 32-bit | 0 – 4294967295 |

The mode encodes the file type in its high bits per POSIX:
- `0x81ed` = `0o100755` = S_IFREG | 0755 (regular file)
- `0xa1ff` = `0o120777` = S_IFLNK | 0777 (symlink)
- `0x41ed` = `0o040755` = S_IFDIR | 0755 (directory)

### Why \x01

`\x01` (SOH) is the separator between path and metadata because:
- Valid in S3 keys
- Technically valid in Linux filenames but never occurs in practice
  (no tool, editor, or build system generates it in file names)
- Lowest non-null byte value, which gives clean sort behavior

### No trailing slash on directories

Directories do NOT use a trailing `/` in their key. The mode field
already encodes the type. A directory named `usr/bin` is stored as:

```
usr/bin\x0141ed00000000000000000000
```

Not `usr/bin/\x01...`. This is critical — it means files and directories
share the same key pattern (`path\x01<meta>`), which enables single-LIST
lookup regardless of type.

### Symlink targets

Symlink targets are stored as the **object body** (the object's content),
not in the key or in headers. This mirrors real filesystems — on ext4, a
symlink's data content is the target path.

- Keeps the key format uniform: always `path\x01<20 hex chars>`
- No variable-length data in keys, no secondary delimiters to parse
- No 1024-byte key length concerns from long targets
- `readlink` does a GET to fetch the target (one request, then cached)

### Example keyspace

```
etc\x0141ed00000000000000000000          ← directory
etc/hosts\x0181a000000000000000000000    ← regular file
etc/motd\x0181a400000000000000000000     ← regular file
etc/shadow\x0181a0000000000000002a       ← root:shadow
usr\x0141ed00000000000000000000          ← directory
usr/bin\x0141ed00000000000000000000      ← directory
usr/bin/bash\x0181ed00000000000000000000 ← regular file
usr/bin/ls\x0181ed00000000000000000000   ← regular file
usr/lib\x0141ed00000000000000000000      ← directory
usr/lib/libfoo.so\x01a1ff00000000000000000000  ← symlink
```

### Implementation

**`keys.rs`**: `encode`/`decode`/`list_prefix` for the `path\x01<20 hex>`
format. Includes test for sort-order property (directory key < child key).

### Open questions

1. **MinIO key handling.** Do `\x01` bytes in keys work correctly with
   MinIO's S3 API, web console, and `mc` CLI? Valid per S3 spec but
   might confuse tooling. Need to test.

2. **Key readability.** Keys become opaque: `usr/bin/bash\x0181ed...`
   instead of clean paths. Acceptable for machine-managed rootfs. For
   debugging, the packer could include a `list-decoded` command that
   prints paths with human-readable metadata.

---

## Sort Order and Pre-fill

### Why this works with sort order

S3's lexicographic sort on our key format produces a natural ordering:

```
\x01 = 0x01  (metadata separator)
/    = 0x2F  (path separator)
0-9  = 0x30-0x39
a-z  = 0x61-0x7A
```

`\x01` sorts before `/`, which sorts before all alphanumeric characters.
This means for any path prefix like `usr/bin`:

```
usr/bin\x01<meta>           ← the directory entry itself
usr/bin/bash\x01<meta>      ← child (/ comes after \x01)
usr/bin/cat\x01<meta>       ← child
usr/bin/subdir\x01<meta>    ← child directory
usr/bin/subdir/tool\x01...  ← grandchild
```

The directory entry sorts immediately before its children. A LIST
starting at `usr/bin\x01` naturally flows into the directory's
contents. This is a consequence of `\x01 < /` — the metadata suffix
on the directory entry sorts before the path separator that begins
child paths.

If a sibling file exists (rare but possible):

```
usr/bi\x01<meta>            ← hypothetical file "usr/bi"
usr/bin\x01<meta>           ← directory "usr/bin"
usr/bin/bash\x01<meta>      ← child of usr/bin
usr/binder\x01<meta>        ← hypothetical file "usr/binder"
```

The targeted prefix `usr/bin\x01` skips straight to our entry and its
children. `usr/bi` and `usr/binder` don't match because the bytes
after `bin` differ. No ambiguity.

### Opportunistic pre-fill

Pre-fill happens at the **parent directory** level, not the file level.
When the kernel looks up a child name inside a directory, we LIST the
parent directory with a small cap instead of doing a precise single-key
probe:

```
LIST prefix=usr/bin/&max-keys=12
```

No `\x01`, no child name in the prefix — just the parent path + `/`.
S3 returns up to 12 keys starting with `usr/bin/`:

```
usr/bin/bash\x0181ed...    ← maybe the one we need
usr/bin/cat\x0181ed...     ← free
usr/bin/chmod\x0181ed...   ← free
usr/bin/cp\x0181ed...      ← free
usr/bin/dash\x0181ed...    ← free
...up to 12 total
```

We scan the results for the specific child we're looking for. If it's
there, we have our answer. Either way, we cache everything we got —
those siblings are exactly the entries the kernel is likely to ask about
next (think `$PATH` resolution, `ls`, tab completion).

**Why `max-keys=12` instead of 1?** The round-trip latency dominates
the cost of a LIST — returning 12 results vs 1 adds negligible overhead
(a few hundred extra bytes of XML on the wire). By asking for 12, we
get the entry we're looking for plus up to 11 siblings for free.

**If the child isn't in the first 12 results** (large directory, child
sorts late alphabetically), we fall back to a precise probe:

```
LIST prefix=usr/bin/zsh\x01&max-keys=1
```

This always returns exactly the right entry. The pre-fill was a
speculative optimization that pays off in narrow directories (which
are the common case for system paths) and misses gracefully in wide
directories.

**Example: looking up "lib" inside "usr/":**

```
LIST prefix=usr/&max-keys=12
```

Returns:
```
usr/bin\x0141ed...        ← child directory, cached
usr/include\x0141ed...    ← child directory, cached
usr/lib\x0141ed...        ← the one we wanted, cached
usr/local\x0141ed...      ← child directory, cached
usr/sbin\x0141ed...       ← child directory, cached
usr/share\x0141ed...      ← child directory, cached
```

`/usr` has ~6-10 children. All of them fit in 12. The next time the
kernel asks about `share` or `local` or `sbin`, they're already cached.
One LIST pre-filled the entire directory.

The depth of pre-fill depends on the fan-out. A narrow directory like
`/usr` (8-10 children) gets entirely pre-filled. A wide directory
like `/usr/bin` (hundreds of entries) gets the first ~11 entries,
which still helps for alphabetically early entries. The rest fall
through to precise probes.

### max-keys tuning

12 is a starting point (`LIST_PROBE_MAX_KEYS` in `s3fs.rs`). Could be
tuned based on typical directory fan-out. Too low wastes the pre-fill
opportunity. Too high wastes bandwidth on entries that are never accessed.
The sweet spot is roughly the width of a typical shallow system directory
(`/usr` has ~8-10 entries, `/etc` has ~20-30). 12 covers the common
narrow directories fully and gives partial coverage of wider ones.

---

## FUSE and the Serial Lookup Problem

The kernel's FUSE interface resolves paths one component at a time.
To access `/usr/bin/bash`, it calls `lookup("usr")`, waits for the
result, then `lookup("bin")`, waits, then `lookup("bash")`. Each
call is serial — the kernel needs the parent inode before it can ask
about the child. We can't change this; it's how Linux VFS works.

But with pre-fill, each lookup caches siblings for the next steps:

1. `lookup("usr")` in root → `LIST prefix=&max-keys=12` (root children)
   Returns: `bin` (symlink), `etc` (dir), `lib` (symlink), `sbin` (symlink),
   `usr` (dir), `var` (dir), ...
   Cache now has: bin, etc, lib, sbin, usr, var — the top-level dirs.
   Found "usr" in results. 1 LIST.

2. `lookup("bin")` in usr → `LIST prefix=usr/&max-keys=12`
   Returns: `usr/bin` (dir), `usr/include` (dir), `usr/lib` (dir),
   `usr/local` (dir), `usr/sbin` (dir), `usr/share` (dir)
   Cache now has all children of `/usr`. Found "bin". 1 LIST.

3. `lookup("bash")` in usr/bin → `LIST prefix=usr/bin/&max-keys=12`
   Returns: `usr/bin/bash`, `usr/bin/cat`, `usr/bin/chmod`, ...
   Cache now has first 12 entries of `/usr/bin`. Found "bash". 1 LIST.

A 3-component path traversal costs 3 LISTs, but each one pre-fills
siblings at that level. The next path traversal through the same tree
(e.g., `/usr/lib/libfoo.so`) would cost:

1. `lookup("usr")` → **cache hit** (from step 1). 0 requests.
2. `lookup("lib")` → **cache hit** (from step 2). 0 requests.
3. `lookup("libfoo.so")` → 1 LIST (new directory). Caches 12 siblings.

Second traversal: 1 LIST instead of 3. The first traversal pays full
price but warms the tree for everything that follows.

This works especially well for the shallow, narrow directories at the
top of the tree (`/usr`, `/etc`, `/var`) which typically have fewer than
12 direct children — a single LIST caches the entire level.

For wider directories like `/usr/bin` (hundreds of entries), pre-fill
covers the first ~12 entries alphabetically. The rest fall through to
precise probes when accessed.

### The readdir→lookup storm

When the guest runs `ls -l /usr/bin/`, the kernel:
1. Calls `readdir` on `/usr/bin/` → we do 1 LIST, return all entries
2. Calls `lookup` on every entry name → N lookups

`readdir` already told us every entry's key and size. We populate the
inode cache during `readdir`, so all subsequent lookups are cache hits.
The readdir→lookup storm goes from N×3 requests to 0.

### Future: kernel FUSE patching

We control the guest kernel. A custom FUSE extension that passes the
full remaining path (or batches multiple components) would let us
collapse the serial lookup chain into a single S3 call. Not needed
today, but architecturally we should keep the FUSE layer clean enough
that swapping in a patched protocol is straightforward.

---

## Directory Listing (readdir)

```
LIST prefix=usr/bin/&delimiter=/
```

S3 returns all keys starting with `usr/bin/`. The `/` delimiter groups
grandchildren into CommonPrefixes. Direct children appear in Contents
with full metadata:

```
Contents:
  usr/bin/bash\x0181ed00000000000000000000
  usr/bin/ls\x0181ed00000000000000000000
  usr/bin/python3\x01a1ff00000000000000000000
  usr/bin/subdir\x0141ed00000000000000000000
CommonPrefixes:
  usr/bin/subdir/   ← because subdir has children under it
```

Subdirectories like `subdir` appear in both Contents (with metadata,
because their key `usr/bin/subdir\x01<meta>` starts with the prefix)
and CommonPrefixes (because keys like `usr/bin/subdir/tool\x01...`
exist). We read metadata from Contents and ignore the CommonPrefixes
duplicate.

`readdir` parses every result and populates the inode cache. Subsequent
`lookup` calls for any child of this directory are served from cache
with zero S3 traffic.

---

## Cache Architecture

### InodeCache (`cache.rs`)

Bidirectional `InodeCache` — `HashMap<String, CachedInode>` keyed by
path, plus `HashMap<u64, String>` for reverse inode→path lookup, plus
`HashSet<String>` for negative entries.

Populated by LIST results via `insert_from_key()`. Preserves inode
numbers and symlink targets on update. Root inode pre-initialized.

### Design principles

**Our inode table is the primary cache, not the kernel's dentry cache.**

**Kernel TTL (`entry_timeout`)**: kept short. The kernel re-lookups
frequently, but our cache serves instantly with no S3 traffic. This
gives us control over cache lifetime rather than relying on the
kernel's timer.

**Our cache is authoritative for local writes.** When the guest creates,
modifies, or deletes a file, we update the inode table directly. No
staleness between write and subsequent read.

**Host-side writes invalidate the cache directly.** The VMM owns both
the S3 client and the FUSE server in the same process. When the host
pushes a file to S3 (e.g., injecting a config file into the running
VM), it can update the inode table in the same process. The guest sees
the change on the next lookup — no TTL expiry needed, no staleness
window. This is a unique advantage of owning the full stack.

**Negative caching.** Lookup misses (ENOENT) are cached in a `HashSet`.
The kernel probes many non-existent paths (library search paths, locale
variants, optional config files). Without negative caching, each probe
is a wasted LIST. With it, the first miss is cached and subsequent
probes are free. Positive inserts evict negative entries.

### Cache consistency

Serving from cache means we might return stale data. Key scenarios:

1. **We're the only writer** (coding agent VM): cache is always correct.
   Files only change when the guest writes them, which updates our cache.
   Safe to cache aggressively.

2. **External writer** (someone pushes to S3 outside the VM): our cache
   is stale. The guest sees old files until cache expires or VM restarts.

For our use case (1), aggressive caching is safe. The `entry_timeout` TTL
(currently 1 second) controls how often the kernel re-lookups. We could
increase this significantly, but even at 1 second, if our inode cache
serves the re-lookup without hitting S3, the TTL just controls kernel-side
dentry expiry — not S3 traffic.

**One caveat**: if we ever support a workflow where the rootfs S3 bucket
is modified externally while the VM is running, we'd need either:
- A cache invalidation mechanism (e.g., S3 event notifications)
- A way to force a cache flush (e.g., guest-initiated `sync` or remount)
- Accept staleness within a bounded window

For now, single-writer assumption holds. Document it and move on.

### Data flow: lookup always precedes data access

A critical property of the VFS interface: the kernel never asks us to
`read`, `open`, or `readlink` a file it hasn't already resolved via
`lookup`. Every data access is preceded by a lookup that established the
inode. This means:

1. `lookup` discovers the S3 key (via LIST) and caches it in the inode
   table — including the full key with metadata suffix.
2. `open` / `read` / `readlink` use the **already-known key** from cache
   to issue a direct GET. No additional LIST is needed.
3. `getattr` / `stat` serve entirely from cache — the metadata was
   parsed from the key during lookup. Zero S3 traffic.

The only FUSE operations that hit S3 without a prior lookup are writes
(`create`, `mkdir`) — these construct new keys (we choose the metadata)
and PUT directly. But writes generate keys; they don't need to discover
them.

This means **LIST is purely a discovery mechanism** and **GET is purely
a data fetch**. They never overlap. Every file interaction follows:

```
LIST (discovery, once) → cache → GET (data, when needed)
```

---

## S3 Request Budget

### Historical context

The original implementation used HEAD for metadata discovery:

| FUSE op | S3 requests | Calls | Notes |
|---------|-------------|-------|-------|
| `lookup` (file hit) | 1 | HEAD | Best case — file exists |
| `lookup` (dir hit, has children) | 3 | HEAD + LIST + HEAD | **Redundant** |
| `lookup` (dir hit, empty dir) | 3 | HEAD + LIST + HEAD | |
| `lookup` (miss) | 2 | HEAD + LIST | File HEAD 404, LIST empty |
| `getattr` | 0 | — | Serves from inode cache |
| `readdir` | 1 | LIST | Single list with delimiter |
| `read` | 1 | GET (Range) | Single range request |
| `open` (read-only) | 0 | — | Just creates handle |
| `open` (write) | 1 | GET | Preloads existing content |
| `write` | 0 | — | Buffered in memory |
| `release` (dirty) | 1 | PUT | Flushes buffer to S3 |
| `release` (clean) | 0 | — | No-op |
| `create` | 1 | PUT | Empty object |
| `mkdir` | 1 | PUT | Empty marker object |
| `unlink` | 1 | DELETE | Single delete |
| `setattr` | 0 | — | Stub, inode cache only |
| `readlink` | 0 | — | Serves from inode cache |

The fundamental issue: S3 has no concept of directories. We didn't know
if `usr` was a file or a directory because they're different keys (`usr`
vs `usr/`). So we tried one, failed, tried the other.

### Current request map (metadata-in-keys)

Every FUSE call and exactly which S3 calls it makes:

| FUSE op | S3 calls | What hits S3 | What's cached |
|---------|----------|--------------|---------------|
| `lookup` (cold) | 1 LIST | key discovery | type, mode, uid, gid, full key |
| `lookup` (warm) | 0 | — | served from inode table |
| `lookup` (pre-filled) | 0 | — | populated by prior lookup's max-keys=12 |
| `lookup` (miss) | 1 LIST | confirms non-existence | negative result cached |
| `getattr` | 0 | — | metadata parsed from key at lookup time |
| `readdir` | 1 LIST | enumerate children | all children + metadata cached |
| `open` (read) | 0 | — | just allocates a handle |
| `open` (write, no trunc) | 1 GET | preload content for read-modify-write | — |
| `open` (write, O_TRUNC) | 0 | — | skip preload, start empty |
| `read` | 1 GET | fetch data using cached key | — |
| `readlink` (cold) | 1 GET | symlink target from object body | target cached |
| `readlink` (warm) | 0 | — | target served from cache |
| `write` | 0 | — | buffered in memory |
| `release` (dirty) | 1 PUT | flush buffer to S3 | updates inode table |
| `release` (clean) | 0 | — | — |
| `create` | 1 PUT | new key (we choose metadata) | updates inode table |
| `mkdir` | 1 PUT | new key (we choose metadata) | updates inode table |
| `unlink` | 1 DELETE | remove key | evicts from inode table |
| `setattr` | 0 | — | updates inode table (persisted on next write) |

**HEAD is never used.** All metadata comes from key parsing at LIST time.

**GET is only used for data**: file content (`read`, `open` preload) and
symlink targets (`readlink`). Never for metadata discovery.

**LIST is only used for discovery**: lookup and readdir. Never for data.

**PUT/DELETE are only used for writes.** Always exactly 1 request.

### Key wins vs original

| FUSE op | Before | After (cold) | After (warm) |
|---------|--------|--------------|--------------|
| lookup (any type) | 1-3 | 1 LIST (max-keys=12) | 0 (cache) |
| lookup (pre-filled by prior) | 1-3 | 0 | 0 (cache) |
| lookup (miss) | 2 | 1 LIST | 0 (neg. cache) |
| readdir | 1 LIST | 1 LIST | 1 LIST |
| getattr | 0 | 0 | 0 |
| read | 1 GET | 1 GET | 1 GET |
| readlink | 0 | 1 GET (first) | 0 (cache) |
| open (write, no trunc) | 1 GET | 1 GET | 1 GET |
| open (write, O_TRUNC) | 1 GET | 0 | 0 |
| release (dirty) | 1 PUT | 1 PUT | 1 PUT |
| create | 1 PUT | 1 PUT | 1 PUT |
| mkdir | 1 PUT | 1 PUT | 1 PUT |
| unlink | 1 DELETE | 1 DELETE | 1 DELETE |

- **Every lookup: 1 LIST, always** — no type guessing, no HEAD
- **Directory lookup: 3 → 1**
- **Pre-fill: next ~11 entries cached for free** per lookup
- **Path traversal `/usr/bin/bash`: 1 LIST** instead of 3 (pre-fill)
- **Symlink identification: free from key metadata**
- **readdir→lookup storm: N×3 → 0** (readdir warms entire directory)
- **Host-side writes: instant visibility** via direct cache update
- **Data access (read/readlink) never needs LIST** — key already known

---

## S3 Client (`s3.rs`)

`list_probe()` — single-page LIST with configurable max-keys, no
pagination. URL-decodes keys from S3's XML response (handles the
percent-encoded `\x01` byte correctly — see `06-s3-percent-encoding-bug.md`).

`list()` — paginated LIST with `NextContinuationToken` loop. Used by
`readdir` (which may have large result sets).

### LIST vs HEAD latency

The whole strategy assumes LIST isn't dramatically slower than HEAD for
small result sets. Expectation: the round-trip dominates and payload
size is negligible. Not yet benchmarked formally, but boot time improved
from 1min 44s to 1min 39s after the switch, which validates the approach.

---

## Rootfs Packing

The upload tool (`dist/s3-upload/`) encodes keys in the new format:

1. Walk the source filesystem
2. For each entry, construct: `path\x01<mode_hex><uid_hex><gid_hex>`
   - No trailing `/` on directories
3. Upload with content:
   - Regular files: file content as body
   - Symlinks: target path as body
   - Directories: empty body (0 bytes)

The packer replaces `x-amz-meta-*` headers entirely — all metadata
is in the key. PUT requests become simpler (no custom headers).

`readdir` strips the `\x01...` suffix when returning names to the
kernel. All other operations parse the fixed 20-byte hex suffix.

---

## Implementation Status

### Implemented

| Feature | Module | Notes |
|---------|--------|-------|
| Metadata-in-keys format | `keys.rs` | encode/decode/list_prefix |
| Bidirectional inode cache | `cache.rs` | path↔inode, negative entries |
| Probe-and-cache lookup | `s3fs.rs` | LIST with max-keys=12, pre-fill neighbors |
| Negative caching | `cache.rs`, `s3fs.rs` | HashSet, evicted on positive insert |
| readdir→lookup pre-fill | `s3fs.rs` | readdir populates cache for all children |
| readdir with delimiter | `s3fs.rs` | `delimiter='/'` lets S3 filter children |
| O_TRUNC skip preload | `s3fs.rs` | skip GET when truncating on open |
| Write buffering | `s3fs.rs` | per-handle buffer, flushed on release |
| Symlink target caching | `cache.rs` | populated on first readlink |
| Cache observability | `s3fs.rs`, `cache.rs` | `CacheStats` with hit/miss/negative_hit counters |
| Read caching | `s3fs.rs` | configurable via `with_read_cache()`, default 2MB |
| Dir-level prefill | `s3fs.rs` | LIST parent dir with delimiter, fallback to precise probe |
| IsTruncated handling | `s3.rs`, `s3fs.rs` | use S3's flag, not object count heuristic |
| Integration test harness | `tests/` | 43 tests across 5 modules against live RustFS |

### Read caching — implemented, no boot time impact

Read caching is implemented: on first `read()`, files ≤ `read_cache_max`
(default 2MB, configurable, 0 = disabled) are fetched in full via a single
GET and cached in the file handle. Subsequent reads serve from the buffer.

This eliminates ~12 sequential GETs per shared library load (e.g., libc.so.6
at 1.7MB was 12 range GETs, now 1 full GET). However, **boot time testing
showed no measurable improvement**:

| read_cache_max | Boot time |
|----------------|-----------|
| 0 (disabled) | 1min 39.6s |
| 2MB (default) | ~1min 39s |
| 5MB | 1min 38.3s |

**Why:** The FUSE worker runs on a dedicated thread (via ioeventfd). S3
I/O was already overlapped with vCPU execution. The vCPU is CPU-bound
during boot (systemd service startup, process spawning), not waiting on
filesystem I/O. Reducing S3 round-trips saves network bandwidth but
doesn't unblock the critical path.

The read cache is still worth keeping — it reduces S3 request volume and
will matter for I/O-bound workloads (e.g., `grep -r`, compilation). But
further boot time optimization needs to target the CPU side: reducing
systemd services, increasing kernel dentry TTL, or batching FUSE lookups.

### Remaining Gaps

**No `rename`.**
This is the most impactful missing FUSE operation for a coding agent
workload:

- `mv file.txt new.txt` — fails
- `sed -i 's/foo/bar/' file.txt` — fails (writes tempfile, renames)
- Every text editor (vim, nano, emacs) — atomic save via write-tmp + rename
- `git checkout` — renames files during branch switch
- `pip install` — renames during package installation

S3 has no rename primitive. Implementation requires copy + delete:

```
1. GET old key (or skip if we have cached data)
2. PUT new key (path\x01<meta>) with same body
3. DELETE old key
4. Update cache: remove old path, insert new path with same inode
```

For directories, this requires recursively renaming all children (LIST
prefix, copy+delete each). This is expensive but necessary for
correctness.

**Priority:** High. Without rename, the filesystem is effectively
read-only for most real-world tools.

#### Medium impact

**`setattr` doesn't persist.**
`setattr` returns success but only updates the cache. The metadata change
is never written to S3. This means `chmod`, `chown`, `utimes` appear to
succeed but are lost on next cold lookup.

For a coding agent VM where everything is root:root 0755, this may not
matter in practice. But package managers (`apt`, `pip`) and build systems
(`make`) do check and set permissions.

**Fix:** On `setattr`, if mode/uid/gid changed, we need to rename the S3
key (since metadata is encoded in the key). This is a copy + delete in
S3 — no atomic rename:

```
1. PUT new key (path\x01<new_meta>) with existing object body
2. DELETE old key (path\x01<old_meta>)
3. Update cache
```

This is expensive (GET + PUT + DELETE = 3 requests) so it should be
deferred or batched. One option: mark the inode as "meta-dirty" and
persist on `release` or `fsync`, similar to how data writes are deferred.

#### Low impact

**No cache invalidation or TTL.**
The `InodeCache` has no eviction. Once an inode is inserted, it lives
forever. Acceptable under the single-writer assumption. Two scenarios
to consider eventually:

1. **Host-side file injection.** The VMM can update the cache directly
   since it owns the FUSE server — no TTL needed. This path doesn't
   exist yet but is architecturally straightforward.

2. **Long-running VMs.** Memory grows monotonically as inodes are cached.
   A `find` of the entire filesystem would cache every inode. An LRU or
   size-bounded cache would cap this, but it's not urgent for 256MB guest
   RAM VMs where the inode table is small relative to guest memory.

**Write buffer is unbounded.**
`write()` grows `HandleEntry.data` without limit. A guest writing a large
file will grow the VMM's heap until OOM.

**Fix:** Check `data.len()` against a configured max (e.g., 100MB) and
return `EFBIG` if exceeded. One-liner in `write()`.

**`readdir` loads entire directory before yielding.**
`readdir` calls `list()` which paginates internally and returns all
results, then iterates to yield entries. For a directory with 10K files,
this fetches and parses all 10K keys before returning the first `DirEntry`.

For typical system directories (10-100 entries) this is fine. For large
directories (`node_modules/`, `.git/objects/`) it could be a latency
spike on first `ls`. Not urgent but worth noting.

### Summary table

| Feature | Design | Implementation | Gap |
|---------|--------|----------------|-----|
| Metadata-in-keys | Full | Full | None |
| Probe-and-cache (LIST) | max-keys=12 parent prefix | Implemented (parent dir + fallback) | None |
| Negative caching | Designed | Implemented | None |
| readdir→lookup pre-fill | Designed | Implemented | None |
| readdir with delimiter | Designed | Implemented | None |
| O_TRUNC skip preload | Designed | Implemented | None |
| Read caching | Designed | Implemented (configurable, default 2MB) | No boot impact (CPU-bound) |
| Cache eviction/TTL | Deferred | Not implemented | Low (single-writer) |
| setattr persistence | Not designed | Not implemented | Medium |
| Write buffer bounds | 100MB limit noted | Unbounded | Low risk, easy fix |
| rename | Not designed | Not implemented | **High impact** |
| readdir streaming | Not designed | Loads all first | Low |

---

## Measured Cache Performance

### Test harness

Integration tests in `s3-rootfs/tests/` exercise FUSE operations against
a live RustFS (S3-compatible) backend. The `InodeCache` tracks hit/miss
counters on `get()` and `is_negative()`. Tests seed S3 data directly via
`S3Client`, then call `FileSystem` trait methods on `S3Fs`.

Test modules: `test_read_ops`, `test_write_ops`, `test_readdir`,
`test_permissions`, `test_cache`. See `tests/harness/` for fixtures.

### Realistic workload: readdir + repeated lookup

Workload: seed 8 files in `src/`, 1 file in `docs/`, then readdir `src/`,
lookup each child twice, probe 3 nonexistent paths twice.

```
Phase                    hits  misses  neg_hits  hit_rate
after readdir src/          0       2         0     0.0%
after 8 child lookups       8       2         0    80.0%
after 8 repeat lookups     16       2         0    88.9%
after 3 misses             16       5         0    76.2%
after 3 repeat misses      16       8         3    66.7%
```

**Overall: 66.7% cache hit rate, 3 negative cache hits.**

### What's working

- **readdir→lookup pre-fill works.** All 8 child lookups after `readdir`
  on `src/` were cache hits. This is the most important optimization — it
  eliminates the N×requests storm when `ls -l` or a build system stats
  every file in a directory.

- **Repeat lookups are 100% cached.** The second pass of 8 lookups added
  8 hits and 0 misses. Once an inode is in cache, it stays there.

- **Negative caching works.** 3 lookups for nonexistent paths produced
  3 negative cache entries. Repeat lookups for the same missing paths
  hit the negative cache (3 negative_hits), avoiding 3 LIST round-trips.

- **All fundamental FUSE operations pass.** lookup, getattr, read,
  readlink, readdir, create, write, release, mkdir, unlink — 40 tests
  pass, confirming end-to-end correctness against a real S3 backend.

### What's broken: LIST pre-fill not caching neighbors

The `list_probe(prefix, max_keys=12)` call returns only 1 result, not 12.
The pre-fill design assumed `max-keys=12` would return the target entry
plus 11 subsequent keys from the sorted keyspace. But that's not how S3
LIST works — it only returns keys that **match the prefix**. The prefix
is too precise.

**Root cause:** `list_prefix("usr/bin/bash")` returns `usr/bin/bash\x01`.
S3 LIST with `prefix=usr/bin/bash\x01` only matches keys starting with
those exact bytes. The only key that starts with `usr/bin/bash\x01` is
`usr/bin/bash\x01<meta>` itself. Neighbors like `usr/bin/cat\x01<meta>`
start with `usr/bin/cat\x01`, not `usr/bin/bash\x01`. So `max-keys=12`
gets at most 1 result.

S3 LIST is prefix-filtered — it only returns keys matching the prefix,
not "start reading at this position." The pre-fill design relies on
listing the **parent directory** (`usr/bin/`) to get siblings, not the
precise key.

**Test evidence:** Seeding 10 files `file_00` through `file_09` under
one directory, then looking up `file_00`, results in only 1 new cache
entry — not the expected ~10. The `lookup_prefills_neighbors` and
`list_probe_max_keys_boundary` tests both fail.

**Fix:** `probe_and_cache` should use the parent directory prefix, not
the child path prefix:

```
LIST prefix=usr/bin/&max-keys=12    ← parent prefix, returns siblings
```

Instead of:

```
LIST prefix=usr/bin/bash\x01&max-keys=12  ← too precise, 1 result
```

This returns up to 12 keys from the parent directory — the target and
its siblings. Scan results for the exact child path. Cache everything.

If the child isn't found in the first 12 results (wide directory, child
sorts late alphabetically), fall back to the precise probe:

```
LIST prefix=usr/bin/bash\x01&max-keys=1   ← precise fallback
```

The grandchildren issue: `prefix=usr/bin/` also matches keys like
`usr/bin/subdir/tool\x01...`. Without a delimiter, these eat pre-fill
slots. But they're still useful cache entries if the kernel walks
deeper, and the 12-key cap limits waste. Alternatively, add
`delimiter=/` and read siblings from Contents only.

**Expected improvement when fixed:** The realistic workload above would
see the initial `lookup("src")` pre-fill `src/file_00` through
`src/file_07` (the first 8 lexicographic neighbors within max-keys=12).
The subsequent 8 child lookups would be cache hits even without readdir.
Hit rate for the same workload would rise from 66.7% to ~85-90%.

### Test failures (expected)

| Test | Status | Why |
|------|--------|-----|
| `setattr_mode_change_persists_to_s3` | FAIL | setattr is a no-op, doesn't persist to S3 |
| `lookup_prefills_neighbors` | FAIL | Pre-fill only caches 1 entry, not neighbors |
| `list_probe_max_keys_boundary` | FAIL | Same root cause as above |

These are intentional failures — the tests assert correct/designed
behavior, not current behavior. They'll pass when the underlying issues
are fixed.
