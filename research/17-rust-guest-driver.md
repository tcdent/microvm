# sharedfs guest driver in Rust, built to upstream

Successor to research/16, which specified the host side and the protocol.
This note sets the plan for the guest half: a kernel filesystem written in
Rust, with the two abstraction layers under it written so that they could
go to mainline.

> "even if we have to write Rust bindings for the network or the file
> system operations in the kernel, it's still a worthwhile pursuit ... a
> real full implementation, which is something that we could potentially
> upstream into the Linux kernel."

## Kernel base

Linux 7.2.9, the current stable at the start of this work (2026-10-10),
imported from the release tarball into `kernel/src/linux-7.2.9` as a git
repository with the release as the baseline commit. Our work lives on
branches above it. The tree moves to each new stable as it appears and to
mainline when a series is being prepared for posting; the price of chasing
upstream is rebasing, and the alternative, pinning an LTS, would leave the
bindings two years behind the `kernel` crate they must fit.

Toolchain on elsenor: rustc 1.94 (the tree requires 1.85 or newer on
x86), bindgen 0.73 from `cargo install bindgen-cli`, and a libclang 18
from the PyPI `libclang` wheel under `~/.local/libclang`, because Debian
12 ships libclang 14 and the tree requires 17. The C side is built with
GCC 12. `make rustavailable` passes with `PATH` and `LIBCLANG_PATH` set as
`kernel/build-kernel.sh` does.

## Three layers

1. **virtio bindings.** Not ours. Manos Pitsidianakis's RFC v3 "Add Rust
   virtio bindings and sample device" (rust-for-linux, July 2026) provides
   `virtio::Driver`, `Device::find_vqs`, `Virtqueue::{add_sgs, kick,
   get_buf}` and scatter-gather tables. It applies to 7.2.9 apart from
   MAINTAINERS hunks. We carry it as the bottom of our branch, add what
   sharedfs needs that it lacks, first a `virtio_get_shm_region` accessor
   for the index window, and send those additions back to the series.
2. **VFS abstractions.** Wedson Almeida Filho's RFC v2 "Rust abstractions
   for VFS" (May 2024, 30 patches, against 6.9) is the design reference:
   `FileSystem` with `fill_super` and `init_root`, `INode<T>`,
   `DEntry<T>`, inode, file and address-space operation traits, folios.
   It does not apply to 7.2 and was never merged. We port its shape onto
   the 7.2 `kernel` crate, which now has `fs::File`, `Kiocb`, `IovIter`,
   `mm::virt`, `Page` and `dma`, and add what a writable filesystem with a
   custom lookup path needs: dentry revalidation, directory iteration
   with cookies, create/unlink/rename/setattr, write paths, statfs.
   This is the layer with upstream value beyond sharedfs and is written
   as generic VFS abstractions, with sharedfs as its first user the way
   tarfs was the RFC's.
3. **sharedfs.** The filesystem from research/16 on top of the two:
   metadata from the shared index under seqlocks, mutations and data over
   the virtqueue, a conventional page cache for data in v0 so mmap and
   exec work, DAX afterwards.

## Order of work

1. Branch `sharedfs` on the 7.2.9 baseline. Enable `CONFIG_RUST`. Apply
   the virtio series. Build and boot with the sample driver compiled in
   as the proof that Rust runs in our guest kernel.
2. Add the shm-region accessor to the virtio bindings. A throwaway Rust
   driver binds device 0x5346, maps the index window and prints the
   header: the smoke script's devmem reads, done by the kernel.
3. VFS abstractions, read-only subset, by porting the RFC's patches 01-18
   onto 7.2: registration, `fill_super`, inodes, dentries, lookup,
   `read_dir`, `read_folio`. Milestone: a mountable sharedfs that lists
   and reads the project tree.
4. Writable subset and the remaining sharedfs operations. The pet's fstab
   line changes from virtiofs to sharedfs.
5. xfstests generic group. Then the notification queue and DAX.
6. Posting: the virtio additions to the existing series, the VFS
   abstractions as an RFC with sharedfs as the user, in that order.

## Layout

```
kernel/src/linux-7.2.9/        git: baseline-7.2.9, branch sharedfs
kernel/build-kernel.sh         builds the branch with Rust enabled
sharedfs/src/proto.rs          the protocol; mirrored into the driver by hand
                               until a shared definition is worth the tooling
```

## Status, 2026-10-10

Steps 1 and 2 done. Branch `sharedfs` on the 7.2.9 baseline carries the
virtio RFC v3 (six commits, original authorship), a forward-port commit
for two 7.2 driver-model changes the series predates, a commit adding
`Device::shm_region()` and `DeviceId::from_raw()`, and `fs/sharedfs/` as a
probe-only driver. The kernel builds with 786 Rust symbols and boots the
pet with every smoke check passing; it is now `dist/vmlinux`, with the
6.12 kernel kept beside it. At boot the driver logs:

    sharedfs: index 64 MiB at 0xe0000000: 169290 slots (4 live), table 524288
    sharedfs: bound, 1 virtqueue(s)

Next is step 3, the read-only VFS subset.

## Status, 2026-10-10, later

Step 3 done: the pet mounts its project directory read-only through
sharedfs. Six more commits on the `sharedfs` branch:

- `rust: sync: add Locked` — proof-of-lock type from the RFC.
- `rust: fs: abstractions for implementing file systems` — the RFC's
  patches 01–26 ported onto the 7.2 crate as one commit, plus `folio.rs`
  and `mem_cache.rs`. Departures from the RFC are listed in the commit
  message; the notable ones are that `File` is not parameterised by file
  system, block-device superblocks and xattrs are out, and
  `dentry::Operations::revalidate` is new. To be split along the RFC's
  seams before posting.
- `samples: rust: add read-only file system sample` — the RFC's
  `rust_rofs`, built in and mounted by the pet smoke as a device-free
  check of the abstractions.
- `rust: virtio: raw scatter-gather submission` — `add_sgs_raw`, so a READ
  lands straight in the page-cache page.
- `fs: sharedfs: read-only file system over the host index`.

Two facts the kernel crate forced:

- There is no interrupt-safe spinlock in Rust yet, and the virtio RTC
  sample in the RFC has a `FIXME` for exactly that. The vring is therefore
  only touched under a mutex in process context; the virtqueue callback
  schedules a work item that drains completions. One extra hop per
  request, no lock in interrupt context.
- `module!` implements `ModuleMetadata` for the module type, so the RFC's
  `fs::Module<T>` falls to the orphan rule. `module_fs!` now declares a
  local module type instead.

Host side, found by the first guest test: the daemon only scanned the
tree at start, so edits made on the host never reached the index. The
daemon now watches every exported directory with inotify and applies
changes through the same single-writer path (`sharedfs/src/watch.rs`,
`Server::pump_watch`); the device worker polls the kick eventfd and the
inotify fd together.

Verified by `pet/bin/pet-fs-smoke`, fifteen checks: both mounts, listing,
a 300 KB file and a text file by hash, symlink, nested path, mode and
owner, negative lookup, statfs, and three host-side changes while the
guest is up (new file and directory, removal, size change).

Known gaps, in order of bite:

1. The guest page cache is not invalidated when the host changes a file's
   contents. Names, sizes and times refresh on the next path walk through
   `d_revalidate`; already-cached pages do not. This is the notification
   queue, step 6 of research/16.
2. A slot reused for a different file while the old inode is still cached
   returns `ESTALE` from lookup until the old inode is evicted. The fix is
   `iget5_locked` keyed on (slot, generation); the abstraction only has
   `iget_locked` today.
3. Read-only: `SB_RDONLY` is forced in `fill_super`. Step 4.
4. `add_sgs_raw` and `Device::drvdata` are `unsafe` entry points the
   driver uses directly; both want safe wrappers before posting.
5. Large folios are off, so reads are one page per request.

Plan change, same day: research/16 §5 and §8 now share data by reference
through a window with DAX instead of copying through READ/WRITE, and the
order becomes notification queue, data window, mutations. Gap 1 above is
retired by the window rather than fixed; gap 5 goes with the copy path.
## Status, 2026-10-10, notification queue

Done, two commits: `rust: fs: fsnotify, page invalidation and dcache
helpers` and `fs: sharedfs: notification queue`. On the host the watcher
now carries inotify masks and folds a moved-from/moved-to pair into one
rename, so a directory moved on the host keeps its slot and the guest's
inode for it survives; the device worker has a second queue with a
bounded outbox (1024, then one OVERFLOW). On the guest, 64 posted buffers,
a drain that reposts before delivering, and delivery to the mount under
the session's lock.

What a guest watcher sees for host-side changes, from `inotifywait -r` in
the pet during `pet-fs-smoke`:

    CREATE /mnt/s/late.txt
    CREATE,ISDIR /mnt/s/sub/latedir
    DELETE /mnt/s/sub/deep/one
    MODIFY /mnt/s/README.md
    CLOSE_WRITE,CLOSE /mnt/s/README.md
    MOVED_FROM,ISDIR /mnt/s/sub/deep
    MOVED_TO,ISDIR /mnt/s/sub/moved
    MOVE_SELF /mnt/s/sub/moved/

That is the same log a local file system produces. Twenty-two checks in
`pet-fs-smoke`, all passing; `pet-smoke` unchanged at nine.

Two interim measures, both retired by the data window: the driver drops
an inode's pages on MODIFY so the copy in the guest page cache cannot go
stale, and a single mount per session (a second gets `EBUSY`), since
notification fan-out to several superblocks is not needed until it is.

One bug worth remembering from the first run: the guest decoded every
notification as malformed because the name offset was off by four. The
wire format is defined once on the host (`proto::Notification`) and
mirrored by hand in the driver; when the data window adds messages, the
mirror is the place to look first.

## Status, 2026-10-10, data window

Done, two commits: `rust: fs: DAX for file systems whose data lives in a
physical window` and `fs: sharedfs: file data through the window with
DAX`. The kernel config gained ZONE_DEVICE, DAX and FS_DAX.

Shape, as built:

- VMM reserves 8 GiB of host address space (`PROT_NONE`), registers it
  as one KVM memory slot at guest-physical 16 TiB, and reports it as
  shared memory region 1. The daemon owns the reservation and answers
  MAP by `mmap(MAP_FIXED | MAP_SHARED)` of the file range into it, UNMAP
  by putting `PROT_NONE` back, EXTEND by `ftruncate` upward, FSYNC by
  `fsync`. No host file handles are held for data anymore.
- Guest `fs::dax`: `Window` runs `memremap_pages` over the region
  (ZONE_DEVICE, `MEMORY_DEVICE_FS_DAX`), `Device` is a `dax_device` whose
  `direct_access` is arithmetic, the iomap `Operations` trait answers
  `begin` with a window offset, and read, write, mmap and faults are the
  kernel's `dax_iomap_rw` and `dax_iomap_fault`. `break_layout` wraps
  `dax_break_layout` with the invalidate lock for reclaim.
- `fs/sharedfs/data.rs`: chunk table keyed by (slot, chunk index) in an
  rbtree under one mutex, per-chunk user counts from `begin` to `end`,
  LRU reclaim in the read/write path (fault path reports EAGAIN and the
  handler retries), chunks returned when an inode is destroyed, EXTEND
  before a write past the end, `truncate_pages` when the host shrinks a
  file.
- VMM: a `KVM_EXIT_MEMORY_FAULT` inside the window, which a host
  truncate under a live guest mapping can cause, is answered with a page
  of zeros; beyond a file's end that is what is there.

Verified by `pet-fs-smoke`, 24 checks: among them a 5 MB file hashed
through three chunks, `/bin/true` executed from the mount (mmap and
fault path), partial reads at both ends, and a host append read back
correctly with no invalidation anywhere. `pet-smoke` nine of nine.

The window costs the guest 128 MiB of `struct page` for 8 GiB, so the
window is bounded and reclaim is real, not theoretical. Window size is a
constant in `sharedfs::device` for now.

The bug of the day: `Device::new(&window)` kept a pointer to a `Window`
on the probe's stack, and the window was then moved into the driver's
struct. Reads returned whatever the stack held. The device now owns its
window in a `KBox`, and the constructor is safe.

Not done: UNMAP as a daemon-to-guest notification (the zero-fill
fallback covers the race it was for); echo suppression for the guest's
own mutations; the guest's own writes are untested until the mount is
read-write (step 7).

Next is step 7: mutations (create, mkdir, symlink, unlink, rmdir, rename,
setattr), dropping `SB_RDONLY`, and the switch of the pet's project mount
from virtiofs to sharedfs.

## Status, 2026-10-11, several exports

Three commits: `rust: virtio: read device configuration space`
(`Device::config_read` over `virtio_cread_bytes`), `rust: fs: hand
fill_super the mount source` (`fill_super(sb, source: Option<&CStr>)`
from `fs_context::source`), and `fs: sharedfs: one device per export,
mounted by tag`. The driver keeps bound sessions in a global list keyed
by a 36-byte tag read from configuration space; `mount -t sharedfs TAG`
picks one; a duplicate tag is refused at probe with EEXIST.

VMM: `VM_SHAREDFS` is a list of `TAG=PATH[:WINDOW]`, one device each at
MMIO 0xD000_8000 + 4K*i, IRQ 15+i, index slot at 0xE000_0000 + 64 MiB*i,
window at 16 TiB + 8 GiB*i. The window size is per export (the config
export takes 1 GiB, so 16 MiB of guest page metadata instead of 128).

The pet mounts `config` at `/home/slomp/.config` from an fstab line the
initramfs writes for `pet.config=`; the seed is pointers only (the
`~/.claude` symlinks the dotfiles README prescribes, including the
credentials symlink added on the host the same day, and `.bashrc` /
`.bash_profile` stubs that source `~/.config/bash/bashrc` if present).
`pet-fs-smoke` grew a check for the mount: 25 checks, passing on four
consecutive runs. Two earlier runs failed with no guest output at all,
before the smoke printed the VMM's stderr on failure; the VMM had exited
within about 90 s of launch. Unexplained. The smoke now prints that
stderr, so the next occurrence is diagnosable. Host inotify instances
were at 57 of 128 when checked afterwards, which makes `inotify_init`
failing in `Server::open` a candidate, not a finding.

Step 7 stands as written above.

## Status, 2026-10-11, writes

Two commits: `rust: fs: mutating inode operations` and `fs: sharedfs:
writes`. The abstractions gain create, mkdir, symlink, unlink, rmdir,
rename and setattr on `inode::Operations`, `Unhashed::instantiate`,
`DEntry::name`, a `SetAttr` view of `struct iattr`, and
`FileSystem::READ_ONLY` in place of the forced `SB_RDONLY`. The driver
sends each mutation to the host and refreshes the affected inodes from
the index; the host records the inotify events its own actions will
raise and drops them when they arrive, so the guest sees each of its
own changes exactly once, from its own VFS.

Two lessons. A regular file must not carry `get_link` in its
operations: `d_flags_for_inode` takes any non-directory inode with one
for a symlink, and every read then fails. One operations table per
inode type, as C file systems do. And a file system with no inode
operations at all gets `simple_setattr`, which succeeds without telling
anyone; chmod and truncate "worked" and reverted on the next
revalidation.

The pet's project tree is now mounted from fstab as sharedfs; virtio-fs
is out of the launcher. `pet-fs-smoke` is 32 checks: the earlier 25
plus guest create, append, mkdir, symlink, rename, chmod, truncate, a 5
MB copy, unlink and rmdir, each verified from the host afterwards, and
a count that each guest change raised exactly one inotify event in the
guest. Passing on three consecutive runs; `pet-smoke` 9 of 9.

Still open from step 7: `iget5_locked`-style lookup keyed by (slot,
generation) so a reused slot does not return ESTALE while the old inode
is cached; refusing an export nested in or enclosing another. The
console harness occasionally garbles a typed line (one run showed a
doubled character, one lost a line); `KEEP=DIR` now saves the
transcript for the next time.

Next, toward hibernation: the index that outlives the daemon (16 §8
item 8), then freeze/restore in the driver, then `CONFIG_HIBERNATION`,
a swap file on the data disk and `resume=`.

## Status, 2026-10-11, hibernation

Three commits: `rust: virtio: freeze and restore callbacks`, `fs:
sharedfs: inode numbers carry the generation`, `fs: sharedfs: survive
hibernation`. The kernel config gains `CONFIG_HIBERNATION` (and so
`PM_SLEEP`); the canonical `.config` and `build-kernel.sh` follow.

The pieces, in the order the freeze needs them:

- **Inode numbers** are slot plus the low 32 bits of the generation, on
  both sides. A reused slot is a new inode; a stale number gets ESTALE
  from the host. The `MAP` request's own generation field is now
  redundant and stays for the moment.
- **Persisted index.** `Index::open_file` maps the index from a file in
  the VM's state directory (`VM_STATE`, `sharedfs-<tag>.index`) and
  adopts an existing one after checking its geometry and that no entry
  was mid-write. `Server::open` then reconciles: every known directory
  is re-read name by name, surviving entries keep slot and generation,
  and each addition, removal and change becomes a notification. Those
  are held until the guest's first request and replayed after every
  device reset until then, because on a resume boot the kernel that
  loads the image takes the first copy and the restored kernel needs
  one too.
- **Device reset in the VMM.** Hibernation resets every virtio device
  twice: once when the snapshot is taken (devices are resumed to write
  the image) and once when the restored kernel takes over. None of the
  VMM's devices handled a reset. Now the MMIO transport clears the queue
  configuration on a status write of zero, each device resets its ring
  positions, and the worker threads (console RX, net RX, sharedfs) share
  their queues with the device through a mutex instead of owning them,
  so the reconfigured rings reach them. The sharedfs worker is started
  once per VM and its ioeventfd registered once.
- **Guest driver freeze/restore.** `freeze` breaks the layout of every
  mapped window chunk, forgets the chunk table and drops the virtqueues;
  `restore` finds the queues again and reposts the notification buffers.
  The mount and its inodes survive in the image.
- **Guest side of the pet.** A swap disk sized to RAM (`vdc`, made by
  the launcher), `resume=/dev/vdc` on the command line, a swap line in
  the fstab the initramfs writes, and `HibernateMode=reboot` in a
  `sleep.conf.d` drop-in the initramfs writes too: the VMM has no ACPI,
  so a powered-off guest would halt, while a reset ends the VMM and the
  next launch resumes.

Two kernel-side details found the hard way. In 7.2's
`fsnotify_handle_event`, a non-dirent event delivered on a directory's
own mark has its name stripped (`if (!(mask & ALL_FSNOTIFY_DIRENT_EVENTS))
name = NULL`); only the parent-mark path keeps it, and that path needs
the child's inode. So an event about an entry the guest never opened
now instantiates it first, as the CREATE path already did, and reports
through `notify_child`. And `virtio_console` writes spin with the port
lock held until the device consumes the buffer, so a device that loses
its queue state after a reset wedges every console writer, which read
as "the guest went silent".

`pet-freeze-smoke`: boot, start tmux sessions with running loops and an
inotify watcher, `systemctl hibernate`, change the project on the host
while the VMM is gone, launch again, and check 15 things: resumed,
uptime continues, both tmux sessions alive, counters advancing, host
changes visible and reported as named events, a write after resume
reaching the host, clean exit. Passing twice in a row; `pet-fs-smoke`
32 of 32 and `pet-smoke` 9 of 9 on the same build. The smoke itself had
to learn that a resumed guest prints no login banner.

Not done: the guest clock does not advance across the gap (no RTC);
timesyncd steps it. A kernel upgrade costs the sessions once. The
`MAP` generation field can go.
