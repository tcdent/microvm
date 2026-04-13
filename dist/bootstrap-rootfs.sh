#!/bin/bash
# Bootstrap a Debian rootfs and upload to S3 with POSIX metadata.
#
# Usage: sudo -E ./bootstrap-rootfs.sh [suite] [snapshot-date] [bucket] [s3-endpoint]
#
# Naming convention:
#   template-debian-trixie-20260407  — pinned golden image
#   vm-{instance-id}                 — per-VM, derives from template
#
# Requires: debootstrap, golden-upload (Rust binary in dist/golden-upload/)
# Run with: sudo -E (preserves AWS env vars)
#
set -e

SUITE="${1:-trixie}"
SNAPSHOT_DATE="${2:-$(date +%Y%m%d)}"
BUCKET="${3:-template-debian-${SUITE}-${SNAPSHOT_DATE}}"
MIRROR="${DEBOOTSTRAP_MIRROR:-http://deb.debian.org/debian}"
ENDPOINT="${4:-http://192.168.88.15:9090}"
DIR="$(cd "$(dirname "$0")" && pwd)"

# Packages to include in the rootfs beyond minbase.
# Core system
PACKAGES="systemd-sysv,procps,less,sudo,locales"
# Networking — drop bind9-dnsutils (use upstream DNS, no need for dig),
# net-tools (iproute2 covers it), apt-transport-https (apt 2.0+ has TLS)
PACKAGES="$PACKAGES,iproute2,iputils-ping,curl,wget,ca-certificates,isc-dhcp-client,systemd-timesyncd"
# Development
PACKAGES="$PACKAGES,git,python3,build-essential,openssh-client"
# Tools
PACKAGES="$PACKAGES,iperf3"

# Packages we don't want pulled in even as dependencies / recommends.
# Notes:
# - systemd-timesyncd: KEPT — without NTP the guest clock stays at the
#   kernel build date, breaking TLS (certs not yet valid). The ~1MB
#   isn't worth the breakage. See 09-distro-audit.md.
# - systemd-resolved: dropped, we use dhclient DNS directly
# - vim-tiny: agent doesn't need an editor
# - apt-transport-https: apt 2.0+ has built-in TLS
# - debconf-i18n: non-English package config translations
EXCLUDES="systemd-resolved,vim-tiny,apt-transport-https,debconf-i18n"

export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-minioadmin}"
export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-minioadmin}"

# Verify S3 is reachable
aws --endpoint-url "$ENDPOINT" s3 ls >/dev/null 2>&1 || {
    echo "Error: can't connect to S3 at $ENDPOINT"
    exit 1
}

if [ "$(id -u)" -ne 0 ]; then
    echo "Error: debootstrap requires root. Run with: sudo -E $0 $*"
    exit 1
fi

WORK=$(mktemp -d)
echo "==> Bootstrapping Debian $SUITE into $WORK ..."
echo "    Mirror: $MIRROR"
echo "    Bucket: $BUCKET"

CACHE_DIR="${DIR}/.debootstrap-cache"
mkdir -p "$CACHE_DIR"
/usr/sbin/debootstrap \
    --variant=minbase \
    --include="$PACKAGES" \
    --exclude="$EXCLUDES" \
    --cache-dir="$CACHE_DIR" \
    "$SUITE" "$WORK" "$MIRROR"

# ── Purge transitively-pulled packages we don't want ───────────
# These come in via dependencies. dpkg --purge is safe in chroot.
# NOTE: libapparmor1 is REQUIRED by systemd binary (linked at compile time).
#       Removing it causes /sbin/init to fail at "cannot open shared object".
#       Keep libapparmor1, only remove the userspace apparmor tools+profiles.
echo "==> Purging unneeded packages..."
PURGE_PKGS="apparmor libfido2-1 libcbor0.10 sqv gnupg gpgv gnupg-l10n"
for pkg in $PURGE_PKGS; do
    chroot "$WORK" dpkg --purge --force-depends "$pkg" 2>/dev/null || true
done

# ── microVM configuration (applied before upload) ──────────────

echo "==> Configuring rootfs for microVM..."

# Interactive console on hvc0 (virtio-console) — no FIFO limits, supports paste
mkdir -p "$WORK/etc/systemd/system"
cat > "$WORK/etc/systemd/system/serial-console.service" << 'GETTY'
[Unit]
Description=Console on hvc0 (virtio-console)
After=systemd-user-sessions.service

[Service]
ExecStart=/sbin/agetty --autologin root --noclear hvc0 115200 linux
Restart=always
RestartSec=0
StandardInput=tty
StandardOutput=tty
TTYPath=/dev/hvc0
TTYReset=yes
TTYVHangup=yes
UtmpIdentifier=ttyS0
UtmpMode=login

[Install]
WantedBy=multi-user.target
GETTY

# Enable it via .wants directory (no % in filename)
mkdir -p "$WORK/etc/systemd/system/multi-user.target.wants"
ln -sf /etc/systemd/system/serial-console.service \
    "$WORK/etc/systemd/system/multi-user.target.wants/serial-console.service"

# Set default target to multi-user (no graphical)
ln -sf /lib/systemd/system/multi-user.target \
    "$WORK/etc/systemd/system/default.target"

# Mask stock serial-getty templates — they wait 90s for udev (no udev in our VM)
ln -sf /dev/null "$WORK/etc/systemd/system/serial-getty@ttyS0.service"
ln -sf /dev/null "$WORK/etc/systemd/system/serial-getty@hvc0.service"

# Allow root login without password (dev only)
sed -i 's/^root:\*:/root::/' "$WORK/etc/shadow" 2>/dev/null || true

# Network: DHCP on eth0 via simple oneshot service
# systemd-networkd doesn't detect eth0 without udev, so we use a direct script
# dhclient handles IP, gateway, and DNS resolver
cat > "$WORK/etc/systemd/system/network-setup.service" << 'NETCFG'
[Unit]
Description=Configure network
After=systemd-modules-load.service
Wants=network.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'ip link set eth0 up && dhclient eth0'

[Install]
WantedBy=multi-user.target
NETCFG
ln -sf /etc/systemd/system/network-setup.service \
    "$WORK/etc/systemd/system/multi-user.target.wants/network-setup.service"


# Set hostname
echo "microvm" > "$WORK/etc/hostname"

echo "    serial console: ttyS0 (autologin root)"
echo "    default target: multi-user.target"

# ── Strip bloat (Phase 1 cleanup) ──────────────────────────────
# Saves ~360MB. Done after debootstrap, before upload.

echo "==> Stripping bloat..."

# apt cache and metadata — we don't apt-install in the running VM
rm -rf "$WORK/var/cache/apt/archives"/*.deb 2>/dev/null
rm -rf "$WORK/var/cache/apt/archives/partial" 2>/dev/null
rm -rf "$WORK/var/lib/apt/lists"/* 2>/dev/null
rm -rf "$WORK/var/lib/apt/lists/partial" 2>/dev/null

# Documentation — no humans reading docs in an agent VM
rm -rf "$WORK/usr/share/doc"/* 2>/dev/null
rm -rf "$WORK/usr/share/man"/* 2>/dev/null
rm -rf "$WORK/usr/share/info"/* 2>/dev/null

# Locale data — keep en_US only
find "$WORK/usr/share/locale" -mindepth 1 -maxdepth 1 -type d \
    ! -name 'en_US' -exec rm -rf {} + 2>/dev/null
rm -rf "$WORK/usr/share/i18n/locales" 2>/dev/null

# ── Drop broken/unneeded systemd unit files ────────────────────
# These either fail (no kmod, no udev) or serve no purpose (no console)

echo "==> Removing unneeded systemd units..."

# Stock serial-getty templates wait 90s for udev events that never come
rm -f "$WORK/lib/systemd/system/serial-getty@.service" 2>/dev/null

# Modprobe units — kernel modules are all built-in, no /sbin/modprobe
rm -f "$WORK/lib/systemd/system"/modprobe@*.service 2>/dev/null

# ldconfig.service — Phase 2: investigate the FUSE failure first before removing.
# Keeping it in for now so we can capture the actual error.

# Journal catalog rebuild — we don't use journalctl --catalog
rm -f "$WORK/lib/systemd/system/systemd-journal-catalog-update.service" 2>/dev/null

# Update-done — writes a stamp file we don't care about
rm -f "$WORK/lib/systemd/system/systemd-update-done.service" 2>/dev/null

# tty1-tty6 getty — no consoles attached, only hvc0 is real
# Remove the wants symlinks; the template stays for hvc0 if needed
rm -f "$WORK/etc/systemd/system/getty.target.wants/getty@tty1.service" 2>/dev/null
rm -f "$WORK/lib/systemd/system/getty.target.wants/getty@tty1.service" 2>/dev/null
# The default debian autovt enables tty1 via getty.target.wants — also remove
rm -f "$WORK/lib/systemd/system/autovt@.service" 2>/dev/null

# ── Phase 2: drop scheduled-maintenance timers and unneeded services ──

# Timers — all waste in an ephemeral VM:
#   apt-daily, apt-daily-upgrade — package metadata refresh / upgrade
#   dpkg-db-backup — backup dpkg state (we're stateless)
#   fstrim — discard unused fs blocks (not applicable to virtiofs/S3)
# Keep: systemd-tmpfiles-clean (cleans /tmp tmpfs, cheap)
rm -f "$WORK/etc/systemd/system/timers.target.wants/apt-daily.timer" 2>/dev/null
rm -f "$WORK/etc/systemd/system/timers.target.wants/apt-daily-upgrade.timer" 2>/dev/null
rm -f "$WORK/etc/systemd/system/timers.target.wants/dpkg-db-backup.timer" 2>/dev/null
rm -f "$WORK/etc/systemd/system/timers.target.wants/fstrim.timer" 2>/dev/null

# getty-static.service — explicit "if no dbus, spawn tty2-6" which we hit.
# Pulled in by getty.target.wants/getty-static.service
rm -f "$WORK/lib/systemd/system/getty.target.wants/getty-static.service" 2>/dev/null
rm -f "$WORK/lib/systemd/system/getty-static.service" 2>/dev/null

# systemd-modules-load.service — fails because we removed kmod. We have
# no loadable modules (everything is =y in our kernel).
rm -f "$WORK/lib/systemd/system/systemd-modules-load.service" 2>/dev/null
rm -f "$WORK/lib/systemd/system/sysinit.target.wants/systemd-modules-load.service" 2>/dev/null

# systemd-ask-password-wall.path — no passwords/FDE prompts. Drop.
rm -f "$WORK/lib/systemd/system/systemd-ask-password-wall.path" 2>/dev/null
rm -f "$WORK/lib/systemd/system/multi-user.target.wants/systemd-ask-password-wall.path" 2>/dev/null
rm -f "$WORK/lib/systemd/system/systemd-ask-password-wall.service" 2>/dev/null

# systemd-logind — session manager. Single root login on hvc0, no PAM
# session needed. If this breaks login, restore.
rm -f "$WORK/lib/systemd/system/systemd-logind.service" 2>/dev/null
rm -f "$WORK/lib/systemd/system/multi-user.target.wants/systemd-logind.service" 2>/dev/null
rm -f "$WORK/lib/systemd/system/dbus-org.freedesktop.login1.service" 2>/dev/null

# ── Upload to golden bucket ────────────────────────────────────
#
# Under the golden-FS model (research/12) the bootstrap tool
# populates a content-addressed golden bucket + a .manifest object.
# Per-VM buckets start empty and accumulate only the VM's deltas;
# they are NOT populated by this script.

echo "==> Creating S3 bucket $BUCKET ..."
aws --endpoint-url "$ENDPOINT" s3 mb "s3://$BUCKET" 2>/dev/null || true

echo "==> Populating golden bucket s3://$BUCKET ..."

UPLOADER="$DIR/golden-upload/target/release/golden-upload"
if [ ! -x "$UPLOADER" ]; then
    echo "ERROR: golden-upload binary not found at $UPLOADER"
    echo "       Build it first: cd dist/golden-upload && cargo build --release"
    echo "       (The old aws-cli fallback is gone — the golden-FS model"
    echo "        requires the BLAKE3-hashing + manifest-emitting tool.)"
    exit 1
fi
"$UPLOADER" "$WORK" "$BUCKET" "$ENDPOINT"

# Cleanup
rm -rf "$WORK"

echo "==> Done. Rootfs uploaded to s3://$BUCKET"
aws --endpoint-url "$ENDPOINT" s3 ls "s3://$BUCKET/" --recursive | wc -l | xargs -I{} echo "    {} objects"
