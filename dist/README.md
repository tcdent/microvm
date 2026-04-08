# dist — Build and bootstrap tools

Scripts for building the microVM's boot artifacts and populating
S3 with a root filesystem.

## Quick start

```bash
# 1. Build the kernel (one-time)
../kernel/build-kernel.sh

# 2. Build the initramfs (after kernel build)
./build-initramfs.sh

# 3. Bootstrap a Debian rootfs into S3 (needs root for debootstrap)
sudo ./bootstrap-rootfs.sh rootfs trixie http://localhost:9090

# 4. Boot the VM
cd ../microvm
cargo run -- ../dist/vmlinux ../dist/initramfs.cpio.gz [net-interface]
```

## Scripts

| Script | What it does |
|--------|-------------|
| `fetch-debian.sh` | Downloads Debian netboot kernel, initrd, extracts vmlinux and modules |
| `build-initramfs.sh` | Builds minimal initramfs with busybox + virtio_mmio module |
| `bootstrap-rootfs.sh` | Runs debootstrap, uploads entire rootfs to S3 with POSIX metadata |
| `extract-vmlinux` | Kernel script for extracting vmlinux from bzImage |

## S3 metadata format

Files uploaded by `bootstrap-rootfs.sh` include POSIX metadata in S3
object user metadata:

```
x-amz-meta-mode: 755        (octal permission bits)
x-amz-meta-uid:  0           (numeric user ID)
x-amz-meta-gid:  0           (numeric group ID)
x-amz-meta-target: /usr/bin  (symlink target, if applicable)
```

The `s3-rootfs` crate reads these on `lookup`/`HEAD` and returns
correct `stat` values to the guest kernel.

## Dependencies

- `debootstrap` — Debian bootstrap tool (`apt install debootstrap`)
- `aws` CLI — S3 operations (`apt install awscli`)
- `busybox-static` — for initramfs (`apt install busybox-static`)
- `curl` — downloading kernel artifacts
