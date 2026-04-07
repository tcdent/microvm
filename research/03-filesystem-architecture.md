# Filesystem Architecture — S3-Backed virtiofs

## The Core Principle

Zero guest-side code. The guest is a stock Debian/Ubuntu system. All
services — storage, networking, lifecycle — are provided by the host
VMM. The guest doesn't know and doesn't care how its filesystem is
backed. This is the whole point of virtualization.

The filesystem is not a disk. It's not blocks. It's files in S3.
The guest sees POSIX. We provide POSIX. Behind the scenes, every
file is an S3 object.

## Why Not Firecracker's Approach

Firecracker was designed for AWS Lambda — stateless, ephemeral, no
persistent filesystem, no restarts. Everyone using it for coding agents
is bolting on persistence after the fact. E2B runs authentication
agents inside the guest and communicates with external servers. This
is what happens when you adopt a stack designed for a different use
case.

We're designing the stack from the ground up for persistent,
interactive coding agent containers.

---

## Architecture

```
Guest userspace:  open("/home/agent/code.py") → read() → write()
       │
  Guest VFS:      dentry lookup, page cache, permissions
       │
  virtiofs driver (kernel module, NOT userspace FUSE):
       │          encodes as FUSE_LOOKUP, FUSE_READ, FUSE_WRITE, etc.
       │
═══ virtio MMIO transport ══════════════════════════════════
       │
  Host VMM process (our Rust binary):
       │
  FUSE protocol handler (fuse-backend-rs):
       │          decodes FUSE ops, dispatches to our filesystem impl
       │
  S3 filesystem layer:
       │          maps FUSE ops to S3 API calls
       │          inode table in memory, loaded from S3 at mount
       │
  S3-compatible storage (MinIO local, AWS S3, any S3 API):
              one bucket per VM instance
```

### Guest mount layout

```
/              virtiofs → S3 bucket (persistent, all user-visible files)
/proc          procfs (kernel, RAM only)
/sys           sysfs (kernel, RAM only)
/dev           devtmpfs (kernel, RAM only)
/run           tmpfs (RAM only)
/tmp           tmpfs (RAM only)
```

The kernel handles this split naturally via mount points. Pseudo-
filesystems (/proc, /sys, /dev) and runtime state (/run, /tmp, Unix
sockets, pipes, device nodes) live in RAM and are never persisted.
Everything the agent works with — /home, /etc, /usr, /var — is
S3-backed through virtiofs.

---

## Boot Sequence

The virtiofs root filesystem is not available at the very start of
boot. The kernel needs an initramfs to bootstrap:

```
1. Kernel loads, mounts initramfs as temporary root (RAM)
2. Initramfs /init mounts /proc, /sys, /dev, /run (all RAM)
3. Initramfs loads virtiofs kernel module
4. mount -t virtiofs rootfs /sysroot
5. switch_root /sysroot /sbin/init
6. Real init (systemd) starts, reads /etc from virtiofs
```

No chicken-and-egg problem. The virtiofs kernel module is self-
contained — no config files from the root filesystem needed to load
it. It talks directly to the virtio transport. Our VMM's FUSE handler
just needs to be ready before step 4, which is 1-2 seconds into boot.

The initramfs is a small custom image containing: busybox (or a
static init binary), the virtiofs kernel module, and a mount script.
It's loaded into guest memory alongside the kernel, same as the
Debian netinstall initrd we use today.

---

## S3 Data Model

### Storage layout

```
Bucket: vm-{instance-id}

Object key:     File path relative to root, no leading slash
                e.g., "home/agent/code.py"
                e.g., "etc/hosts"

Object body:    File contents (max 100MB)
                Zero bytes for directories and empty files

Object metadata (x-amz-meta-*):
  mode:         POSIX mode as octal string, e.g., "0100644"
                Includes file type bits (regular, dir, symlink, etc.)
  uid:          Numeric user ID, e.g., "1000"
  gid:          Numeric group ID, e.g., "1000"
  mtime:        Modification time as epoch seconds
  type:         "file", "dir", "symlink"
  target:       Symlink target path (only if type=symlink)
```

S3 allows 2KB of user metadata per object. A typical file's metadata
is ~100-200 bytes. Plenty of room.

### Directories

Directories are zero-byte S3 objects with a trailing `/` in the key:

```
Key: "home/agent/"
Body: (empty)
Metadata: mode=0040755, uid=1000, gid=1000, type=dir
```

`ListObjectsV2` with `Prefix` and `Delimiter=/` provides directory
listing. This is how S3 natively presents hierarchy.

### Permissions

S3 ACLs and IAM do not map to POSIX uid/gid/mode. We store all
POSIX permissions in object metadata and enforce them in our FUSE
handler. The S3 bucket itself uses IAM for access control between
our VMM processes and the storage backend — one bucket per VM
provides isolation at the storage level.

### File size limit

100MB maximum per file. This eliminates the need for chunking,
multipart uploads, or partial object updates. Every write is a
whole-object PUT. Every read is a whole-object GET (or range GET
for partial reads, which S3 supports natively).

100MB is appropriate for coding agent workloads: source code,
configs, build artifacts, logs, small databases. Video editing
and large dataset processing are not target use cases.

### Future extensibility

If we ever need files larger than 100MB, the metadata schema
supports it without protocol changes. A zero-byte S3 object with
metadata pointing to chunked data objects acts as a file handle.
The FUSE handler checks metadata, finds chunks, assembles reads.
Not built now, but the door is open.

---

## FUSE Operation → S3 Mapping

Every FUSE operation maps to one or two S3 API calls:

| FUSE Op    | What it does           | S3 Call                        |
|------------|------------------------|--------------------------------|
| INIT       | Protocol negotiation   | (none — local state)           |
| LOOKUP     | Find file by name      | HeadObject                     |
| GETATTR    | stat() — size, perms   | HeadObject (metadata + size)   |
| SETATTR    | chmod, chown, utimes   | CopyObject (update metadata)   |
| READ       | Read file bytes        | GetObject (Range header)       |
| WRITE      | Write file bytes       | PutObject (whole object)       |
| CREATE     | Create new file        | PutObject (empty or with data) |
| MKDIR      | Create directory       | PutObject (zero-byte, key/)    |
| UNLINK     | Delete file            | DeleteObject                   |
| RMDIR      | Delete directory       | DeleteObject (key/)            |
| RENAME     | Move/rename            | CopyObject + DeleteObject      |
| READDIR    | List directory         | ListObjectsV2 (prefix+delim)   |
| SYMLINK    | Create symlink         | PutObject (metadata has target) |
| READLINK   | Read symlink target    | HeadObject (read target meta)  |
| OPEN       | Open file handle       | (local state, maybe prefetch)  |
| RELEASE    | Close file handle      | (flush pending writes)         |
| FLUSH      | Sync writes            | (ensure PUT completed)         |
| STATFS     | Filesystem stats       | (return fixed values)          |

Operations we can reject or stub:
- LINK (hardlinks) — return ENOTSUP, rare in agent workflows
- MKNOD (device nodes) — return ENOTSUP, /dev is devtmpfs
- IOCTL — return ENOTTY
- FALLOCATE — return ENOTSUP
- LSEEK — handled by guest kernel

### Write path detail

Guest writes go through the FUSE WRITE operation. Our handler:

1. Receives the full write data from the FUSE request
2. PUTs the complete file to S3 (whole-object replacement)
3. Returns success to the guest

For small files (source code, configs) this is fast — a few KB over
the network. For larger files approaching the 100MB cap, the PUT
takes longer but is still a single network operation.

The guest kernel's page cache absorbs most write patterns. Repeated
writes to the same file will be batched by the kernel before hitting
our FUSE handler. We don't need our own write buffering.

### Read path detail

Guest reads go through FUSE READ. Our handler:

1. Receives the read request (inode, offset, size)
2. Issues an S3 GetObject with Range header for the requested bytes
3. Returns the data to the guest

The guest kernel's page cache is our primary optimization. Once a
file is read, subsequent reads are served from the guest's RAM. We
don't maintain a host-side cache — the guest cache is sufficient
and avoids double-caching complexity.

---

## Inode Management

S3 is flat key-value. FUSE speaks inodes. We maintain an in-memory
inode table that maps inode numbers to S3 keys:

```
Inode 1  →  ""           (root directory)
Inode 2  →  "etc/"
Inode 3  →  "etc/hosts"
Inode 4  →  "home/"
Inode 5  →  "home/agent/"
...
```

On mount, we do a recursive ListObjectsV2 to build this table.
For a typical Debian install (~50K files), this takes a few seconds
and uses ~5MB of memory for the table.

Inode numbers are assigned sequentially and are stable for the
lifetime of the mount. They don't persist across VM restarts (the
table is rebuilt from S3 on each mount). This is fine — no
application depends on stable inode numbers across reboots.

---

## Golden Images

A base OS image (e.g., Debian Trixie minimal) is a set of S3 objects
in a template bucket:

```
Bucket: template-debian-trixie
  bin/bash          (file contents + metadata)
  bin/ls            (file contents + metadata)
  etc/              (directory marker)
  etc/hosts         (file contents + metadata)
  etc/passwd        (file contents + metadata)
  ...               (~50K objects for a minimal install)
```

Creating a new VM:
1. Create bucket `vm-{instance-id}`
2. Copy the template manifest (or use S3 cross-bucket references)
3. On first write to any file, the object is created in the VM's
   bucket, overriding the template

This is CoW at the S3 object level. Reads fall through to the
template bucket if the object doesn't exist in the VM bucket.
Writes always go to the VM bucket.

### Building a golden image

Take a Debian install (from debootstrap or a disk image), walk the
filesystem, upload each file as an S3 object with the appropriate
metadata. A script that does `find / | upload_each_to_s3` with
metadata extraction. This is a build-time operation, done once per
base image version.

---

## What We Need to Build

1. **virtio MMIO transport** — shared infrastructure for any virtio
   device. Register emulation, virtqueue descriptor chain processing.
   This is section 6 from the original design doc.

2. **virtio-fs device** — implements the virtio-fs spec on top of
   the MMIO transport. Passes FUSE messages between guest and host.

3. **FUSE protocol handler** — `fuse-backend-rs` crate parses FUSE
   requests and gives us typed Rust handlers to implement.

4. **S3 filesystem backend** — our implementation of the FUSE
   handlers that maps operations to S3 API calls. Inode table,
   metadata mapping, read/write dispatch.

5. **S3 client** — `rust-s3` crate for talking to MinIO/S3.

6. **Custom initramfs** — minimal init that mounts virtiofs as root
   and does switch_root. A few hundred lines of shell or a small
   static Rust binary.

## What We Don't Need to Build

- ext4 or any block filesystem
- Block device emulation (virtio-block)
- FUSE daemon in the guest
- Guest-side agents, services, or custom code
- Disk images or partition tables
- Write buffering, caching, or compaction
- File chunking or multipart assembly
- Snapshot/restore mechanism (the filesystem IS the state)

---

## External Introspection and File Injection

Because the filesystem IS S3, the host (and any external system with
bucket access) can read and modify the guest's files without any
guest cooperation. No SSH, no guest agent, no special protocol.

### Reading guest files from outside

```
aws s3 cp s3://vm-{id}/home/agent/code.py ./code.py
```

That's it. The file is an S3 object. Read it directly.

### Writing files into the guest from outside

```
External API:  PUT s3://vm-{id}/home/agent/uploaded.txt
       │
VMM notified:  (S3 event, poll, or direct API call to VMM)
       │
VMM updates:   add/update inode in table
       │
VMM sends:     FUSE_NOTIFY_INVAL_ENTRY on parent directory
       │
Guest kernel:  invalidates dentry cache, re-fetches on next access
       │
Guest process: sees the new file on next readdir() or stat()
```

The FUSE protocol supports host-initiated cache invalidation via
FUSE_NOTIFY_INVAL_ENTRY and FUSE_NOTIFY_INVAL_INODE. The host tells
the guest kernel "this path changed" and the kernel discards its
cached view. fuse-backend-rs supports sending these notifications.

### inotify from external writes

When a file is dropped into S3 and we push the cache invalidation,
the kernel re-fetches the directory listing and sees new entries.
This should trigger inotify events for processes watching that
directory. Needs verification that virtiofs cache invalidation
propagates to inotify, but the information flow is architecturally
sound — we know what changed, the kernel has the notification
mechanism.

### Why this matters

This is the killer feature of S3-backed storage. No other VMM
architecture allows this. Block device-backed VMs require mounting
the disk image (offline) or running a guest agent to access files.
FUSE passthrough requires host filesystem access. S3 backing means
the guest's files are a standard API call away from any system with
credentials.

For coding agent platforms:
- Upload files to the agent's workspace without SSH
- Read agent output without guest cooperation
- Monitor filesystem activity via S3 access logs
- Share files between VMs by cross-bucket references
- Backup/restore is just S3 bucket operations

---

## Implementation Stack

No S3-FUSE Rust crate exists. The individual pieces do:

| Component | Crate/Source | What it provides |
|-----------|-------------|-----------------|
| FUSE protocol | fuse-backend-rs v0.14 | FileSystem trait, Server, protocol parsing |
| virtiofs transport | fuse-backend-rs | Reader/Writer for virtio descriptor chains |
| virtio queue | virtio-queue (already a dep) | Shared types with fuse-backend-rs |
| Guest memory | vm-memory (already a dep) | Shared types with fuse-backend-rs |
| S3 client | rust-s3 or aws-sdk-s3 | S3 GET/PUT/HEAD/LIST/DELETE |
| S3 reference | mountpoint-s3 (Apache-2.0) | Study their FUSE-to-S3 mapping patterns |

### What we write

1. **virtio MMIO transport** (~200 lines) — register emulation,
   virtqueue processing. Shared by all virtio devices.

2. **virtio-fs MMIO device** (~100 lines) — wires virtio queues
   to fuse-backend-rs Server::handle_message().

3. **S3 FileSystem impl** (~8 methods on the FileSystem trait) —
   lookup, getattr, read, write, create, unlink, readdir, mkdir.
   Each method is 10-30 lines mapping to an S3 call.

4. **Inode table** (~100 lines) — in-memory HashMap from inode
   numbers to S3 keys, built from ListObjectsV2 on mount.

5. **Custom initramfs** — shell script that mounts virtiofs and
   does switch_root.

### What we don't write

- FUSE protocol parsing (fuse-backend-rs)
- virtio descriptor chain handling (fuse-backend-rs + virtio-queue)
- S3 API client (rust-s3)
- Guest kernel modules (virtiofs is upstream since 5.4)
- Block device emulation
- Filesystem journaling, fsck, block allocation

---

## Key Design Decisions Summary

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Guest filesystem | virtiofs | File-level ops, no block device, stock kernel support |
| Storage protocol | S3 API | Ubiquitous, MinIO for dev, any S3-compatible for prod |
| Storage granularity | Whole objects | Simple, no chunking, 100MB cap makes this viable |
| Permissions | S3 object metadata | 2KB per object, stores uid/gid/mode/timestamps |
| Bucket model | One bucket per VM | Isolation at storage level |
| FUSE handler | In-process in VMM | No external daemon, one process per VM |
| Write strategy | Synchronous PUT | Correct, simple, network overhead acceptable |
| Read caching | Guest page cache only | No host-side cache, avoid double-caching |
| Inode table | In-memory, rebuilt on mount | Simple, fast, no persistence needed |
| File size limit | 100MB | Eliminates chunking complexity |
| Hardlinks | ENOTSUP | Not needed for agent workflows |
| inotify | Future work | We have the information, plumbing TBD |
| Golden images | Template bucket + CoW reads | Object-level CoW via bucket fallback |
| External access | Direct S3 read/write | No guest agent, no SSH, files are just S3 objects |
| Cache invalidation | FUSE_NOTIFY_INVAL_ENTRY | Host pushes changes into guest kernel cache |

## References

- virtiofs spec: https://virtio-fs.gitlab.io/
- FUSE protocol: https://libfuse.github.io/doxygen/
- fuse-backend-rs: https://github.com/cloud-hypervisor/fuse-backend-rs
- virtiofsd (Rust): https://gitlab.com/virtio-fs/virtiofsd
- mountpoint-s3: https://github.com/awslabs/mountpoint-s3 (S3-to-FUSE reference)
- rust-s3: lightweight S3 client crate for Rust
- Kata Containers: production user of virtiofs as rootfs
- Apache Iceberg: inspiration for append-only + manifest data model
- S3 user metadata: 2KB limit per object, x-amz-meta-* headers
