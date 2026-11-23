#!/bin/sh
# Makes `vendor/` from `patches/` and `linux/`: each source tree named in `patches/`,
# `<name>-<version>`, exactly as released, with every patch in its directory applied and our
# own files from `linux/` copied in at their paths.
#
# A patch is one change, named for what it is, and stands on its own: each is first tried
# against the tree as released, alone, so that none leans on another to apply. A tree whose
# patches have not changed since it was made is left alone. Our own files are brought up to
# date every run, each only where it differs, so a build after editing one compiles that one.
# `build-kernel.sh` builds the result.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
tag() { [ -f "$1/CACHEDIR.TAG" ] || echo 'Signature: 8a477f597d28d172789f06886806bc55' > "$1/CACHEDIR.TAG"; }
for series in patches/linux-[0-9]*/; do
  tree=$(basename "$series")
  made="vendor/$tree"
  stamp="$made/.patched"
  # Up to date where the stamp is newer than every patch of the series.
  if [ -f "$stamp" ] && [ -z "$(find "$series" -name '*.patch' -newer "$stamp")" ]; then
    continue
  fi
  version=${tree##*-}
  tarball="patches/pristine/$tree.tar.xz"
  mkdir -p patches/pristine vendor
  tag patches/pristine
  tag vendor
  if [ ! -f "$tarball" ]; then
    echo "apply.sh: fetching $tree"
    curl --fail --location --output "$tarball.part" \
      "https://cdn.kernel.org/pub/linux/kernel/v${version%%.*}.x/$tree.tar.xz"
    mv "$tarball.part" "$tarball"
  fi
  rm -rf "$made"
  tar --extract --xz --file "$tarball" --directory vendor
  # The tree is as released until the first patch goes on, so this is where each is tried alone.
  for patch in "$series"*.patch; do
    patch --quiet --dry-run --strip 1 --directory "$made" --input "$root/$patch" > /dev/null || {
      echo "apply.sh: $patch does not apply to $tree as released, on its own" >&2
      exit 1
    }
  done
  for patch in "$series"*.patch; do
    echo "patched: $tree: $(basename "$patch" .patch)"
    patch --quiet --no-backup-if-mismatch --strip 1 --directory "$made" --input "$root/$patch"
  done
  touch "$stamp"
done

# Our own files, at their paths in the tree. The list of what was copied last time says which
# to take away again when one is deleted or renamed here.
for made in vendor/linux-[0-9]*/; do
  [ -d linux ] || break
  ours="$made.ours"
  (cd linux && find . -type f | sort) > "$ours.new"
  if [ -f "$ours" ]; then
    comm -23 "$ours" "$ours.new" | while read -r gone; do rm -f "$made$gone"; done
  fi
  while read -r file; do
    cmp -s "linux/$file" "$made$file" && continue
    mkdir -p "$(dirname "$made$file")"
    cp "linux/$file" "$made$file"
    echo "copied: $(basename "$made"): ${file#./}"
  done < "$ours.new"
  mv "$ours.new" "$ours"
done
