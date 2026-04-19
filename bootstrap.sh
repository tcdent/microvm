#!/bin/bash
# Bootstrap and run a development microVM instance.
#
# Two-bucket golden-FS model (research/12):
#   - GOLDEN bucket: content-addressed (BLAKE3 blobs + .manifest), shared
#     across every VM. Built once by dist/bootstrap-rootfs.sh. Costly to
#     produce (debootstrap + ~450MB upload), cheap to reuse.
#   - PER-VM bucket: empty at boot, holds this VM's writes only.
#
# Defaults:
#   VM_GOLDEN_BUCKET=template-debian-trixie-golden
#   VM_BUCKET=vm-dev
#
# Per-VM lifecycle: recreated empty on each run. The golden bucket is
# the source of truth, per-VM is ephemeral scratch. Pass --persist to
# reuse an existing vm-dev across boots (for iterating on state that
# lives in the VM).
#
# Usage:
#   ./bootstrap.sh             boot; reset per-VM bucket; build golden if missing
#   ./bootstrap.sh --rebuild   rebuild golden bucket from scratch, then boot
#   ./bootstrap.sh --persist   reuse existing per-VM bucket instead of resetting
#
set -e

REBUILD=0
PERSIST=0
while [ $# -gt 0 ]; do
    case "$1" in
        --rebuild) REBUILD=1; shift ;;
        --persist) PERSIST=1; shift ;;
        *) echo "Unknown arg: $1" >&2; exit 1 ;;
    esac
done

VM_GOLDEN_BUCKET="${VM_GOLDEN_BUCKET:-template-debian-trixie-golden}"
VM_BUCKET="${VM_BUCKET:-vm-dev}"
VM_NET="${VM_NET:-microvm0}"
VM_HOST_IFACE="${VM_HOST_IFACE:-enp97s0f0}"
VM_MAC="${VM_MAC:-06:00:AC:10:FF:01}"
VM_S3_ENDPOINT="${VM_S3_ENDPOINT:-http://192.168.88.15:9090}"
VM_KERNEL="${VM_KERNEL:-dist/vmlinux}"
VM_INITRD="${VM_INITRD:-dist/initramfs.cpio.gz}"
VM_MEMORY="${VM_MEMORY:-1024}"

export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-minioadmin}"
export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-minioadmin}"
EP="--endpoint-url $VM_S3_ENDPOINT"

# --- Ensure kernel and initramfs exist ---
if [ ! -f "$VM_KERNEL" ] || [ ! -f "$VM_INITRD" ]; then
    echo "Missing $VM_KERNEL or $VM_INITRD"
    echo "Run: kernel/build-kernel.sh && dist/build-initramfs.sh"
    exit 1
fi

# --- Ensure macvtap interface exists ---
if ! ip link show "$VM_NET" >/dev/null 2>&1; then
    echo "Creating macvtap $VM_NET on $VM_HOST_IFACE..."
    sudo ip link add "$VM_NET" link "$VM_HOST_IFACE" type macvtap mode bridge
    sudo ip link set "$VM_NET" address "$VM_MAC" up
    TAPIDX=$(cat /sys/class/net/"$VM_NET"/ifindex)
    sudo chmod 666 /dev/tap"$TAPIDX"
    echo "  created /dev/tap$TAPIDX"
else
    echo "macvtap $VM_NET already exists"
fi

# --- Golden bucket: build or reuse ---
if [ "$REBUILD" -eq 1 ]; then
    echo "Rebuilding golden bucket $VM_GOLDEN_BUCKET (--rebuild)..."
    aws $EP s3 rm "s3://$VM_GOLDEN_BUCKET" --recursive 2>/dev/null || true
    aws $EP s3 rb "s3://$VM_GOLDEN_BUCKET" --force 2>/dev/null || true
    sudo -E dist/bootstrap-rootfs.sh trixie "$(date +%Y%m%d)" "$VM_GOLDEN_BUCKET" "$VM_S3_ENDPOINT"
elif aws $EP s3api head-bucket --bucket "$VM_GOLDEN_BUCKET" 2>/dev/null; then
    echo "Golden bucket $VM_GOLDEN_BUCKET exists"
else
    echo "Creating golden bucket $VM_GOLDEN_BUCKET (first boot)..."
    sudo -E dist/bootstrap-rootfs.sh trixie "$(date +%Y%m%d)" "$VM_GOLDEN_BUCKET" "$VM_S3_ENDPOINT"
fi

# --- Per-VM bucket: reset unless --persist ---
if [ "$PERSIST" -eq 1 ] && aws $EP s3api head-bucket --bucket "$VM_BUCKET" 2>/dev/null; then
    echo "Per-VM bucket $VM_BUCKET reused (--persist)"
else
    if aws $EP s3api head-bucket --bucket "$VM_BUCKET" 2>/dev/null; then
        echo "Resetting per-VM bucket $VM_BUCKET (empty for fresh boot)"
        aws $EP s3 rm "s3://$VM_BUCKET" --recursive >/dev/null 2>&1 || true
    else
        echo "Creating empty per-VM bucket $VM_BUCKET"
        aws $EP s3 mb "s3://$VM_BUCKET" >/dev/null
    fi
fi

# --- Launch the VMM ---
echo "Booting microVM..."
echo "  kernel:  $VM_KERNEL"
echo "  initrd:  $VM_INITRD"
echo "  memory:  ${VM_MEMORY}MB"
echo "  golden:  s3://$VM_GOLDEN_BUCKET"
echo "  per-vm:  s3://$VM_BUCKET"
echo "  net:     $VM_NET"
echo "  s3:      $VM_S3_ENDPOINT"
echo "  log:     /tmp/microvm.log"
echo ""

cd microvm
export VM_KERNEL="../$VM_KERNEL"
export VM_INITRD="../$VM_INITRD"
export VM_NET VM_MEMORY VM_S3_ENDPOINT VM_BUCKET VM_GOLDEN_BUCKET
exec cargo run --release
