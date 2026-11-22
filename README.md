# microvm

A KVM virtual machine runtime built for persistent, interactive coding
agent environments.

Existing microVM stacks were designed for serverless workloads:
stateless, ephemeral, no restarts. Using them for coding agents means
bolting persistence and guest-side agents on afterwards. This project
designs the stack for the use case instead. The host provides storage,
networking and lifecycle; the guest is a Debian system that does not
need to know how any of it is backed.

The VMM is hand-written on top of the rust-vmm crates for KVM, guest
memory and kernel loading. Devices, the run loop and the filesystems
are ours. In the guest kernel we add Rust bindings for the file system
layer, extending the unmerged VFS abstractions RFC with what a writable,
DAX-backed file system needs, and implement our own file system,
sharedfs, on top of them.

## What is here

| Path | What it is |
|---|---|
| `microvm/` | The VMM: vCPUs, MP table, virtio-mmio transport, and the console, net, blk, virtio-fs and sharedfs devices. A library (`launcher::run`) and a standalone binary configured from `VM_*` environment variables. |
| `sharedfs/` | Host daemon for the shared project filesystem. A library with no VMM in it; the tests speak its protocol directly. |
| `kernel/` | The guest kernel: build script, config, and `patches/`, the Rust guest driver for sharedfs as a series on Linux 7.2.9. |
| `s3-rootfs/` | An S3-backed FUSE filesystem for VM root filesystems, with a content-addressed golden image shared across VMs. |
| `virtio-fs-transport/` | The virtqueue worker that serves a FUSE filesystem over virtio-fs. |
| `orchestrator/` | `microd`, the daemon that runs many VMs as threads in one process. |
| `microctl/` | Command-line client for `microd`: `start`, `stop`, `list`, `logs`. |
| `compat/` | A checker that exercises the KVM paths the VMM uses and reports whether a host can run it. |
| `dist/` | Scripts for boot artifacts and for bootstrapping a root filesystem into S3. See `dist/README.md`. |
| `packaging/` | Debian package build for `microd` and `microctl`. See `packaging/README.md`. |
| `research/` | Numbered design notes, in the order the work happened. |

## Two ways a guest gets its files

**Disks plus sharedfs.** The guest boots from a read-only environment
disk with a data disk and swap beside it, and the project being worked
on is a plain directory on the host, exported to the guest through
sharedfs. Design: `research/16`, `17`.

sharedfs is the reason for this shape. The host directory stays an
ordinary directory, not a block image, that editors and `git` on the
host use directly. The guest sees it as a native kernel filesystem:

- Metadata lives in a shared index that the host daemon alone writes
  and the guest maps read-only, so lookups, attributes and directory
  listings are memory reads with no round trip.
- File data is shared by reference. The daemon maps ranges of host
  files into a window the guest reaches through DAX, so the guest's
  view of a file is the host page cache and nothing is copied.
- Mutations go to the daemon over a virtqueue. Changes made on the host
  reach the guest as notifications that feed its own inotify.
- The index can persist across runs, which is what lets a guest
  hibernate and resume with its mounts and inode numbers intact.

The guest half is a filesystem written in Rust, which is why the guest
kernel is built here rather than taken from a distribution.

**The S3 root.** The guest's root filesystem is virtio-fs backed by S3
objects, with a golden image stored once and each VM's bucket holding
only its own writes. This is what `microd` launches today. Design:
`research/03`, `07`, `12`, `15`.

## Building

Each crate is its own Cargo project; there is no workspace.

```
cd sharedfs      && cargo test
cd microvm       && cargo build --release
cd orchestrator  && cargo build --release    # microd
cd microctl      && cargo build --release
```

`microvm` depends on `xdp-net` by path at `../../rust-vmm/xdp-net`,
outside this repository.

The guest kernel:

```
kernel/build-kernel.sh
```

It expects the 7.2.9 source at `kernel/src/linux-7.2.9` (untracked)
with the `kernel/patches` series applied, and a Rust-for-Linux
toolchain; `research/17` lists the versions. The script does not apply
the series itself.

## Running

The host needs KVM on x86_64. `compat/` gives a pass or fail for a
candidate host, and `research/05` covers which platforms work.

A development VM on the S3 root, including the macvtap interface and
the golden bucket on first run:

```
docker compose up -d      # local S3 (RustFS)
./bootstrap.sh
```

Many VMs under the daemon: `dist/deploy/README.md` and
`packaging/README.md`.

The standalone `microvm` binary reads its configuration from the
environment. The ones that select the shape of the guest:

| Variable | Meaning |
|---|---|
| `VM_KERNEL`, `VM_INITRD` | Boot artifacts. Default `dist/vmlinux`, `dist/initramfs.cpio.gz`. |
| `VM_MEMORY`, `VM_CPUS` | Guest RAM in MB and vCPU count. Default 1024 and 1. |
| `VM_NET`, `VM_MAC` | macvtap interface and the guest MAC, which must match it. |
| `VM_DISK`, `VM_DATA`, `VM_SWAP` | Backing files for `vda` (read-only), `vdb` and `vdc`. With any disk the guest boots without the S3 root. |
| `VM_SHAREDFS` | Exports, whitespace-separated, each `TAG=PATH[:OPTION]...`. An option is a window size such as `1G`, or `ro`. The guest mounts one with `mount -t sharedfs TAG /mnt`. |
| `VM_STATE` | Per-VM state directory: the log and the persisted sharedfs indexes. |
| `VM_CMDLINE` | Appended to the kernel command line. |
| `VM_BUCKET`, `VM_GOLDEN_BUCKET`, `VM_S3_ENDPOINT` | The S3 root. |

## Status

Under active development; nothing here promises a stable interface.
sharedfs passes its host-side tests and has been exercised end to end
from a guest, including hibernate and resume, by smoke scripts that
live outside this repository. Running it against the xfstests generic
group is the next definition of done (`research/16` §8).
