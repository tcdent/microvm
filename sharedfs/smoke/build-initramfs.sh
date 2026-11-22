#!/bin/sh
# Smoke initramfs: busybox only. Lists virtio devices, reads the
# sharedfs index straight out of guest physical memory via /dev/mem,
# prints what it finds, and reboots (which the VMM treats as exit).
#
# Usage: build-initramfs.sh OUTPUT.cpio.gz
set -e
OUT="${1:?output path}"
BB=$(command -v busybox-static || command -v busybox)
WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT
for d in bin dev proc sys; do mkdir -p "$WORK/$d"; done
cp "$BB" "$WORK/bin/busybox"
for c in sh mount echo cat ls devmem printf reboot sleep basename; do ln -s busybox "$WORK/bin/$c"; done
cat > "$WORK/init" <<'INIT'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
exec > /dev/console 2>&1
echo "smoke: begin"
for d in /sys/bus/virtio/devices/*; do
    [ -e "$d" ] || continue
    echo "smoke: $(basename $d) device=$(cat $d/device) vendor=$(cat $d/vendor)"
done
B=$((0xE0000000))
echo "smoke: magic $(devmem $B 32) $(devmem $((B+4)) 32)"
echo "smoke: version=$(devmem $((B+8)) 32) entry_cap=$(devmem $((B+16)) 32) table_cap=$(devmem $((B+20)) 32) live=$(devmem $((B+48)) 32)"
# Print LEN bytes of the name at ADDR. /dev/mem read() refuses this range
# (not kernel-mapped RAM) while mmap works, so go word by word via devmem.
name() {
    addr=$1; len=$2; out=""
    while [ $len -gt 0 ]; do
        w=$(devmem $addr 64); w=${w#0x}
        for i in 14 12 10 8 6 4 2 0; do
            [ $len -gt 0 ] || break
            out="$out$(printf "\\x${w:$i:2}")"; len=$((len-1))
        done
        addr=$((addr+8))
    done
    echo "$out"
}
E0=$((B+4096))
C=$(devmem $((E0+24)) 32)
i=0
while [ $((C)) -ne $((0xFFFFFFFF)) ] && [ $i -lt 16 ]; do
    E=$((E0 + C*384))
    NL=$(devmem $((E+52)) 16)
    echo "smoke: slot $((C)) parent=$(devmem $((E+16)) 32) mode=$(devmem $((E+40)) 32) size=$(devmem $((E+56)) 64) seq=$(devmem $E 32) name=$(name $((E+96)) $((NL)))"
    C=$(devmem $((E+28)) 32); i=$((i+1))
done
echo "smoke: end"
reboot -f
INIT
chmod 755 "$WORK/init"
( cd "$WORK" && find . -print0 | cpio --null -oH newc --quiet | gzip -9 ) > "$OUT"
echo "built $OUT ($(du -h "$OUT" | cut -f1))"
