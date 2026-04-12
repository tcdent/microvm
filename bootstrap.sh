#!/bin/bash
# Bootstrap and run a development microVM instance.
#
# Notes:
# - Kernel cmdline includes systemd.unit=multi-user.target to skip graphical target
#
# Stand-in for the orchestration layer. Handles:
# - Ensuring the macvtap interface exists
# - Ensuring the VM's S3 bucket exists (copies from template if needed)
# - Launching the VMM with the right arguments
#
# Usage: ./bootstrap.sh [--rebuild] [template-bucket]
#
set -e

REBUILD=0
if [ "$1" = "--rebuild" ]; then
    REBUILD=1
    shift
fi

VM_BUCKET="${VM_BUCKET:-vm-dev}"
VM_NET="${VM_NET:-microvm0}"
VM_HOST_IFACE="${VM_HOST_IFACE:-enp97s0f0}"
VM_MAC="${VM_MAC:-06:00:AC:10:FF:01}"
VM_S3_ENDPOINT="${VM_S3_ENDPOINT:-http://192.168.88.15:9090}"
VM_KERNEL="${VM_KERNEL:-dist/vmlinux}"
VM_INITRD="${VM_INITRD:-dist/initramfs.cpio.gz}"
VM_MEMORY="${VM_MEMORY:-1024}"
VM_READ_AHEAD="${VM_READ_AHEAD:-16}"
VM_CACHE_TTL="${VM_CACHE_TTL:-300}"

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

# --- Ensure VM bucket exists (rebuild if requested) ---
if [ "$REBUILD" -eq 1 ]; then
    echo "Rebuilding rootfs (--rebuild)..."
    aws $EP s3 rb "s3://$VM_BUCKET" --force 2>/dev/null || true
    sudo -E dist/bootstrap-rootfs.sh trixie "$(date +%Y%m%d)" "$VM_BUCKET"
elif aws $EP s3api head-bucket --bucket "$VM_BUCKET" 2>/dev/null; then
    echo "VM bucket $VM_BUCKET exists"
else
    echo "Creating VM bucket $VM_BUCKET via bootstrap..."
    sudo -E dist/bootstrap-rootfs.sh trixie "$(date +%Y%m%d)" "$VM_BUCKET"
fi

# --- Launch the VMM ---
echo "Booting microVM..."
echo "  kernel:  $VM_KERNEL"
echo "  initrd:  $VM_INITRD"
echo "  memory:  ${VM_MEMORY}MB"
echo "  rootfs:  s3://$VM_BUCKET"
echo "  net:     $VM_NET"
echo "  s3:      $VM_S3_ENDPOINT"
echo "  log:     /tmp/microvm.log"
echo ""

cd microvm
export VM_KERNEL="../$VM_KERNEL"
export VM_INITRD="../$VM_INITRD"
export VM_NET VM_MEMORY VM_S3_ENDPOINT VM_READ_AHEAD VM_CACHE_TTL
exec cargo run --release
