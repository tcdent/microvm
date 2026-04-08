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
# Usage: ./bootstrap.sh [template-bucket]
#
set -e

VM_BUCKET="${1:-vm-dev}"
NET_IFACE="microvm0"
HOST_IFACE="eno1"
MAC="06:00:AC:10:FF:01"
ENDPOINT="http://localhost:9090"
KERNEL="dist/vmlinux"
INITRD="dist/initramfs.cpio.gz"

export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-minioadmin}"
export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-minioadmin}"
EP="--endpoint-url $ENDPOINT"

# --- Ensure kernel and initramfs exist ---
if [ ! -f "$KERNEL" ] || [ ! -f "$INITRD" ]; then
    echo "Missing $KERNEL or $INITRD"
    echo "Run: kernel/build-kernel.sh && dist/build-initramfs.sh"
    exit 1
fi

# --- Ensure macvtap interface exists ---
if ! ip link show "$NET_IFACE" >/dev/null 2>&1; then
    echo "Creating macvtap $NET_IFACE on $HOST_IFACE..."
    sudo ip link add "$NET_IFACE" link "$HOST_IFACE" type macvtap mode bridge
    sudo ip link set "$NET_IFACE" address "$MAC" up
    TAPIDX=$(cat /sys/class/net/"$NET_IFACE"/ifindex)
    sudo chmod 666 /dev/tap"$TAPIDX"
    echo "  created /dev/tap$TAPIDX"
else
    echo "macvtap $NET_IFACE already exists"
fi

# --- Ensure VM bucket exists ---
if aws $EP s3api head-bucket --bucket "$VM_BUCKET" 2>/dev/null; then
    echo "VM bucket $VM_BUCKET exists"
else
    echo "Creating VM bucket $VM_BUCKET via bootstrap..."
    sudo -E dist/bootstrap-rootfs.sh trixie "$(date +%Y%m%d)" "$VM_BUCKET"
fi

# --- Launch the VMM ---
echo "Booting microVM..."
echo "  kernel:  $KERNEL"
echo "  initrd:  $INITRD"
echo "  rootfs:  s3://$VM_BUCKET"
echo "  net:     $NET_IFACE"
echo "  log:     /tmp/microvm.log"
echo ""

cd microvm
exec cargo run -- "../$KERNEL" "../$INITRD" "$NET_IFACE"
