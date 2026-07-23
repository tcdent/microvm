#!/bin/bash
# Empty an S3 bucket. Used to reset per-VM state between bench runs
# so the ruleset/deferred-writes/etc. benchmarks run against a known
# empty starting point.
#
# Usage: dist/reset-bucket.sh [bucket] [endpoint]
#   defaults: vm-dev on http://192.168.88.15:9090

BUCKET="${1:-vm-dev}"
ENDPOINT="${2:-http://192.168.88.15:9090}"

export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-minioadmin}"
export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-minioadmin}"

echo "Clearing s3://$BUCKET @ $ENDPOINT ..."
aws --endpoint-url "$ENDPOINT" s3 rm "s3://$BUCKET" --recursive 2>&1 | tail -1
