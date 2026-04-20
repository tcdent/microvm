#!/bin/bash
# Bootstrap a golden bucket for a production microd deployment.
#
# Reads /etc/microd/config.ini (or --config FILE) to get the S3
# endpoint, admin credentials, and target golden_bucket name, then
# chains debootstrap + golden-upload to build and upload a fresh
# golden image.
#
# Usage:
#   sudo bootstrap-golden.sh [--config /path/to/config.ini] [--suite trixie]
#
# Requires:
#   - debootstrap (from apt)
#   - dist/golden-upload binary built and on PATH (or at
#     /usr/local/bin/golden-upload if installed alongside microd)
#   - Source repo accessible for dist/bootstrap-rootfs.sh (which does
#     the actual debootstrap + upload work).
#
# Pattern:
#   - On one host: build + upload the golden bucket.
#   - On many hosts: point microd at the shared S3 endpoint + bucket.
#   Most deployments do bootstrap once per image refresh, not per
#   host.

set -euo pipefail

CONFIG="/etc/microd/config.ini"
SUITE="trixie"
SNAPSHOT=""

while [ $# -gt 0 ]; do
    case "$1" in
        --config) CONFIG="$2"; shift 2 ;;
        --suite)  SUITE="$2"; shift 2 ;;
        --snapshot) SNAPSHOT="$2"; shift 2 ;;
        --help|-h)
            sed -n '2,25p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "unknown flag: $1" >&2
            exit 1
            ;;
    esac
done

if [ ! -r "$CONFIG" ]; then
    echo "error: cannot read $CONFIG (use --config to point elsewhere)" >&2
    exit 1
fi

# Minimal INI extraction — pulls key from the [s3] and [defaults]
# sections without needing a real parser.
ini_val() {
    local section="$1"
    local key="$2"
    awk -v s="[$section]" -v k="$key" '
        $0 == s { in_section = 1; next }
        /^\[/ { in_section = 0 }
        in_section && $1 == k && $2 == "=" { print $3; exit }
    ' "$CONFIG"
}

ENDPOINT=$(ini_val s3 endpoint)
AWS_ACCESS_KEY_ID=$(ini_val s3 admin_access_key)
AWS_SECRET_ACCESS_KEY=$(ini_val s3 admin_secret_key)
BUCKET=$(ini_val defaults golden_bucket)

if [ -z "$ENDPOINT" ] || [ -z "$BUCKET" ]; then
    echo "error: config missing [s3] endpoint or [defaults] golden_bucket" >&2
    exit 1
fi

if [ "$AWS_ACCESS_KEY_ID" = "CHANGEME" ] || [ -z "$AWS_ACCESS_KEY_ID" ]; then
    echo "error: [s3] admin_access_key is unset or still CHANGEME" >&2
    exit 1
fi

# Find bootstrap-rootfs.sh — either in the repo or next to us.
if [ -x "$(dirname "$0")/../bootstrap-rootfs.sh" ]; then
    BOOTSTRAP_SCRIPT="$(dirname "$0")/../bootstrap-rootfs.sh"
elif command -v bootstrap-rootfs.sh >/dev/null 2>&1; then
    BOOTSTRAP_SCRIPT="$(command -v bootstrap-rootfs.sh)"
else
    echo "error: bootstrap-rootfs.sh not found (expected at dist/ or on PATH)" >&2
    exit 1
fi

if [ -z "$SNAPSHOT" ]; then
    SNAPSHOT=$(date +%Y%m%d)
fi

echo "bootstrap-golden: SUITE=$SUITE  SNAPSHOT=$SNAPSHOT"
echo "                  BUCKET=$BUCKET"
echo "                  ENDPOINT=$ENDPOINT"

export AWS_ACCESS_KEY_ID
export AWS_SECRET_ACCESS_KEY

# bootstrap-rootfs.sh takes positional args: suite, snapshot, bucket, endpoint.
sudo -E "$BOOTSTRAP_SCRIPT" "$SUITE" "$SNAPSHOT" "$BUCKET" "$ENDPOINT"

echo
echo "bootstrap-golden: done. Golden bucket '$BUCKET' on '$ENDPOINT' is ready."
echo "To activate, ensure /etc/microd/config.ini has:"
echo "  [defaults]"
echo "  golden_bucket = $BUCKET"
echo "Then: systemctl restart microd"
