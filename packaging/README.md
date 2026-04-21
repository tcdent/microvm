# Packaging

Debian package build for microd, microctl, and supporting tooling
(see `VM-8` for design notes).

## Version scheme

`0.SPRINT.PATCH`:

- `0.15.0` — sprint-15 close (initial release)
- `0.15.1` — hotfix during sprint-15
- `0.16.0` — sprint-16 close
- `0.16.1` — hotfix during sprint-16

The leading `0.` stays until the API (IPC surface + binary interface)
promises stability. We're nowhere near that yet.

## Quick build

From the repo root:

```
./packaging/build.sh                    # current HEAD, auto-detect version
./packaging/build.sh --version 0.16.0   # explicit version
```

Output lands in `packaging/out/microd_<version>_amd64.deb`.

## Install

```
sudo dpkg -i packaging/out/microd_0.16.0_amd64.deb
sudo systemctl enable microd
# edit /etc/microd/config.ini (replace CHANGEME creds)
sudo systemctl start microd
```

## Upgrade / downgrade

Standard dpkg patterns:

```
# Upgrade
sudo dpkg -i microd_0.16.1_amd64.deb

# Downgrade — same command, older version
sudo dpkg -i microd_0.15.2_amd64.deb
```

Maintainer scripts stop microd before replacing binaries and start
after. Running VMs die across upgrade (house of cards, see VM-3 for
the future fix). Drain VMs (`microctl stop <each>`) before upgrading
production workloads.

`/etc/microd/config.ini` is a conffile — operator edits survive
upgrade. The upstream template ships as
`/etc/microd/config.ini.dpkg-dist` for reference diffing.

## What's in the package

- `/usr/local/bin/microd` — orchestrator daemon
- `/usr/local/bin/microctl` — CLI client
- `/usr/local/bin/golden-upload` — golden-bucket build tool
- `/etc/systemd/system/microd.service` — systemd unit
- `/etc/microd/config.ini` — config (conffile; CHANGEME creds)
- `/etc/microd/coding-agent.toml` — default ruleset
- `/usr/local/share/microd/bootstrap-golden.sh` — operator bootstrap script

**Not in the package:**

- Guest kernel + initrd. These ship separately (big, refresh cadence
  independent of orchestrator code). Operators fetch these from a
  separate release stream or build locally via `dist/bootstrap-rootfs.sh`.
- bootstrap-rootfs.sh itself — requires debootstrap on the build host,
  so it's a build-side tool not a runtime artifact.
