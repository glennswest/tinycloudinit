# tinycloudinit

A tiny cloud-init replacement in Rust for small Fedora (and other Linux)
images. Single static binary (~700 KB, musl), no Python, no dependencies on
the target beyond `useradd`/`chpasswd` from shadow-utils. Implements the
NoCloud and EC2 (IMDS) datasources and the most-used subset of
`#cloud-config`.

## What it does

On boot (systemd oneshot, after `network-online.target`, before `sshd`):

1. **Finds a seed**, in order (`--datasource auto`, the default):
   - `--seed <dir>` on the command line
   - `/var/lib/tinycloudinit/seed/` on the local filesystem
   - a block device with filesystem label `cidata`/`CIDATA` (iso9660 or vfat),
     or any iso9660/vfat block device containing `meta-data`/`user-data`
     (one immediate pass)
   - **NoCloud device wait and EC2 IMDS probed in parallel** for up to 10 s —
     the first seed found wins and the other probe is cancelled. EC2 IMDS at
     `169.254.169.254` uses IMDSv2 (session token) with IMDSv1 fallback and
     fetches `instance-id`, `local-hostname`, and `user-data`.

   `--datasource nocloud` or `--datasource ec2` restricts the search (ec2
   retries the metadata service for up to 30 s).
2. **Checks the instance-id** from `meta-data` against
   `/var/lib/tinycloudinit/instance-id` — if unchanged, exits immediately
   (run-once-per-instance semantics; `--force` overrides).
3. **Applies the configuration** and records the instance-id.

## Supported user-data

`#cloud-config` (YAML) with this subset:

| Key | Notes |
|---|---|
| `hostname`, `fqdn` | writes `/etc/hostname` + `sethostname(2)` |
| `manage_etc_hosts` | `true` writes a minimal `/etc/hosts` with the fqdn |
| `users` | `name`, `gecos`, `shell`, `homedir`, `groups` (string or list), `sudo` (string or list), `passwd` (crypt hash, via `chpasswd -e`), `lock_passwd`, `system`, `ssh_authorized_keys` |
| `ssh_authorized_keys` | top-level keys are installed for **root** |
| `write_files` | `path`, `content`, `encoding` (`plain`/`b64`), `permissions` (quote it: `'0644'`), `owner` (`user` or `user:group`), `append` |
| `runcmd` | string (run via `/bin/sh -c`) or argv list; failures are logged, not fatal |
| `final_message` | printed at the end |

User-data starting with `#!` is executed as a script instead (once per
instance).

`meta-data` keys used: `instance-id`, `local-hostname`.

On EC2, user-data fetched from IMDS goes through the same paths: a
`#cloud-config` document is applied, a `#!` script is executed. Anything else
is ignored (multi-part MIME is not supported).

Not implemented (by design, keep it tiny): network config (use DHCP /
NetworkManager), package installation, growpart, multi-part MIME, vendor-data.

## Comparison with cloud-init

Measured on the same Fedora VM (cloud-init 25.2, Python 3.14, tinycloudinit
v0.3.0):

| | tinycloudinit | cloud-init |
|---|---|---|
| Footprint | **705 KB** single static binary | 7.1 MB package + Python runtime (113 MB `/usr/lib/python3.14` incl. stdlib) |
| Runtime deps | shadow-utils, `/bin/sh` | Python interpreter, PyYAML, Jinja2, … |
| Boot services | 1 oneshot | 4 stages / 5 services |
| Boot time (seed on cidata device) | **7 ms** | ~1.7 s total across services on the same VM |
| Boot time (EC2 IMDS) | 15 ms after network-online | similar staging plus per-module overhead |
| Datasources | NoCloud, EC2 IMDS | ~40 (all major clouds) |
| user-data | `#cloud-config` subset, `#!` scripts | full: MIME multi-part, jinja templates, vendor-data, … |
| Modules | hostname, hosts, users/sudo/ssh, write_files, runcmd | 50+ (growpart, packages, network config, disk setup, …) |

Timings above are from `systemd-analyze blame` and repeated runs of the
binary; cloud-init's numbers exclude its one-time package-install module.

Use real cloud-init when you need its datasource breadth, network
configuration, disk/partition handling, or package installation. Use
tinycloudinit when the image is small, boots on NoCloud or EC2, and the goal
is an interpreter-free image that is ready milliseconds after the disks and
network are up.

## Example seed

```
meta-data:
    instance-id: iid-node1-001
    local-hostname: node1

user-data:
    #cloud-config
    fqdn: node1.g8.lo
    manage_etc_hosts: true
    users:
      - name: glenn
        groups: wheel
        shell: /bin/bash
        sudo: "ALL=(ALL) NOPASSWD:ALL"
        ssh_authorized_keys:
          - ssh-ed25519 AAAA... glenn
    runcmd:
      - systemctl enable --now chronyd
```

Build the seed ISO:

```bash
xorriso -as mkisofs -o seed.iso -volid cidata -joliet -rock user-data meta-data
# or: genisoimage -output seed.iso -volid cidata -joliet -rock user-data meta-data
```

Attach `seed.iso` as a CD-ROM (or a small vfat volume labeled `CIDATA`) to the
VM.

## Install into an image

```bash
install -m 0755 tinycloudinit /usr/local/sbin/tinycloudinit
install -m 0644 systemd/tinycloudinit.service /etc/systemd/system/
systemctl enable tinycloudinit.service
```

To disable without uninstalling: `touch /etc/tinycloudinit.disabled`.

## CLI

```
tinycloudinit [--seed DIR] [--datasource auto|nocloud|ec2] [--state-dir DIR]
              [--dry-run] [--force] [--version]
```

`--dry-run` prints what would be done without touching the system — useful for
validating a seed on a workstation:

```bash
tinycloudinit --seed ./myseed --dry-run
```

## Building

Builds run on `dev.g8.lo` (see `CLAUDE.md`). Release binaries are static musl
builds:

```bash
export CARGO_TARGET_DIR=/build/cargo/tinycloudinit
cargo test
cargo build --release --target x86_64-unknown-linux-musl
```

Prebuilt binaries are attached to [GitHub releases](https://github.com/glennswest/tinycloudinit/releases).
