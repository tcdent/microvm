#!/bin/bash
# Build a microd .deb package for the current HEAD (or a given version).
#
# Usage:
#   ./packaging/build.sh                       # auto-detect version from git
#   ./packaging/build.sh --version 0.16.0      # explicit
#
# Output: packaging/out/microd_<version>_amd64.deb

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
OUT_DIR="$SCRIPT_DIR/out"

VERSION=""
while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,10p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "unknown flag: $1" >&2
            exit 1
            ;;
    esac
done

# Auto-detect version from most recent git tag matching v0.*.*, or fall
# back to a timestamp-based dev version.
if [ -z "$VERSION" ]; then
    LAST_TAG=$(cd "$REPO_ROOT" && git tag --list 'v0.*.*' --sort=-v:refname | head -1)
    if [ -n "$LAST_TAG" ]; then
        VERSION="${LAST_TAG#v}"
        COMMITS_AHEAD=$(cd "$REPO_ROOT" && git rev-list --count "${LAST_TAG}..HEAD")
        if [ "$COMMITS_AHEAD" -gt 0 ]; then
            SHA=$(cd "$REPO_ROOT" && git rev-parse --short HEAD)
            VERSION="${VERSION}+dev.${COMMITS_AHEAD}.${SHA}"
        fi
    else
        SHA=$(cd "$REPO_ROOT" && git rev-parse --short HEAD)
        VERSION="0.0.0+dev.${SHA}"
    fi
fi

echo "==> Building microd ${VERSION}"

# Build release binaries. Each crate has its own target/; cargo doesn't
# have a workspace Cargo.toml at the repo root.
echo "==> cargo build --release"
(cd "$REPO_ROOT/orchestrator"          && cargo build --release)
(cd "$REPO_ROOT/microctl"              && cargo build --release)
(cd "$REPO_ROOT/dist/golden-upload"    && cargo build --release)

# Stage the package tree.
STAGE="$SCRIPT_DIR/stage"
rm -rf "$STAGE"
mkdir -p "$STAGE"

# Copy DEBIAN/ metadata with @VERSION@ substituted in control.
cp -r "$SCRIPT_DIR/debian/microd/DEBIAN" "$STAGE/DEBIAN"
sed -i "s/@VERSION@/$VERSION/g" "$STAGE/DEBIAN/control"
chmod 0755 "$STAGE/DEBIAN/preinst" "$STAGE/DEBIAN/postinst" \
           "$STAGE/DEBIAN/prerm"   "$STAGE/DEBIAN/postrm"

# Binaries.
install -Dm 0755 "$REPO_ROOT/orchestrator/target/release/microd"        "$STAGE/usr/local/bin/microd"
install -Dm 0755 "$REPO_ROOT/microctl/target/release/microctl"          "$STAGE/usr/local/bin/microctl"
install -Dm 0755 "$REPO_ROOT/dist/golden-upload/target/release/golden-upload" "$STAGE/usr/local/bin/golden-upload"

# systemd unit.
install -Dm 0644 "$REPO_ROOT/dist/deploy/microd.service" "$STAGE/etc/systemd/system/microd.service"

# Config files (conffiles — preserved across upgrade).
install -Dm 0644 "$REPO_ROOT/dist/deploy/microd.ini"                      "$STAGE/etc/microd/config.ini"
install -Dm 0644 "$REPO_ROOT/s3-rootfs/rulesets/coding-agent.toml"        "$STAGE/etc/microd/coding-agent.toml"

# Ops helper scripts + README.
install -Dm 0755 "$REPO_ROOT/dist/deploy/bootstrap-golden.sh" "$STAGE/usr/local/share/microd/bootstrap-golden.sh"
install -Dm 0644 "$REPO_ROOT/dist/deploy/README.md"           "$STAGE/usr/local/share/microd/README"

# Build the .deb.
mkdir -p "$OUT_DIR"
DEB_NAME="microd_${VERSION}_amd64.deb"
dpkg-deb --build --root-owner-group "$STAGE" "$OUT_DIR/$DEB_NAME"

echo ""
echo "==> Built $OUT_DIR/$DEB_NAME"
echo ""
dpkg-deb --info "$OUT_DIR/$DEB_NAME" | sed 's/^/    /'
echo ""
echo "Install:"
echo "  sudo dpkg -i $OUT_DIR/$DEB_NAME"
