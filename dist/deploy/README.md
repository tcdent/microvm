# microd systemd deployment

Deployment instructions for the system agent.

## Source locations (build-time)

Release binaries live under each crate's `target/release/`:

```
/home/slomp/Work/vm/orchestrator/target/release/microd
/home/slomp/Work/vm/microctl/target/release/microctl
```

Build command (run from each crate dir):

```
cd /home/slomp/Work/vm/orchestrator && cargo build --release
cd /home/slomp/Work/vm/microctl    && cargo build --release
```

Deployment templates:

```
/home/slomp/Work/vm/dist/deploy/microd.ini       # → /etc/microd/config.ini
/home/slomp/Work/vm/dist/deploy/microd.service   # → /etc/systemd/system/microd.service
```

Other assets the daemon needs:

```
/home/slomp/Work/vm/dist/vmlinux                         # → /var/lib/microd/vmlinux
/home/slomp/Work/vm/dist/initramfs.cpio.gz               # → /var/lib/microd/initramfs.cpio.gz
/home/slomp/Work/vm/s3-rootfs/rulesets/coding-agent.toml # → /etc/microd/coding-agent.toml
```

## Install layout (target host)

```
/usr/local/bin/microd              (0755, root:root)
/usr/local/bin/microctl            (0755, root:root)

/etc/microd/config.ini             (0644, root:root; 0600 if inlining S3 creds)
/etc/microd/coding-agent.toml      (0644, root:root)

/etc/systemd/system/microd.service (0644, root:root)

/var/lib/microd/                   (0755, root:root)
/var/lib/microd/vmlinux            (0644, root:root)
/var/lib/microd/initramfs.cpio.gz  (0644, root:root)
/var/lib/microd/ephemeral/         (0755, root:root; created on first run)

/var/log/microd/                   (0755, root:root; created on first run)
/run/microd/                       (0755, root:root; created by systemd RuntimeDirectory=)
```

## Prerequisites

- `ip` command available (`iproute2` package) — microd uses it for
  tap device allocation/release per-VM.
- `CAP_NET_ADMIN` on the microd process. Two ways:
  1. Run as root (default in the provided unit file).
  2. Run as dedicated user with `setcap cap_net_admin+ep /usr/local/bin/microd`
     (comment-driven alternative in the unit file).
- Reachable S3 endpoint (RustFS or MinIO) — endpoint + creds go in
  `/etc/microd/config.ini` under `[s3]`.
- Kernel + initrd at `/var/lib/microd/` — see template paths above.
- Golden-FS bucket (optional but recommended): pre-upload via
  `dist/golden-upload` before first start. Disable by removing
  `golden_bucket = ...` from `[defaults]`.

## Install steps (for the system agent)

1. Copy binaries:
   ```
   install -m 0755 /home/slomp/Work/vm/orchestrator/target/release/microd /usr/local/bin/microd
   install -m 0755 /home/slomp/Work/vm/microctl/target/release/microctl /usr/local/bin/microctl
   ```

2. Create config dirs and copy templates:
   ```
   mkdir -p /etc/microd /var/lib/microd /var/log/microd
   install -m 0644 /home/slomp/Work/vm/dist/deploy/microd.ini /etc/microd/config.ini
   install -m 0644 /home/slomp/Work/vm/s3-rootfs/rulesets/coding-agent.toml /etc/microd/coding-agent.toml
   install -m 0644 /home/slomp/Work/vm/dist/vmlinux /var/lib/microd/vmlinux
   install -m 0644 /home/slomp/Work/vm/dist/initramfs.cpio.gz /var/lib/microd/initramfs.cpio.gz
   ```

3. Edit `/etc/microd/config.ini`:
   - Set real `[s3] admin_access_key` / `admin_secret_key`.
   - Verify `[s3] endpoint` matches the production S3.
   - Verify `[defaults] golden_bucket` matches the pre-uploaded bucket
     (or remove the line to disable the golden layer).

4. Install the systemd unit:
   ```
   install -m 0644 /home/slomp/Work/vm/dist/deploy/microd.service /etc/systemd/system/microd.service
   systemctl daemon-reload
   systemctl enable microd
   systemctl start microd
   ```

5. Verify:
   ```
   systemctl status microd
   journalctl -u microd -n 50 --no-pager
   microctl --socket /run/microd/microd.sock list     # should print "no VMs"
   ```

## Golden bucket bootstrapping

microd boots VMs faster when a shared golden-FS bucket is configured
(one in-RAM manifest + blob cache serves every VM, no per-VM load
from S3). The deployment runs without it — each VM just falls back
to per-VM load from its own bucket — but it's strongly recommended.

Two patterns:

### Pattern A — one golden bucket, many microd hosts (recommended)

If you have multiple microd instances (dev + production, or multiple
production hosts sharing an S3 endpoint), build the golden bucket
ONCE from any host with the source repo + `debootstrap`, then point
every microd `[defaults] golden_bucket` at it.

```
# On any host with access to the source repo:
cd /home/slomp/Work/vm
sudo -E dist/bootstrap-rootfs.sh trixie 20260420 golden http://192.168.88.15:9090

# On every microd host, in /etc/microd/config.ini:
[defaults]
golden_bucket = golden

# Restart microd to load it:
systemctl restart microd
```

### Pattern B — one-shot on a production host

If the source repo is checked out on the production host, the
provided wrapper reads `/etc/microd/config.ini` and invokes
`bootstrap-rootfs.sh` with the right args:

```
sudo /home/slomp/Work/vm/dist/deploy/bootstrap-golden.sh \
    --config /etc/microd/config.ini \
    --suite trixie
```

The wrapper validates that `admin_access_key` isn't still
`CHANGEME` before running and exports it as `AWS_ACCESS_KEY_ID`
for the upload step. After it completes, restart microd to pick
up the shared caches.

### Verifying the golden bucket loaded

```
# microd logs the golden cache stats on startup:
journalctl -u microd | grep -i "golden"
# → "S3Fs: golden manifest loaded, 4231 entries" (or similar)

# In a running microd, every VM that doesn't override golden_bucket
# shares those caches.
```

### Refreshing the golden image

Golden buckets are immutable per version — re-running bootstrap with
the same bucket overwrites blobs in place. Safer pattern: bootstrap
to a new bucket (`golden-20260420`, `golden-20260421`), switch the
config, restart. Old bucket can be deleted via S3 admin tools once
no microd points at it.

## Smoke test (after install)

```
# Start a test VM (note: requires a reachable S3 bucket and golden if configured).
microctl --socket /run/microd/microd.sock start --name test-vm --memory 512

# Verify it's running.
microctl --socket /run/microd/microd.sock list
microctl --socket /run/microd/microd.sock inspect test-vm

# Watch the serial log.
microctl --socket /run/microd/microd.sock logs test-vm --follow

# Clean up.
microctl --socket /run/microd/microd.sock stop test-vm
```

## Operational notes

- **Shutdown semantics**: `systemctl stop microd` sends SIGTERM →
  microd sets its shutdown flag → IPC server stops accepting → all
  running VM threads are signaled and joined. Timeout 60s per the
  unit file; a busy guest vcpu may extend this.
- **Logs**: serial console output per VM at
  `/var/log/microd/<name_hash>/serial.log`. microd's own stdout/stderr
  go to the journal (`journalctl -u microd`).
- **Bucket persistence**: VM buckets (`vm-<name_hash>`) are PRESERVED
  on `microctl stop`. Relaunching the same name resumes the same
  state. No auto-delete; use the S3 admin tools to destroy if needed.
- **Tap cleanup on crash**: if microd crashes without running
  `ip link del`, the tap devices leak. Check `ip link show | grep tap-`
  after an unclean restart; `ip link del tap-...` by hand.
- **Multiple deployments**: to run a pool alongside a dev microd,
  override socket/log_dir/ephemeral_root in a separate config file
  (copy `microd.ini` and give it unique paths).

## Known v0 gaps

See `research/15-orchestration-design.md` §11 for the full list.
Operationally relevant:

- No orchestrator restart recovery — restarting microd kills all
  running VMs. Clean restart only.
- No ACPI graceful guest shutdown — stop waits for next KVM vcpu exit.
  Busy guests may take seconds to stop.
- Per-VM S3 access uses the `[s3]` admin creds as defaults (latent
  gap; works for RustFS/MinIO where admin==dev).
- No HTTP adapter yet — only the Unix socket. HTTP wrapper is a
  sprint-16 candidate.

## Bumping the deployment

Release a new build:

```
cd /home/slomp/Work/vm/orchestrator && cargo build --release
cd /home/slomp/Work/vm/microctl    && cargo build --release
sudo systemctl stop microd
sudo install -m 0755 /home/slomp/Work/vm/orchestrator/target/release/microd /usr/local/bin/microd
sudo install -m 0755 /home/slomp/Work/vm/microctl/target/release/microctl /usr/local/bin/microctl
sudo systemctl start microd
```

Running VMs are killed by the stop. Drain first if needed
(`microctl stop <each vm>` until `list` is empty).
