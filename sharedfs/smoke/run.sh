#!/usr/bin/env bash
# Boot the April kernel with a busybox initramfs and a sharedfs-exported
# fixture directory; check that the guest sees the device and reads the
# index. Prints every guest "smoke:" line, then PASS/FAIL per check.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
BIN="${BINARY:-$ROOT/microvm/target/release/microvm}"
KERNEL="${KERNEL:-$ROOT/dist/vmlinux}"
WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/proj/src"
printf 'fn main() {}\n' > "$WORK/proj/src/main.rs"
printf 'hello\n' > "$WORK/proj/README.md"
ln -s README.md "$WORK/proj/link"
"$HERE/build-initramfs.sh" "$WORK/initramfs.cpio.gz" >/dev/null

cd "$ROOT"
set +e
VM_SHAREDFS="$WORK/proj" VM_KERNEL="$KERNEL" VM_INITRD="$WORK/initramfs.cpio.gz" VM_MEMORY=512 \
    timeout "${TIMEOUT:-60}" "$BIN" </dev/null >"$WORK/out" 2>"$WORK/err"
rc=$?
set -e
tr -d '\r' < "$WORK/out" > "$WORK/out.lf" && mv "$WORK/out.lf" "$WORK/out"
grep -a 'sharedfs:' "$WORK/out" "$WORK/err" | sed 's/^[^:]*://' | head -3 || true
grep -a 'smoke:' "$WORK/out" || { echo "no guest output (rc=$rc); last host lines:"; tail -20 "$WORK/out" "$WORK/err"; exit 1; }

fail=0
check() { if grep -aq "$2" "$WORK/out"; then echo "PASS $1"; else echo "FAIL $1"; fail=1; fi; }
check "device enumerated"      'device=0x5346'
check "index magic"            'magic 0x52414853 0x53464445'
check "format version"         'version=0x00000001'
check "live count 5"           'live=0x00000005'
check "root child README.md"   'name=README.md$'
check "root child link"        'mode=0x0000A1FF .* name=link$'
check "root child src"         'mode=0x000041ED .* name=src$'
if grep -a 'smoke: slot' "$WORK/out" | grep -aqv 'seq=0x[0-9A-F]*[02468] '; then echo "FAIL every seq even"; fail=1; else echo "PASS every seq even"; fi
if [ "$rc" -eq 0 ]; then echo "PASS clean guest exit"; else echo "FAIL clean guest exit (rc=$rc)"; fail=1; fi
exit $fail
