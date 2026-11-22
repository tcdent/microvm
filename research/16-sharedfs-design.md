# sharedfs — host-owned project filesystem for guest VMs

Successor to research/15. The S3 root filesystem is retired; what replaces
it is narrower. A project's working tree lives on the host as a plain
directory. The guest sees it as a native kernel filesystem whose metadata
is read directly from host memory and whose mutations are executed by one
host daemon. Nothing is translated, nothing is cached twice.

`sharedfs` is a working name. Rename freely; nothing depends on it.

---

## 1. Why this shape

The April work established that a Linux system touches tens of thousands
of small files to do anything, so remote storage cannot sit in the hot
path. The requirements that survived the redesign:

> "I expect that the project and what's being worked on and scratch files
> and the Git repository are on a native file system in the container. But
> it would be amazing if that native file system on the host was not some
> kind of obscure block format."

> "Single writer to both the file system and the cache at the same time,
> and the kernel on the guest does not maintain its own cache. It references
> the cache from the host that has been exposed."

> "I would rather just build it because it is the correct solution."

Two kernels cannot both own a filesystem. Existing designs pick a corner:
one owner with round-trip clients (NFS, virtiofs), shared disk with
distributed locks (GFS2), or replication (mirrors). This design takes the
one-owner corner and removes the round trips for everything except
mutation, which is possible because there is exactly one writer.

## 2. Roles

- **Host daemon** (`sharedfs` crate, in the VMM process). Sole writer of
  the shared index. Executes every guest mutation against host ext4 as the
  owning user, then updates the index. Maps ranges of host files into the
  data window on request. Watches the directory and folds host-side
  changes into the index and the notification queue.
- **Guest driver** (`fs/sharedfs` in our kernel). Resolves lookups,
  attributes and directory listings by reading the shared index. Reads
  and writes file bytes through the data window. Sends mutations and
  mapping requests on a virtqueue, receives notifications on another.
  Keeps no authoritative state and no cache of its own.
- **Host users**. Edit the directory directly with whatever they like.
  The daemon learns of it through inotify and nothing on the host has to
  know a guest exists.

**Lock granularity.** The lock is the daemon instance, and its domain is
the exported directory: one daemon per export, created when the guest's
mount is instantiated, sole writer of that export's index.

> "I didn't want to have the granularity of the lock be system wide. I
> thought that it would make more sense if it was attached to a specific
> directory, and so perhaps that is just the instantiation of the mount in
> the VM that then sets which lock instance is actually used."

The daemon holds `flock(LOCK_EX)` on the export root for the life of the
mount. A second guest pointed at the same directory, or at a directory
inside or enclosing one that is already exported, is refused at start.
Host editors are unaffected: flock is advisory and they never take it.

A guest may carry several exports, each its own virtio device with its
own index, window and queues, so the lock granularity above is also the
device granularity. The device carries the export's tag NUL-padded at
the start of its configuration space, as virtio-fs does, and a mount
names the tag as its source. The pet's two exports are the project tree
(`work`) and the host's `~/.config` (`config`, read-only, host as
writer), the latter so that configuration is never copied:

> "as soon as you make a copy, now we're managing differences over time
> ... We're just gonna end up with config drift all over the place"

The guest kernel is ours and the daemon is ours, so neither side carries
a general-purpose layer the other could be swapped out from under:

> "We can just keep this simpler because we know the conventions that
> we're operating within, and so we don't need to add a ton of
> abstraction. We just need to hook into the places that are relevant,
> create the Rust bindings for them, and implement what is essentially
> the default behavior of the kernel into the file system in the ways
> that we need."

## 3. Shared index

A fixed-size memory region, host-allocated, mapped into the guest
read-only (`KVM_MEM_READONLY`), so single-writer is enforced by the
hardware page tables rather than by convention.

```
[ Header (one page) ][ Entry[entry_cap] ][ u32 table[table_cap] ]
```

**Entry** is 384 bytes, `repr(C)`, little-endian, one per live file or
directory. Names are inline (`NAME_MAX` is 255, so no arena is needed).
Entry slot `i` has inode number `i + 1`; a per-slot generation counter
distinguishes reuse, and the guest exposes it as `i_generation`.

Each entry carries a **seqlock**: `seq` is odd while the daemon is
writing it and even when stable. A reader copies the entry, re-reads
`seq`, and retries if it moved or was odd. The daemon fully writes an
entry before linking it into its parent's child list or the hash table,
with release ordering, so a linked entry is always complete.

**Lookup** is a hash table keyed by `(parent slot, name)`, chained through
`Entry.hash_next`. **Readdir** walks the parent's doubly linked child list;
the cookie is the child slot. Both are pure memory reads in the guest.

Capacity is fixed at mount from the region size. A full index returns
`ENOSPC`. Caches and build output belong on the guest's native disk by
environment convention, which keeps project trees well inside the
default capacity.

**Host-side changes.** The daemon is the index's only writer, but not
the directory's: editors, `git` and everything else on the host write
the project tree directly. The daemon holds an inotify watch on every
exported directory and folds each event into the index on its own
thread, so a change made on the host is a plain index update to the
guest, indistinguishable from one the guest requested. Events name a
directory and an entry; the daemon re-examines that one name against the
disk (created, removed, replaced, moved, changed in place) rather than
trusting the event type. A queue overflow re-examines every directory.

## 4. Queue protocol

One request queue, guest to daemon, and one notification queue, daemon to
guest. Fixed `repr(C)` headers, little-endian, names as `u16` length plus
bytes. Request opcodes:

| Opcode | Purpose |
|---|---|
| MAP, UNMAP | map a range of a file into the data window, give it back |
| EXTEND, FSYNC | grow a file before writing past its end; flush it |
| CREATE, MKDIR, SYMLINK | new entries |
| UNLINK, RMDIR, RENAME | removal and moves |
| SETATTR | mode, owner, size, times |
| READLINK, STATFS | the two reads the index does not answer |

OPEN, RELEASE, READ and WRITE existed in the first read-only guest and
go away with the data window (§5); the daemon does not hold file handles
on the guest's behalf once data is shared by reference.

The daemon applies a mutation to the host directory first and the index
second, inside the entry's seqlock. The guest's own writes are therefore
visible in the index by the time the response arrives.

Notifications carry what the guest cannot read from the index: *that*
something changed, so the guest's own fsnotify can fire, and that a
mapping is about to go.

| Notification | Guest action |
|---|---|
| CHANGED slot, generation, kind, name | raise the matching fsnotify event on the inode and its parent (created, removed, moved, modified, attributes) |
| UNMAP window range | drop guest mappings of the range, acknowledge; the daemon unmaps only after the acknowledgement |

Everything else about a change the guest reads from the index on its
next operation, which is why the queue carries no attributes or data.
The daemon's own mutations on the guest's behalf also show up in
inotify; it recognises those echoes and does not notify the guest about
work the guest already knows it did.
A notification queue overflow is a signal to the guest to drop every
cached dentry, not an error.

Not supported in v0, returned as errors rather than emulated: hard links
(`EPERM`, the index is a tree keyed by name), extended attributes
(`ENOTSUP`), device nodes (`EPERM`).

Feature bits are negotiated at device setup, virtio-style, so a newer
daemon serves an older guest kernel during a rolling update.

## 5. Data path

File bytes are shared the same way metadata is: by reference, never
copied.

> "What we essentially want to do is share the data as a reference, and
> then send inotifies, and that covers it all."

The VMM reserves a **data window**, a range of guest-physical addresses
backed by a reserved host virtual range and registered as one KVM memory
slot. The daemon maps ranges of host files into it with `mmap(MAP_FIXED |
MAP_SHARED)`; the guest's view of those addresses *is* the host page
cache. A host write is visible to the next guest read with no message,
a guest write dirties the host page cache directly, and there is no
guest page cache for file data at all. Guest `mmap(MAP_SHARED)` is
coherent with host editors for the same reason.

On the guest, the kernel already has the machinery for a file system
whose bytes live at physical addresses outside its page cache: DAX
(`fs/dax.c`, driven through `iomap`). The driver's job is to answer
`iomap_begin` with "this range of this file is at that window offset",
and the kernel does `read`, `write`, `mmap` shared and private,
copy-on-write, exec, `msync` and `get_user_pages` from there. We call it
rather than reimplement it; the cost is iomap and DAX bindings in Rust.

The window is finite, so mappings are a cache of references, not of
bytes. The guest asks the daemon to MAP a chunk of a file when iomap
needs it, keeps a small table of live chunks, and frees the least
recently used when the window is full. Chunk size is ours to choose,
since we own both ends; virtiofs uses 2 MiB and pays for it on small
files. A write past the end of a file is preceded by EXTEND so the host
file exists under the whole chunk before the guest touches it: a store
into a shared mapping beyond the host file's end is a fault on the vcpu
thread, which the VMM must also survive for the truncate-from-host race
(§4, UNMAP).

The first guest moved data through READ and WRITE requests into the
guest page cache. That proved the index and the protocol, and showed
the cost of a second copy within a day: a file appended on the host read
back with zeros where the new bytes should be. The copy path is retired
rather than kept as a fallback; one data path.

## 6. Host confinement

The daemon resolves a slot to its parent directory by walking the index to
the root and opening the path with `openat2(RESOLVE_BENEATH |
RESOLVE_NO_SYMLINKS)`. Every mutation then acts on a single final
component relative to that directory fd. No request can address anything
outside the project directory.

## 7. Guest kernel

Built by us, one per VMM release, shared by every project VM, with the
filesystem compiled in. No modules, no initramfs. Rust in the kernel
requires our own kernel build anyway. The kernel-to-daemon contract is the
protocol in §4, versioned; guest kernel and daemon update independently.

## 8. Order of work

1. This document.
2. Host crate: index + server as a pure library, exercised by tests that
   speak the protocol with no kernel. *(this sprint)*
3. VMM device: MMIO shared-memory registers, region mapping, queue worker.
   *(done; the worker also pumps the host-side change watcher)*
4. Guest driver, read-only first: mount, lookup, readdir, getattr, read.
   *(done 2026-10-10; research/17 has the kernel-side detail)*
5. Notification queue: daemon to guest, feeding guest fsnotify and
   mapping teardown. *(done 2026-10-10 for fsnotify; UNMAP arrives with
   the window)*
6. Data window: VMM slot, daemon mapping and reclaim, guest iomap and
   DAX bindings, MAP/UNMAP/EXTEND. Retires READ/WRITE/OPEN/RELEASE.
   *(done 2026-10-10)*
7. Mutations: create, mkdir, symlink, unlink, rmdir, rename, setattr,
   fsync. The pet's project mount moves from virtiofs to sharedfs.
   *(done 2026-10-11, with echo suppression for the guest's own changes
   and the flock on the export root; the generation-keyed inode lookup
   that closes the stale-slot gap and the refusal of nested exports are
   still open)*
8. An index that outlives the daemon: persisted on the host, reconciled
   against the directory at start with the differences sent as
   notifications, and freeze/restore in the guest driver. This is what
   lets a guest hibernate and resume with its sharedfs mounts intact;
   together with a swap area on the data disk it gives the pet a freeze
   that survives a host reboot, tmux sessions included.
   *(done 2026-10-11: `pet-freeze-smoke` hibernates a pet, changes the
   project while it is down, resumes it and finds its processes running
   and the changes reported; research/17 has the detail)*
9. xfstests generic group as the definition of native.

Reordered 2026-10-10 from "mutations, notifications, DAX": finishing a
writable copy path and then replacing it would have been work thrown
away, and the notification queue is needed under any data path.
