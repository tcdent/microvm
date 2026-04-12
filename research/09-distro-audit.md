# Distro Audit and Build Strategy

How we build the guest rootfs today, what's bloat, and where to go
next. Goal: a fully-featured coding agent VM (minus IDE) that is NOT
a desktop or server. Ephemeral. Headless. Single user.

---

## Current State

### Build pipeline

`dist/bootstrap-rootfs.sh` uses `debootstrap --variant=minbase` with
explicit packages:

```
Core:        systemd-sysv, procps, less, sudo, locales
Networking:  iproute2, iputils-ping, curl, wget, ca-certificates,
             bind9-dnsutils, net-tools, systemd-timesyncd,
             isc-dhcp-client
Development: git, python3, build-essential, openssh-client
Tools:       vim-tiny, apt-transport-https, iperf3
```

debootstrap expands this to **221 packages**, including the full
dependency closure.

### Measured footprint

- **Total rootfs: 1.37 GB / 10,338 objects in S3**
- **`/usr`: 602 MB** (lib 231MB, share 142MB, bin 95MB, sbin 10MB)
- **`/var`: 301 MB** (cache 240MB, lib 60MB)
- **`/usr/share`: locale 73MB, i18n 14MB, doc 21MB, man 8MB**

Kernel: 31MB `vmlinux`, 1261 built-in features, **zero loadable
modules** (everything compiled as `=y`).

### Running services (observed)

Real processes: `systemd`, `systemd-journald`, `systemd-timesyncd`,
`dhclient`, `login`, `bash` — and **6× `agetty` on tty1–tty6**.

Enabled services (filesystem): `network-setup`,
`remote-fs`, `serial-console`. Only 3.

**Not installed/running:** dbus, rsyslog, cron, postfix. Good.

#### Process count (12 user-space, ~30 kernel threads)

```
PID  CMD
  1  /sbin/init (systemd)
459  systemd-journald
507  dhclient eth0
509  agetty tty1
510  login -- root  (parent of bash on hvc0)
516  agetty tty2
518  agetty tty3
519  agetty tty4
525  agetty tty5
528  agetty tty6
531  -bash             (interactive shell)
636  systemd-timesyncd
```

Of these, only systemd, journald, dhclient, login/bash, and timesyncd
do real work. The 6 ttys are pure waste — there's no console for
them to attach to. Dropping them frees ~6MB RSS and 6 process slots.

Also: **dhclient should probably be replaced**. We start it once
on boot via `network-setup.service`, then it daemonizes and renews
the lease forever. Could be replaced with a single `dhcpcd -1` (one
shot) at boot, then static config. Or static IP directly — DHCP is
overkill for a known network.

---

## Bloat Inventory

Ranked by impact and ease of removal.

### Tier 1: Pure Waste (320MB+, trivial to fix)

| Item | Size | Action |
|------|------|--------|
| `/var/cache/apt/archives` | ~240MB | Delete after debootstrap completes — we never apt-install in the VM |
| `/usr/share/locale` | 73MB | Keep en_US only; purge rest |
| `/usr/share/doc` | 21MB | Delete — no human reading docs in an agent VM |
| `/usr/share/man` | 8MB | Delete — same reason |
| `/usr/share/i18n` | 14MB | Delete — locale archives for languages we don't use |
| tty1–tty6 getty | — | Mask in systemd — only hvc0 is a real console |

Immediate savings: **~360MB**, ~26% of rootfs.

### Tier 2: Unused Packages (~50MB, needs package removal)

| Package(s) | Size | Why remove |
|------------|------|------------|
| `bind9-dnsutils`, `bind9-host`, `bind9-libs` | ~15MB | DNS query tools (`dig`, `host`) — rare need; musl stub resolver suffices |
| `net-tools` | ~2MB | Legacy `ifconfig`/`netstat`/`route` — fully replaced by `iproute2` |
| `apt-transport-https` | ~1MB | apt 2.0+ handles HTTPS natively |
| `libfido2-1`, `libcbor0.10` | ~2MB | FIDO2 hardware keys in ssh — unused |
| `apparmor` (userspace) | ~1MB | Userspace package can be removed. **`libapparmor1` must stay** — systemd links against it at compile time (discovered during Phase 1: removing the library caused kernel panic). |
| `sqv` | ~1MB | Sequoia OpenPGP verifier — apt key signing only |
| `vim-tiny` | ~4MB | Editor in ephemeral VM? Debatable — agent writes files directly |
| `debconf` | ~1MB | Interactive package config — we run non-interactive |
| Full `binutils` (after build-essential) | ~30MB | Linker + assembler — needed for compilation but heavy |

Harder to remove than Tier 1 because they're pulled by dependencies.
Would require custom package selection.

#### Pulled-in dependencies worth investigating

These come in via `--include` packages but aren't called out in
debootstrap output:

- **`systemd-resolved`**: pulled by systemd. We don't use it (custom
  `network-setup.service` uses dhclient directly). Could mask.
- **`apparmor`** as a userspace package: profiles directory exists,
  loader runs, but no profiles are loaded. Plus the kernel module
  for it costs nothing if unused, but the userspace tools are dead
  weight.
- **`gpgv`** + **`gpgconf`** + **`gnupg`**: required by apt for
  package signature verification. We don't apt-install at runtime,
  so these are only needed during the bootstrap chroot. Could be
  removed post-debootstrap.
- **`python3-minimal`** vs full `python3`: full pulls in distutils,
  ensurepip, idle-python, lib2to3 — none useful for an agent runtime.
  Need to test if real Python workloads need them.
- **`mawk`** + **`gawk`**: both installed. Pick one (gawk is more
  capable, mawk is faster — most scripts work with either).
- **`tzdata`** (~2MB): timezone database. Required for date handling.
  Could trim to just UTC + a few common zones.
- **`base-passwd`**, **`init-system-helpers`**: priority=required, but
  some files inside (passwd templates, shell scripts) could be
  pruned post-install.

### Tier 3: Systemd Errors (cosmetic, 0MB)

From the audit:
1. `systemctl` times out — no dbus
2. SELinux policy missing but kernel has it compiled
3. `ldconfig.service` fails
4. `systemd-journal-catalog-update` fails
5. `systemd-update-done` fails
6. `modprobe@*.service` all fail (no kmod, no modules)
7. Stale `dev-ttyS0/hvc0.device` 90s timeouts

None affect functionality but they pollute logs and add boot delay.

#### Root cause analysis

These errors all stem from a common root: **systemd assumes a normal
init flow that we don't follow.**

- **No D-Bus**: systemd's interactive control surface (`systemctl`)
  needs D-Bus, but D-Bus needs polkit, which needs PAM... we skip the
  whole chain. Result: any `systemctl` command from inside the guest
  hangs for ~25s before timing out. The agent should never need
  `systemctl` — services are just enabled via filesystem symlinks.
- **No `kmod`**: kernel modules are all `=y`, so `modprobe` doesn't
  need to do anything. But systemd's `modprobe@<name>.service` units
  unconditionally invoke `/sbin/modprobe`, which doesn't exist. Each
  unit fails with `Unable to locate executable '/sbin/modprobe'`.
  Affects: `configfs`, `drm`, `efi_pstore`, `fuse`. Cosmetic since
  the modules are built in.
- **`ldconfig.service` failure**: tries to write to `/etc/ld.so.cache`
  via `ldconfig`. Likely failing because of our `setattr` stub (modes
  not persisted) or write buffer limits. Worth investigating — could
  point to a real FUSE bug.
- **`systemd-journal-catalog-update`**: rebuilds journal metadata
  catalog. Fails with `status=1/FAILURE`. We don't need it — we don't
  use journal catalogs (no `journalctl --catalog` lookups).
- **`systemd-update-done`**: writes a stamp file marking first-boot
  setup complete. Fails to write. Cosmetic.
- **`dev-*.device` 90s timeouts**: the stock `serial-getty@.service`
  templates have `BindsTo=dev-X.device`. systemd waits for udev to
  emit the device-ready event, which never comes (no udev). We mask
  the templates but the `.device` units themselves still wait for
  someone — eventually they time out. Possible fix: mask the
  `.device` units or set `DefaultTimeoutStartSec=10s` globally.

This points at a pattern: **the whole `dev-*.device` mechanism, the
modprobe@ units, and the journal/ldconfig services are all designed
for a system with udev + dbus + writable /etc**. We have none of
those. Stripping these is correct, not a hack.

### Tier 4: Kernel Bloat (31MB compressed, unknown uncompressed)

1261 built-in features. Candidates to drop (confirmed unused):
- SELinux, AppArmor, Smack (no LSM configured)
- iSCSI, NFS, CIFS/SMB, Ceph (we only mount virtiofs)
- XFS, Btrfs, ZFS, F2FS, ReiserFS (virtiofs only)
- Sound subsystem (no audio)
- WiFi, Bluetooth (no wireless)
- Printer support
- Watchdog drivers
- Most IPv6 extensions (keep basic IPv6)
- Multiple console types (keep only virtio-console + serial)

Estimated savings: 30-50% kernel size, faster boot.

---

## Strategic Options

### Option A: Tuned Debian (recommended)

Stay on Debian/debootstrap but aggressively trim. No custom distro.
Steps:

1. **Post-bootstrap cleanup script**: deletes `/var/cache`, `doc`,
   `man`, non-English `locale`, `i18n`. Runs before S3 upload.
2. **Package blocklist**: remove `bind9-*`, `net-tools`, `apparmor`,
   `debconf`, `sqv`, etc. after debootstrap. Some require
   `--force-depends` due to priority=required markings.
3. **Systemd mask file**: one list of services to mask (tty1-6 getty,
   ldconfig, modprobe@*, journal-catalog-update, update-done).
4. **Kernel config strip**: separate pass — build a smaller kernel
   with unused subsystems disabled.

Expected result: **~800MB rootfs, ~20MB kernel, no systemd errors**.

Pros: preserves apt ecosystem, familiar tooling, well-tested security
updates. Easy to add a package later if needed.

Cons: still carrying some Debian overhead (dpkg database, apt).

### Option B: Custom distro from scratch

Roll our own userspace — busybox or toybox for coreutils, musl libc,
hand-picked binaries for git/python/gcc.

Pros: smallest possible image (maybe 100-200MB), fastest boot, total
control.

Cons: major engineering effort. Every package we need would be a
manual build. Security updates require tracking upstream. Debugging
issues means digging through non-standard layouts. High maintenance
burden.

### Option C: Alpine-style minimal distro

Use Alpine Linux — musl libc, busybox, apk package manager. Proven
minimal distro with a working package ecosystem.

Pros: much smaller than Debian (base ~8MB), mature tooling.

Cons: musl compatibility issues with some binaries (glibc-specific
code). Different package ecosystem means we'd rebuild our rootfs
pipeline. Tooling maintainers often don't test on Alpine.

---

## Recommendation

**Tuned Debian (Option A).** The agent workload needs a standard
environment — `git clone`, `pip install`, `npm install`, `cargo build`,
`gcc` all need to work without surprises. Debian's package coverage
and ABI compatibility are worth the 200-400MB overhead vs Alpine/custom.

### Design principles

1. **Pre-boot configuration preferred.** Any rootfs modifications
   happen in `bootstrap.sh` before S3 upload. No scripts run inside
   the VM to do config. The VM boots from a golden filesystem.

2. **Prefer not-installed over removed.** If we can use `--exclude`
   during debootstrap or skip the `--include`, that's cleaner than
   installing then removing. Fewer moving parts, smaller bootstrap.

3. **Eliminate errors, don't work around them.** Services that fail
   to start get removed from the build — not masked. The only exception:
   systemd units that fail because we don't have udev/dbus/kmod (these
   are documented in Tier 3 root cause analysis and can be masked as
   correct behavior, not workarounds).

4. **Minimal tooling inside the VM.** Prefer host-side utilities (our
   own bin dir) for VMM-level operations. Keep the guest focused on
   running the agent and its workloads.

5. **Runtime installs allowed but no persistent caches.** The agent
   needs to `apt install`, `brew install`, `npm install` — fine. But
   don't store package caches in the rootfs. Per-VM caches in /tmp
   are fine (ephemeral); persistent caches would belong in a separate
   cache volume.

6. **Don't break the ability to boot other kernels/rootfs.** Our VMM
   must still work if passed a vanilla kernel or a different distro
   image. Our tuning is for our golden image only.

### Systemd strategy

Keep systemd as init + service manager. Strip the bloat:

- **Drop (not install) or disable**: `systemd-timesyncd` (upstream
  NTP), `systemd-resolved` (upstream DNS via dhclient), journal
  catalog service, `systemd-update-done`, `ldconfig.service`,
  `modprobe@*.service`, tty1-6 getty.
- **Filesystem caches in general are probably slower than direct
  upstream service requests** (DNS resolver, NTP) when those services
  are on the immediate network. Skip the caching layer.
- **Investigate `ldconfig.service` failure** — could be a real FUSE
  bug (write to /etc/ld.so.cache). Worth debugging before we mask.

Longer term, if systemd's dynamic service management isn't earning
its weight (we have 3 services, all known at boot time), a hand-
written init script could replace it. But that's a separate decision
after we see how much bloat is actually removable.

The Tier 1 cleanup alone (360MB, 0 effort) puts us in reasonable
shape. Tier 2 is worth doing. Tier 3 fixes make the VM feel polished.
Tier 4 (kernel) is a separate project but high-value for boot time.

### Long-term direction

If density (VMs per host) becomes critical and we need to push below
500MB, Alpine is the next step. But that's a migration to plan, not
something to rush into. The benefit has to justify the pipeline
rebuild.

A custom distro (Option B) doesn't fit our "simple stack, stable
tooling" ethos. We'd be reinventing package management and security
update tracking. Skip.

---

## Implementation Plan

### Phase 1: debootstrap flags + post-bootstrap cleanup

**Prefer not-installed over removed.** First pass: use debootstrap
`--exclude` for packages we explicitly include elsewhere but don't need,
and `--no-install-recommends` to skip transitively-pulled optional deps
(systemd-timesyncd, systemd-resolved likely come in this way).

Second pass: remove the filesystem waste in the chroot before upload.

```bash
# Clear apt cache and lists — we don't apt-install in the VM
rm -rf "$WORK/var/cache/apt/archives"/*.deb
rm -rf "$WORK/var/lib/apt/lists"/*

# Strip locale (keep en_US)
find "$WORK/usr/share/locale" -mindepth 1 -maxdepth 1 -type d \
    ! -name 'en_US' ! -name 'locale.alias' -exec rm -rf {} +

# Strip docs, man pages, info
rm -rf "$WORK/usr/share/doc"/* "$WORK/usr/share/man"/* "$WORK/usr/share/info"/*

# Drop i18n catalogs
rm -rf "$WORK/usr/share/i18n/locales"
```

### Phase 1: implementation status

**In progress.** Changes to `dist/bootstrap-rootfs.sh`:

1. **Slimmed PACKAGES list:**
   - Removed: `bind9-dnsutils` (use upstream DNS, no need for `dig`),
     `net-tools` (iproute2 covers it), `apt-transport-https` (apt 2.0+
     has TLS built-in)

2. **Added EXCLUDES via debootstrap `--exclude`:**
   `systemd-resolved,systemd-timesyncd,vim-tiny,apt-transport-https,debconf-i18n`
   These get pulled as recommends/deps; explicitly excluded.

3. **Post-debootstrap purge** of transitively-pulled packages:
   `apparmor libfido2-1 libcbor0.10 sqv gnupg gpgv gnupg-l10n`
   Run via `chroot dpkg --purge --force-depends`.

   **Finding (debugged):** Initially included `libapparmor1` in the
   purge list. This caused a kernel panic on boot: `/sbin/init: error
   while loading shared libraries: libapparmor.so.1: cannot open
   shared object file`. **systemd is linked against libapparmor1 at
   compile time**, even when AppArmor is unconfigured. The userspace
   `apparmor` package can be removed safely, but the shared library
   must stay. Lesson: in Debian, "no AppArmor configured" doesn't
   mean "AppArmor lib unused" — systemd still calls into it via
   conditional checks.

   **Finding (TLS broken without timesyncd):** Removing
   `systemd-timesyncd` via `--exclude` left the guest with no clock
   sync. The kernel's built-in epoch is around 2025-09-03 (when the
   kernel was built), which is *before* the validity start date of
   most current TLS certificates. Result: every HTTPS request fails
   with `certificate is not yet valid`. Setting the date manually
   (`date -s ...`) confirms TLS works otherwise.

   **Decision: keep systemd-timesyncd.** The 1MB is worth functional
   TLS out of the box. Without time sync, the guest clock defaults to
   the kernel build epoch, which is before the validity start date of
   current TLS certificates — every HTTPS request fails. Alternatives
   considered:
   - **DHCP option 42 + ntpdate** (one-shot, no daemon) — needs ntpdate
     or similar, more moving parts than timesyncd.
   - **VMM-injected time** via `ptp_kvm` or custom kernel param — most
     elegant (host clock visible as a PTP device, microsecond accuracy,
     zero network dependency) but requires VMM work and guest-side
     chrony (heavier than timesyncd). Worth doing later if we want to
     eliminate the NTP dependency entirely.
   - **chrony/ntpsec** — heavier than timesyncd, no benefit for ephemeral
     VMs on a local network.
   **Not re-evaluated without new data.**

4. **Filesystem cleanup before upload:**
   - `/var/cache/apt/archives/*.deb` and `partial/`
   - `/var/lib/apt/lists/*` and `partial/`
   - `/usr/share/doc/*`
   - `/usr/share/man/*`
   - `/usr/share/info/*`
   - `/usr/share/locale/*` except `en_US`
   - `/usr/share/i18n/locales`

5. **Removed broken systemd unit files:**
   - `serial-getty@.service` (90s udev wait that never fires)
   - `modprobe@*.service` (no `/sbin/modprobe`, modules built in)
   - `systemd-journal-catalog-update.service`
   - `systemd-update-done.service`
   - `autovt@.service` (drives the auto-launched tty1 getty)

6. **Deferred:** `ldconfig.service` removal — investigate the FUSE
   failure first (Phase 2). Want to know if it's a real bug or just
   "we don't need this in a static-image VM".

#### Phase 2 ldconfig investigation

Ran `/sbin/ldconfig` manually inside the guest and traced via VMM log:

- **ldconfig itself succeeds.** Writes `etc/ld.so.cache~` (8819 bytes),
  then renames to `etc/ld.so.cache`. Same for `aux-cache~`. Our
  rename (server-side copy + delete) handles it correctly.
- **`stat /etc/ld.so.cache` works fine** — returns proper metadata.
- **`ls -la /etc/ld.so.cache` reports EIO** but then prints the file.
  Plain `ls` (no `-l`/`-a`) works.

This is a `getxattr` interaction bug unrelated to ldconfig itself.
`ls -l` calls `lgetxattr` for `security.selinux`, capabilities, and
ACLs to render the right indicators. We return `ENODATA` which is
correct, but coreutils' ls treats some response as a transient error
and prints EIO before continuing.

**Conclusion:** ldconfig works. The systemd `ldconfig.service`
failure on boot is probably about *running before required mounts
exist* or similar timing issue, not a FUSE bug. The actual ldconfig
binary writes its caches successfully when run interactively.

**Action:** Keep ldconfig.service for now — the cache is useful for
fast library resolution. The boot-time failure is cosmetic.

#### Phase 2 service cleanup (in progress)

After Phase 1, audited what was still enabled/running:

- **Stock services enabled**: getty.target, systemd-logind,
  systemd-ask-password-wall.path, systemd-user-sessions
- **Stock timers enabled**: apt-daily, apt-daily-upgrade,
  dpkg-db-backup, fstrim, systemd-tmpfiles-clean
- **Failures this boot**: systemd-modules-load.service (kmod removed)
- **tty1-6 getties RETURNED** despite Phase 1 removing autovt@:
  culprit is `getty-static.service`, condition
  `!/usr/bin/dbus-daemon` triggers `ExecStart=systemctl start
  getty@tty2..6.service` directly. Phase 1 removed the wrong unit.

Phase 2 actions added to `bootstrap-rootfs.sh`:

1. **Drop scheduled-maintenance timers**: apt-daily, apt-daily-upgrade,
   dpkg-db-backup, fstrim. Pure waste — ephemeral VM, no apt updates,
   no dpkg state to back up, virtiofs doesn't benefit from fstrim.
   **Keep `systemd-tmpfiles-clean.timer`** — useful for /tmp tmpfs.
2. **Drop `getty-static.service`** + its wants symlink. Eliminates
   the tty2-6 getty spam at boot.
3. **Drop `systemd-modules-load.service`** + sysinit wants. We have
   no kernel modules — everything is built-in.
4. **Drop `systemd-ask-password-wall.path`** + service. No passwords,
   no FDE prompts.
5. **Drop `systemd-logind.service`** + dbus-org.freedesktop.login1.
   Single root login on hvc0, no session manager needed. If login
   breaks, restore.

**FIXED: `ls -la` spurious EIO.** Root cause was returning `ENODATA`
(specific attribute missing) from `getxattr`/`listxattr` when we
should return `ENOTSUP` (filesystem doesn't support xattrs at all).
The distinction matters to coreutils — `ls -la` probes
`system.posix_acl_access/default` on every file before rendering
permissions. `ENODATA` signals "this file has no ACL", which coreutils
treats as unusual and logs EIO. `ENOTSUP` signals "xattrs aren't a
thing on this fs", which coreutils silently skips. Changed both
`getxattr` and `listxattr` in `s3fs.rs` to return `ENOTSUP`.

#### To verify after build

- Boot succeeds, login prompt on hvc0
- `curl https://claude.ai/install.sh | bash` still installs
- Rootfs size dropped from 1.37GB toward target ~800MB
- No new boot errors introduced by package removals

#### Verified results

- **Rootfs size: 1.37GB → 504MB (63% reduction, 866MB saved)**
- Object count: 10338 → 14009 (more individual objects after cleanup —
  unsure why; possibly debootstrap caching artifacts that we removed)
- Boot succeeds with verbose kernel logging
- Claude Code 2.1.101 installs successfully (with manual clock set)
- No new errors compared to baseline

#### Lessons from Phase 1

1. **Verbose logging stays on for development.** With `quiet
   loglevel=3` the boot looks "stuck at KASLR" because all messages
   are suppressed. For development we want to see what's happening.
2. **Test removals incrementally.** libapparmor1 and timesyncd both
   broke things; finding both at once would have been confusing.
3. **debootstrap `--exclude` works for both direct and transitive
   recommends.** `systemd-timesyncd` is pulled as a dependency of
   systemd but `--exclude` successfully blocked it.

### Phase 2: Service cleanup

**Remove (don't mask) services that fail to do real work.** Masking is
reserved for legitimate init-flow differences (no udev/dbus/kmod —
see Tier 3 root cause). Failing services that represent missing
functionality we don't want should be deleted from the build entirely.

- Remove units: `ldconfig.service`, `systemd-journal-catalog-update`,
  `systemd-update-done`, `modprobe@*.service`. These fail because we
  don't need what they do.
- Mask (legitimately): `getty@tty1-6` (no tty devices), the stock
  `serial-getty@.service` (we have our own `serial-console.service`).
- **Investigate `ldconfig` first** — write to `/etc/ld.so.cache` via
  FUSE. Could be a real bug in our setattr/rename path. Fix the bug
  if real, then decide whether ldconfig is actually needed (static
  linking in the golden image avoids the need entirely).

### Phase 3: Package trimming (~50MB saved)

After debootstrap, `dpkg --purge` for Tier 2 packages that couldn't be
excluded via debootstrap flags. Examples: `libfido2-1`, `libcbor0.10`,
`libapparmor1`, `sqv`. Some require `--force-depends` due to
priority=required markings.

### Phase 4: Kernel reconfig

**First pass implemented and verified. Second pass deferred.**

Goal: reduce binary size and attack surface. (Note: kernel start
is already only 2.4s — this is *not* a boot-time win.)

#### First pass disables

Added to `kernel/build-kernel.sh` after the base config:
- Filesystems we don't mount: XFS, F2FS, REISERFS, JFS, NFS,
  CIFS, CEPH_FS, 9P_FS, HFS, UDF, ISOFS, SQUASHFS, OVERLAY,
  AUTOFS
- Block layer: SCSI/iSCSI/ATA/MD/DM/NBD/RAM
- LSMs: SELINUX, APPARMOR, SMACK, TOMOYO, LOCKDOWN, YAMA,
  LOADPIN, SAFESETID, INTEGRITY/IMA/EVM
- Networking extras: WIRELESS/CFG80211/MAC80211, BT, IEEE802154,
  RDS, TIPC, DCCP, SCTP, IPV6_MROUTE, IPV6_SEG6, MPTCP, NET_DSA
- Hardware we don't have: USB, HID, INPUT, DRM, FB, SOUND/SND,
  MEDIA, SURFACE_AGGREGATOR, FIREWIRE, PRINTER, PARPORT
- Power/thermal: WATCHDOG, THERMAL, HWMON, SUSPEND, HIBERNATION,
  ACPI_VIDEO, ACPI_HOTPLUG_*, CPU_FREQ governors except performance

#### First pass results

| Metric | Before | After | Δ |
|--------|--------|-------|---|
| vmlinux file size | 31.1MB | 27.9MB | −10% |
| `=y` features | 1261 | 1179 | −82 |
| Boot time | ~2.4s kernel | ~2.2s kernel | tiny |

Smaller-than-hoped because **the Firecracker base config already
disables most of these subsystems**. Many `--disable` calls were
no-ops on flags that were already off.

#### Kernel size analysis

`size vmlinux`:
- text: 13.9MB (executable code)
- rodata: 2.5MB (constant data, included in text)
- data: 4.9MB (initialized writable data)
- bss: 2.9MB (zero-init, doesn't take disk space)

`objdump -h` shows the biggest sections:
- `.bss` 2.6MB (RAM only, not on disk)
- `.rodata` 2.5MB (string tables, lookup tables, embedded firmware)
- `.init.text` 415KB (init code freed after boot)
- `__bug_table` 90KB (BUG_ON metadata)
- `.call_sites` 568KB (ftrace machinery — likely a target)

`vmlinux` file (27.9MB) is larger than loaded image (text+data ≈ 19MB)
because of debug sections, symbol tables, and unused exception/relocation
tables. **Strippable for distribution** (`strip` or build with smaller
debug levels), but keeping symbols helps when crashes happen — defer
stripping until the kernel is truly stable.

#### Second pass — deferred

When more reduction is needed, target by symbol weight rather than
guessing. Likely candidates by size:
- **CRYPTO algorithms**: kernel ships dozens (AES variants, SHA*,
  ChaCha, Camellia, etc). Trim to what TLS + LUKS + IPsec actually
  call. Probably 200-500KB savings.
- **NETFILTER tables and matches**: dozens of match modules,
  conntrack helpers, NAT helpers. We don't run iptables in guest —
  could trim heavily.
- **FTRACE infrastructure**: `.call_sites` (568KB) suggests
  function-tracer support is compiled in. If we don't use ftrace
  (we don't), CONFIG_FUNCTION_TRACER=n saves significantly.
- **PERF_EVENTS**: useful for profiling but unused by the agent.
  Probably 100-200KB.
- **KASAN / KCSAN / KMEMLEAK / LOCKDEP / DEBUG_INFO**: debug
  features. Likely already off in our config but worth verifying.
- **IKHEADERS / IKCONFIG**: embed kernel headers in image — useful
  for in-tree builds, useless for us.
- **KEXEC / CRASH_DUMP**: 100-200KB for crash kernel infrastructure
  we don't use.

These should be cut data-driven (run `nm --size-sort vmlinux` after
each round to see what actually shrunk) rather than by guessing.

#### Strip opportunity (future)

`strip vmlinux` would drop the symbol/debug sections, taking the
file from 27.9MB closer to the loaded ~19MB. Saves ~30% of disk
size at the cost of debuggability if we hit a crash. **Defer** —
the cost of carrying ~9MB extra in S3 is negligible compared to
the value of being able to symbolicate a kernel oops.

### S3-level pruning (alternative)

Rather than modifying the chroot and re-uploading, we could upload the
raw debootstrap output and delete unwanted objects directly in S3.
Considered but deferred: chroot-level pruning is simpler (one source of
truth) and our upload is fast enough that the "save time" argument
doesn't apply yet.

### Phase 4: Evaluate if we need more

After Phases 1-3, measure:
- Rootfs size
- Boot time (kernel start → login prompt)
- Cold first-command latency (e.g., `python -c 'print(1)'`)

If those numbers are acceptable, stop. If we need to push further,
revisit Alpine.

### Boot time results

The headline metric of this work.

| Phase | Kernel | Userspace | Total | Δ vs baseline |
|-------|--------|-----------|-------|---------------|
| Baseline (pre-Phase 1) | ~2.4s | ~95s | **~98s** | — |
| After Phase 1 (rootfs cleanup) | ~2.4s | ~95s | ~98s | ~0s |
| After Phase 2 (services) | ~2.2s | ~25s | **27.4s** | **−70s (−72%)** |
| After Phase 4 first pass (kernel) | 2.2s | 25.2s | 27.4s | same |

**Phase 2 was the entire boot-time win.** The 70-second savings came
from removing services that did pointless waiting at boot — not from
removing services that took CPU.

Specifically:
- Stock `serial-getty@.service` units had `BindsTo=dev-X.device`,
  waiting up to 90s for udev events that never come (we have no udev).
  Phase 1 masked the templates, but the `.device` units themselves
  still waited.
- `systemd-modules-load.service` failed each boot trying to load
  modules — adds delay before downstream services unblock.
- `getty-static.service` spawned 5 tty getties via systemctl, each
  with start jobs that contended for the unit-state machine.
- The 4 maintenance timers (apt-daily, apt-daily-upgrade,
  dpkg-db-backup, fstrim) all triggered shortly after boot, ate
  CPU and journal bandwidth even when they had nothing to do.

The lesson: **systemd boot time is dominated by unit-ordering wait,
not unit count or unit work**. Removing units that fail (or that do
unnecessary maintenance) eliminates serial dependencies that block
everything downstream. Kernel startup was already only ~2.4s — never
a bottleneck.

Phase 4 (kernel slimming) didn't move boot time at all since the
kernel was never on the critical path. It's a binary-size /
attack-surface improvement, not a startup-time one.

The gap (~85s) is mostly serial systemd unit ordering with no
parallelism. Multi-vCPU would help here.

---

## Open Questions

1. **Do we want `vim-tiny`?** The agent writes files directly via
   FUSE — no human edits files in the VM. Can probably drop.

2. **`build-essential` or selective?** We need `gcc` and `make` for
   native compilation (cgo, Python C extensions, node-gyp). But
   `dpkg-dev`, `fakeroot`, full `binutils-dev` are packaging tools
   we don't use.

3. **`openssh-client` vs `dropbear`?** Dropbear is much smaller but
   lacks some OpenSSH features (ControlMaster, certificate auth).
   For an agent that does `git clone git@github.com:...`, OpenSSH is
   the safer default.

4. **Multi-arch?** Debian pulls some `i386` compatibility libs for
   32-bit binaries even on amd64. We can likely drop those.

5. **How do we track package updates?** With pinned debootstrap
   snapshots we get reproducible builds but miss security patches.
   Need a refresh cadence (monthly?) with regression tests.
