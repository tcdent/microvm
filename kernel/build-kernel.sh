#!/bin/sh
# Build a custom kernel for our microVM.
#
# Starts from Firecracker's microVM config and adds virtiofs support.
# Produces a vmlinux ELF that we load directly (no bzImage decompression).
#
# Usage: ./build-kernel.sh [kernel-version]
#
# Requires: build-essential, flex, bison, libelf-dev, libssl-dev
#
set -e

KVER="${1:-6.12.73}"
DIR="$(cd "$(dirname "$0")" && pwd)"
WORK="$DIR/src"
JOBS=$(nproc)

# Download kernel source if not present
if [ ! -d "$WORK/linux-$KVER" ]; then
    MAJOR=$(echo "$KVER" | cut -d. -f1)
    echo "==> Downloading kernel $KVER..."
    mkdir -p "$WORK"
    curl -L "https://cdn.kernel.org/pub/linux/kernel/v${MAJOR}.x/linux-${KVER}.tar.xz" \
        | tar xJ -C "$WORK"
fi

cd "$WORK/linux-$KVER"

# Start from our config
cp "$DIR/.config" .config

# Apply our customizations on top of Firecracker's config
scripts/config --enable CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES
scripts/config --enable CONFIG_FUSE_FS
scripts/config --enable CONFIG_VIRTIO_FS
scripts/config --enable CONFIG_X86_MPPARSE

# Run olddefconfig to resolve any new dependencies
make olddefconfig

# Verify our flags are set
echo "==> Verifying config..."
for opt in CONFIG_VIRTIO_MMIO CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES \
           CONFIG_FUSE_FS CONFIG_VIRTIO_FS; do
    val=$(grep "^${opt}=" .config || echo "NOT SET")
    echo "    $val"
done

# Build
echo "==> Building kernel with $JOBS cores..."
make -j"$JOBS" vmlinux

# Copy output
cp vmlinux "$DIR/../dist/vmlinux"
echo "==> Done: dist/vmlinux ($(du -h "$DIR/../dist/vmlinux" | cut -f1))"

# Also extract the virtio_mmio module if it's built as module
# (shouldn't be with our config, but just in case)
if [ -f drivers/virtio/virtio_mmio.ko ]; then
    cp drivers/virtio/virtio_mmio.ko "$DIR/../dist/modules/"
    echo "    Also copied virtio_mmio.ko"
fi
