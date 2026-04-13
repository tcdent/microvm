#!/usr/bin/env bash
# Boot-time benchmark for microvm.
#
# Launches the VM N times, captures:
#   T0        orchestrator-start  (from microvm stderr)
#   T1        golden-loaded       (from microvm stderr)
#   T2        vm-started          (from microvm stderr)
#   t_login   host clock when "login:" first appears on hvc0 console
#
# Reports per-iter + median+range:
#   orchestrator_init = T1 - T0      (excluded from boot number)
#   boot_time         = t_login - T2 (THE metric, research/12 §10)
#   total             = t_login - T0
#
# Invocation:
#   dist/bench-boot.sh [N] [golden|nogolden]
#
# The second arg flips VM_GOLDEN_BUCKET on/off so the same script
# runs both the pre-golden baseline and the golden configuration.
# Reuses bootstrap.sh's env-var contract (VM_BUCKET, VM_GOLDEN_BUCKET,
# VM_S3_ENDPOINT, etc.) — set them in the environment before
# invoking, or let the defaults apply.
#
# Exits nonzero on any iteration that doesn't produce all four
# timestamps (partial data is worse than no data for a bench).

set -euo pipefail

N="${1:-5}"
MODE="${2:-nogolden}"  # nogolden | golden
BINARY="${BINARY:-microvm/target/release/microvm}"
LOGIN_TIMEOUT="${LOGIN_TIMEOUT:-60}"  # seconds to wait for 'login:' before declaring failure

if [ ! -x "$BINARY" ]; then
    echo "ERROR: microvm binary not found at $BINARY" >&2
    echo "       Build it first: (cd microvm && cargo build --release)" >&2
    exit 1
fi

case "$MODE" in
    golden|nogolden) ;;
    *) echo "Usage: $0 [N] [golden|nogolden]" >&2; exit 1 ;;
esac

# Per-iteration temp dir for logs.
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

echo "==> bench-boot N=$N mode=$MODE binary=$BINARY"

declare -a OI_VALS BOOT_VALS TOTAL_VALS

for i in $(seq 1 "$N"); do
    echo "--- iter $i/$N ---"
    STDOUT="$WORK/iter-$i.stdout"
    STDERR="$WORK/iter-$i.stderr"

    # Env for the VM: disable / enable golden mode by setting (or not)
    # VM_GOLDEN_BUCKET. The microvm binary reads this and plumbs it
    # into FsConfig::golden. If unset, golden is disabled.
    if [ "$MODE" = "golden" ]; then
        export VM_GOLDEN_BUCKET="${VM_GOLDEN_BUCKET:-template-debian-trixie-golden}"
    else
        unset VM_GOLDEN_BUCKET
    fi

    # Launch in background, redirect stdout/stderr to files.
    "$BINARY" >"$STDOUT" 2>"$STDERR" &
    VM_PID=$!

    # Poll stdout for "login:" with a timeout. tail -f blocks, so we
    # loop with short sleeps and grep the file.
    DEADLINE=$(( $(date +%s) + LOGIN_TIMEOUT ))
    T_LOGIN_NS=""
    while [ "$(date +%s)" -lt "$DEADLINE" ]; do
        if grep -q 'login:' "$STDOUT" 2>/dev/null; then
            # Capture the host clock at the moment we observed 'login:'.
            # This is a coarse approximation — we poll at ~20ms cadence,
            # so the timestamp is up to one poll-interval late. For a
            # ~13s measurement that's <0.2% noise; acceptable.
            T_LOGIN_NS=$(date +%s%N)
            break
        fi
        sleep 0.02
    done

    # Kill the VM regardless of outcome — harness owns lifecycle.
    kill -KILL "$VM_PID" 2>/dev/null || true
    wait "$VM_PID" 2>/dev/null || true

    if [ -z "$T_LOGIN_NS" ]; then
        echo "FAIL iter $i: 'login:' not seen within ${LOGIN_TIMEOUT}s" >&2
        echo "--- stderr ---" >&2
        cat "$STDERR" >&2
        echo "--- stdout (last 40) ---" >&2
        tail -40 "$STDOUT" >&2
        exit 1
    fi

    # Extract T0/T1/T2 from stderr. All three must be present.
    T0_NS=$(awk '/^__BENCH_T0__/ {print $2; exit}' "$STDERR")
    T1_NS=$(awk '/^__BENCH_T1__/ {print $2; exit}' "$STDERR")
    T2_NS=$(awk '/^__BENCH_T2__/ {print $2; exit}' "$STDERR")

    for v in T0 T1 T2; do
        eval "val=\$${v}_NS"
        if [ -z "$val" ]; then
            echo "FAIL iter $i: $v timestamp missing from stderr" >&2
            cat "$STDERR" >&2
            exit 1
        fi
    done

    # Convert to seconds with ms precision. awk rather than bc to avoid
    # a dep; printf %.3f gives us millisecond resolution which is
    # adequate for 13s wall-clock measurements.
    OI_S=$(awk -v a="$T0_NS" -v b="$T1_NS" 'BEGIN{printf "%.3f", (b-a)/1e9}')
    BOOT_S=$(awk -v a="$T2_NS" -v b="$T_LOGIN_NS" 'BEGIN{printf "%.3f", (b-a)/1e9}')
    TOTAL_S=$(awk -v a="$T0_NS" -v b="$T_LOGIN_NS" 'BEGIN{printf "%.3f", (b-a)/1e9}')

    printf "    orchestrator_init=%ss  boot_time=%ss  total=%ss\n" \
        "$OI_S" "$BOOT_S" "$TOTAL_S"

    OI_VALS+=("$OI_S")
    BOOT_VALS+=("$BOOT_S")
    TOTAL_VALS+=("$TOTAL_S")
done

# Median + min + max for each metric. Sort on the shell side, feed
# the sorted list to awk for arithmetic. Simpler than trying to push
# variadic args into awk's ARGV.
summarize() {
    local label="$1"; shift
    # Sort numerically on the shell side; feed newline-delimited values
    # to awk on stdin. Avoids the ARGV-packing dance that broke the
    # previous version of this helper.
    printf '%s\n' "$@" | sort -g | awk -v label="$label" '
        {
            arr[NR] = $1
        }
        END {
            n = NR
            if (n == 0) { print label ": (no data)"; exit }
            mid = int((n+1)/2)
            if (n % 2 == 0) med = (arr[n/2] + arr[n/2+1]) / 2
            else med = arr[mid]
            printf "%-20s median=%.3fs  min=%.3fs  max=%.3fs  range=%.3fs\n", \
                label ":", med, arr[1], arr[n], arr[n] - arr[1]
        }
    '
}

echo ""
echo "==> results (N=$N, mode=$MODE)"
summarize "orchestrator_init" "${OI_VALS[@]}"
summarize "boot_time"         "${BOOT_VALS[@]}"
summarize "total"             "${TOTAL_VALS[@]}"
