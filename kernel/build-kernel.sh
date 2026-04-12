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

# ── Phase 4: kernel slimming ────────────────────────────────────
# Disable subsystems we don't need in a microVM.
# Keep: EXT4, FUSE, VIRTIO_FS, PROC, SYSFS, TMPFS, DEVTMPFS,
#       basic IPV4+IPV6, virtio-net/-fs/-console transport.
# See research/09-distro-audit.md Phase 4.

echo "==> Disabling unused subsystems..."

# Filesystems (keep only what we need)
for opt in XFS_FS F2FS_FS REISERFS_FS JFS_FS \
           NFS_FS NFSD NFS_V2 NFS_V3 NFS_V4 NFS_V4_1 NFS_V4_2 \
           CIFS CIFSD SMB_SERVER \
           CEPH_FS CEPH_FSCACHE \
           NET_9P 9P_FS \
           HFS_FS HFSPLUS_FS \
           UDF_FS ISO9660_FS \
           SQUASHFS \
           OVERLAY_FS \
           AUTOFS_FS AUTOFS4_FS; do
    scripts/config --disable "CONFIG_$opt"
done

# Block layer (no SCSI/ATA/RAID/DM in our virtio-only setup)
for opt in SCSI SCSI_ISCSI_ATTRS ISCSI_TCP ISCSI_BOOT_SYSFS \
           ATA SATA_AHCI SATA_AHCI_PLATFORM \
           MD MD_RAID0 MD_RAID1 MD_RAID10 MD_RAID456 \
           BLK_DEV_MD BLK_DEV_DM \
           DM_MULTIPATH DM_CRYPT DM_THIN_PROVISIONING \
           BLK_DEV_NBD BLK_DEV_RAM; do
    scripts/config --disable "CONFIG_$opt"
done

# Security LSMs — guest is single-root, no enforcement needed
for opt in SECURITY_SELINUX SECURITY_SELINUX_BOOTPARAM \
           SECURITY_SELINUX_DEVELOP SECURITY_SELINUX_AVC_STATS \
           SECURITY_APPARMOR SECURITY_APPARMOR_HASH \
           SECURITY_SMACK SECURITY_TOMOYO \
           SECURITY_LOCKDOWN_LSM SECURITY_LOCKDOWN_LSM_EARLY \
           SECURITY_YAMA SECURITY_LOADPIN SECURITY_SAFESETID \
           INTEGRITY INTEGRITY_SIGNATURE IMA EVM; do
    scripts/config --disable "CONFIG_$opt"
done

# Networking extras (keep IPV4, IPV6 basics, and virtio-net path)
for opt in WIRELESS CFG80211 MAC80211 \
           BT BT_BREDR BT_LE \
           IEEE802154 \
           RDS TIPC DCCP IP_SCTP \
           IPV6_MROUTE IPV6_SEG6_LWTUNNEL IPV6_SEG6_HMAC \
           MPTCP MPTCP_IPV6 \
           NET_DSA NET_SWITCHDEV; do
    scripts/config --disable "CONFIG_$opt"
done

# Hardware we don't have
for opt in USB_SUPPORT USB \
           HID_SUPPORT HID HIDRAW \
           INPUT INPUT_EVDEV INPUT_KEYBOARD INPUT_MOUSE INPUT_JOYSTICK \
           DRM DRM_KMS_HELPER FB FRAMEBUFFER_CONSOLE \
           SOUND SND \
           MEDIA_SUPPORT \
           SURFACE_AGGREGATOR \
           FIREWIRE \
           PRINTER PARPORT; do
    scripts/config --disable "CONFIG_$opt"
done

# Power/thermal/watchdog (no hardware control needed in VM)
for opt in WATCHDOG WATCHDOG_CORE \
           THERMAL THERMAL_NETLINK \
           HWMON \
           SUSPEND HIBERNATION PM_AUTOSLEEP \
           ACPI_VIDEO ACPI_HOTPLUG_CPU ACPI_HOTPLUG_MEMORY \
           CPU_FREQ_GOV_USERSPACE CPU_FREQ_GOV_ONDEMAND \
           CPU_FREQ_GOV_CONSERVATIVE CPU_FREQ_GOV_SCHEDUTIL; do
    scripts/config --disable "CONFIG_$opt"
done

# Run olddefconfig to resolve any dependency changes
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
