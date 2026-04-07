#!/bin/sh
# Start a local MinIO instance for S3-compatible development storage
set -e

DATA_DIR="${1:-$(dirname "$0")/../.minio-data}"
mkdir -p "$DATA_DIR"

echo "Starting MinIO on http://localhost:9000"
echo "Console: http://localhost:9001"
echo "Access key: minioadmin"
echo "Secret key: minioadmin"
echo "Data dir: $DATA_DIR"

exec podman run --rm \
    --name microvm-minio \
    -p 9000:9000 \
    -p 9001:9001 \
    -v "$DATA_DIR:/data:z" \
    -e MINIO_ROOT_USER=minioadmin \
    -e MINIO_ROOT_PASSWORD=minioadmin \
    quay.io/minio/minio server /data --console-address ":9001"
