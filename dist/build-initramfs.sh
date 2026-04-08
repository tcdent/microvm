#!/bin/sh
# Build a minimal initramfs for our microVM.
#
# This initramfs does one thing: load virtio/virtiofs modules,
# mount the virtiofs root filesystem, and switch_root into it.
#
# The guest OS in the virtiofs root is completely unmodified.
# This initramfs is part of the VMM's boot payload, not the guest.
#
# Usage: ./build-initramfs.sh [modules-dir] [output-file]
#
# Requires: busybox-static (for the init shell and utilities)
#
set -e

MODULES_DIR="${1:-$(dirname "$0")/modules}"
OUTPUT="${2:-$(dirname "$0")/initramfs.cpio.gz}"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# Check for busybox
BUSYBOX=$(which busybox-static 2>/dev/null || which busybox 2>/dev/null)
if [ -z "$BUSYBOX" ]; then
    echo "Error: busybox-static not found. Install with: apt install busybox-static"
    exit 1
fi

echo "==> Building initramfs..."
echo "    Modules from: $MODULES_DIR"
echo "    Output: $OUTPUT"

# Create directory structure
for d in bin sbin dev proc sys run tmp sysroot lib/modules; do
    mkdir -p "$WORK/$d"
done

# Install busybox
cp "$BUSYBOX" "$WORK/bin/busybox"
chmod 755 "$WORK/bin/busybox"

# Create busybox symlinks for the commands we need
for cmd in sh mount umount switch_root insmod modprobe mkdir mknod \
           echo cat ls sleep; do
    ln -s busybox "$WORK/bin/$cmd"
done

# Copy kernel modules
if [ -d "$MODULES_DIR" ]; then
    cp "$MODULES_DIR"/*.ko "$WORK/lib/modules/" 2>/dev/null || true
    echo "    Modules included:"
    ls "$WORK/lib/modules/" 2>/dev/null | sed 's/^/      /'
else
    echo "    Warning: no modules directory found at $MODULES_DIR"
fi

# Create the init script
cat > "$WORK/init" << 'INIT_EOF'
#!/bin/sh
# microvm initramfs init
# Loads virtio modules, mounts virtiofs root, switches into it.
# All output goes to /dev/console (serial ttyS0).

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /run /tmp

# Redirect all output to the console so it appears on the serial port
exec > /dev/console 2>&1

echo "initramfs: starting"

# Load any kernel modules that aren't built-in
for mod in /lib/modules/*.ko; do
    [ -f "$mod" ] || continue
    insmod "$mod" 2>/dev/null && echo "initramfs: loaded $(basename $mod)"
done

# Give the virtio device a moment to appear
sleep 0.1

# Mount the virtiofs root filesystem
echo "initramfs: mounting virtiofs root..."
mount -t virtiofs rootfs /sysroot
echo "initramfs: ls /sysroot:"
ls -la /sysroot/
echo "initramfs: ls /sysroot/sbin:"
ls -la /sysroot/sbin 2>&1 || true
if [ $? -ne 0 ]; then
    echo "initramfs: ERROR: failed to mount virtiofs root"
    echo "initramfs: available filesystems:"
    cat /proc/filesystems
    echo "initramfs: dropping to emergency shell"
    exec setsid /bin/sh -l < /dev/console > /dev/console 2>&1
fi

# Move pseudo-filesystem mounts into the new root
mount --move /proc /sysroot/proc || mount -t proc proc /sysroot/proc || echo "WARN: proc mount failed"
mount --move /sys /sysroot/sys || mount -t sysfs sysfs /sysroot/sys || echo "WARN: sys mount failed"
mount --move /dev /sysroot/dev || mount -t devtmpfs devtmpfs /sysroot/dev || echo "WARN: dev mount failed"
mkdir -p /sysroot/run /sysroot/tmp 2>/dev/null || true

echo "initramfs: switching to virtiofs root"
exec switch_root /sysroot /sbin/init
INIT_EOF
chmod 755 "$WORK/init"

# Create essential device nodes
mknod "$WORK/dev/console" c 5 1 2>/dev/null || true
mknod "$WORK/dev/null" c 1 3 2>/dev/null || true
mknod "$WORK/dev/zero" c 1 5 2>/dev/null || true
mknod "$WORK/dev/tty" c 5 0 2>/dev/null || true

# Pack it up
echo "==> Packing initramfs..."
cd "$WORK"
find . -print0 | cpio --null -oH newc --quiet 2>/dev/null | gzip -9 > "$OUTPUT"

echo "==> Done: $OUTPUT ($(du -h "$OUTPUT" | cut -f1))"
