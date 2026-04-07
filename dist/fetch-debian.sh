#!/bin/sh
# Fetch Debian Trixie (testing) netboot kernel, initrd, and kernel modules.
# Extracts vmlinux from bzImage and pulls virtiofs modules from the kernel package.
#
# Usage: ./fetch-debian.sh [output-dir]
#
# Produces:
#   vmlinuz       - compressed kernel (bzImage)
#   vmlinux       - uncompressed ELF kernel
#   initrd.gz     - Debian netinstaller initrd (for reference/testing)
#   modules/      - extracted kernel modules needed for our custom initramfs
#
set -e

RELEASE="trixie"
ARCH="amd64"
BASE="https://deb.debian.org/debian/dists/${RELEASE}/main/installer-${ARCH}/current/images/netboot/debian-installer/${ARCH}"
POOL="https://deb.debian.org/debian/pool/main/l/linux"
DIR="${1:-$(dirname "$0")}"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

echo "==> Downloading netboot kernel..."
curl -L -o "$DIR/vmlinuz" "$BASE/linux"

echo "==> Downloading netboot initrd..."
curl -L -o "$DIR/initrd.gz" "$BASE/initrd.gz"

echo "==> Extracting vmlinux from bzImage..."
if [ ! -x "$DIR/extract-vmlinux" ]; then
    echo "    Fetching extract-vmlinux from kernel source..."
    curl -sL "https://raw.githubusercontent.com/torvalds/linux/master/scripts/extract-vmlinux" \
        -o "$DIR/extract-vmlinux"
    chmod +x "$DIR/extract-vmlinux"
fi
"$DIR/extract-vmlinux" "$DIR/vmlinuz" > "$DIR/vmlinux"

echo "==> Determining kernel version..."
KVER=$(strings "$DIR/vmlinux" | grep -oP 'Linux version \K[^ ]+' | head -1)
echo "    Kernel version: $KVER"

echo "==> Downloading kernel package for modules..."
# Find the unsigned kernel image package for this version
PKG_NAME="linux-image-${KVER}-unsigned"
# The package version derives from the kernel version
# e.g., kernel 6.12.73+deb13-amd64 -> package version 6.12.73-1
PKG_VER=$(echo "$KVER" | sed 's/+deb[0-9]*-amd64//' | sed 's/$/-1/')
DEB_FILE="${PKG_NAME}_${PKG_VER}_${ARCH}.deb"
curl -L -o "$WORK/kernel.deb" "${POOL}/${DEB_FILE}" || {
    echo "    Could not download ${DEB_FILE}, trying signed variant..."
    PKG_NAME="linux-image-${KVER}"
    DEB_FILE="${PKG_NAME}_${PKG_VER}_${ARCH}.deb"
    curl -L -o "$WORK/kernel.deb" "${POOL}/${DEB_FILE}"
}

echo "==> Extracting kernel modules..."
mkdir -p "$DIR/modules"
cd "$WORK"
ar x kernel.deb
tar xf data.tar.* 2>/dev/null || tar xf data.tar 2>/dev/null

# Extract the specific modules we need for our custom initramfs
MODDIR="usr/lib/modules/$KVER/kernel"
for mod in \
    drivers/virtio/virtio.ko* \
    drivers/virtio/virtio_mmio.ko* \
    drivers/virtio/virtio_ring.ko* \
    fs/fuse/fuse.ko* \
    fs/fuse/virtiofs.ko* \
; do
    src=$(find . -path "*/$mod" 2>/dev/null | head -1)
    if [ -n "$src" ]; then
        # Decompress if needed and copy
        base=$(basename "$src" | sed 's/\.xz$//' | sed 's/\.zst$//')
        if echo "$src" | grep -q '\.xz$'; then
            xz -d -c "$src" > "$DIR/modules/$base"
        elif echo "$src" | grep -q '\.zst$'; then
            zstd -d -c "$src" > "$DIR/modules/$base"
        else
            cp "$src" "$DIR/modules/$base"
        fi
        echo "    $base"
    fi
done

# Also copy the modules.dep and related files for modprobe
MODINFODIR="usr/lib/modules/$KVER"
for f in modules.dep modules.dep.bin modules.alias modules.alias.bin modules.order modules.builtin; do
    if [ -f "$MODINFODIR/$f" ]; then
        cp "$MODINFODIR/$f" "$DIR/modules/"
    fi
done

cd "$DIR"
echo ""
echo "==> Done. Files in $DIR/"
ls -lh "$DIR/vmlinuz" "$DIR/vmlinux" "$DIR/initrd.gz"
echo ""
echo "Modules:"
ls -lh "$DIR/modules/"
