# patches

Changes this repository makes to the Linux kernel before upstream has them. Each is a patch
file, applied to the kernel exactly as released, so that what we changed is written down as
the change itself: it can be read, reviewed, and proposed upstream as it stands.

## Layout

```
patches/
    linux-<version>/        one directory per kernel patched, at the version pinned
        <what>.patch        one change, named for what it is
    pristine/               the release tarball, fetched once, never committed
    apply.sh                makes vendor/ from the above
linux/                      files that are ours and live in the kernel tree, whole, at their
                            paths there: linux/fs/sharedfs/ is fs/sharedfs/
vendor/                     made, never committed: the kernel as released, patched, with
                            linux/ copied in
```

`build-kernel.sh` builds `vendor/linux-<version>`.

## Using it

`build-kernel.sh` runs `apply.sh` first, which does nothing where no patch has changed. Where
one has, it makes the tree again from the tarball, and the kernel builds from clean.

The files under `linux/` are brought up to date on every run, each only where it differs, so
editing one costs a build of what depends on it and nothing more.

## What is a patch and what is a file

A patch is a change to code that is someone else's: the kernel's own files, or bindings we
mean upstream to have. A file under `linux/` is code that is ours and happens to be built
inside the kernel tree: the sharedfs file system. It is kept whole so that its history is
this repository's history, a diff of it is a diff of the code and not of a patch, and it is
edited where it lies. The one patch that goes with it, `sharedfs.patch`, is the two lines
that name it in `fs/Kconfig` and `fs/Makefile`.

## A patch

A patch is one change, the one that would be proposed upstream, and is named for what it is:
`rust-fs.patch`, not a number in a sequence. The patches of a kernel are a set and not a
history. Where what we need of a change grows, its patch is rewritten to say the whole of it;
a second patch is a second change.

Each applies on its own to the kernel as released, which `apply.sh` tries before it applies
them together, so any one of them can be proposed, taken or dropped without the others
getting in its way. Applying alone is not building alone: `rust-fs.patch` uses the type
`rust-sync-locked.patch` adds, and each patch's message names what it builds on.

It opens with a message, as a commit does: what the change is, why this project needs it, and
whose work it stands on. The rest is a unified diff with paths relative to the kernel's root,
one directory stripped (`a/fs/...`, `b/fs/...`).

Two patches that add a line at the same place in the same file each apply alone and then
collide when applied together. Give one of them another place.

## Writing one

Work in a checkout of the release, with `KSRC=<checkout> ./build-kernel.sh` to build it as it
stands, so a change in progress does not cost a build from clean each time. When the change
is right, copy the files it touches as released and as changed to two directories `a` and
`b`, and write `diff -ruN a b` after the message.

## Changing the version pinned

Rename the directory to the new version and run `build-kernel.sh <version>`. A patch that no
longer applies is one upstream changed underneath, and is rewritten against the new source;
one upstream has taken is deleted.

## What the patched kernel is not

It is not this repository's code: it is not formatted, checked for lints or tested by
anything here except by booting it.
