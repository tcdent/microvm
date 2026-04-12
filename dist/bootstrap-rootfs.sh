#!/bin/bash
# Bootstrap a Debian rootfs and upload to S3 with POSIX metadata.
#
# Usage: sudo -E ./bootstrap-rootfs.sh [suite] [snapshot-date] [bucket] [s3-endpoint]
#
# Naming convention:
#   template-debian-trixie-20260407  — pinned golden image
#   vm-{instance-id}                 — per-VM, derives from template
#
# Requires: debootstrap, s3-upload (Rust binary in dist/s3-upload/)
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
# Networking
PACKAGES="$PACKAGES,iproute2,iputils-ping,curl,wget,ca-certificates,bind9-dnsutils,net-tools,systemd-timesyncd,isc-dhcp-client"
# Development
PACKAGES="$PACKAGES,git,python3,build-essential,openssh-client"
# Benchmarks / tools
PACKAGES="$PACKAGES,vim-tiny,apt-transport-https,iperf3"

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
/usr/sbin/debootstrap --variant=minbase --include="$PACKAGES" --cache-dir="$CACHE_DIR" "$SUITE" "$WORK" "$MIRROR"

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

# ── Upload (single pass) ───────────────────────────────────────

echo "==> Creating S3 bucket $BUCKET ..."
aws --endpoint-url "$ENDPOINT" s3 mb "s3://$BUCKET" 2>/dev/null || true

echo "==> Uploading rootfs to s3://$BUCKET ..."

UPLOADER="$DIR/s3-upload/target/release/s3-upload"
echo "    Looking for uploader at: $UPLOADER (exists: $(test -x "$UPLOADER" && echo yes || echo no))"
if [ -x "$UPLOADER" ]; then
    "$UPLOADER" "$WORK" "$BUCKET" "$ENDPOINT"
else
    echo "    (s3-upload not built — using aws cli, this will be slow)"
    echo "    Build it: cd dist/s3-upload && cargo build --release"
    find "$WORK" \( -type f -o -type l -o -type d \) -print0 | while IFS= read -r -d '' entry; do
        rel="${entry#$WORK/}"
        [ -z "$rel" ] && continue
        if [ -d "$entry" ]; then
            key="${rel}/"
            aws --endpoint-url "$ENDPOINT" s3api put-object \
                --bucket "$BUCKET" --key "$key" --content-length 0 \
                --metadata "mode=$(stat -c '%a' "$entry"),uid=$(stat -c '%u' "$entry"),gid=$(stat -c '%g' "$entry")" \
                >/dev/null 2>&1 || true
        elif [ -L "$entry" ]; then
            aws --endpoint-url "$ENDPOINT" s3api put-object \
                --bucket "$BUCKET" --key "$rel" --content-length 0 \
                --metadata "mode=120777,uid=0,gid=0,target=$(readlink "$entry")" \
                >/dev/null 2>&1 || true
        elif [ -f "$entry" ]; then
            aws --endpoint-url "$ENDPOINT" s3api put-object \
                --bucket "$BUCKET" --key "$rel" --body "$entry" \
                --metadata "mode=$(stat -c '%a' "$entry"),uid=$(stat -c '%u' "$entry"),gid=$(stat -c '%g' "$entry")" \
                >/dev/null 2>&1 || true
        fi
    done
fi

# Cleanup
rm -rf "$WORK"

echo "==> Done. Rootfs uploaded to s3://$BUCKET"
aws --endpoint-url "$ENDPOINT" s3 ls "s3://$BUCKET/" --recursive | wc -l | xargs -I{} echo "    {} objects"
