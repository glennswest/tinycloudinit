# tinycloudinit

A tiny cloud-init replacement in Rust for small Fedora (and other Linux)
images. Single static binary (~1 MB, musl), no Python, no dependencies on the
target beyond `useradd`/`chpasswd` from shadow-utils. Implements the NoCloud
datasource and the most-used subset of `#cloud-config`.

## What it does

On boot (systemd oneshot, before `sshd`):

1. **Finds a seed**, in order:
   - `--seed <dir>` on the command line
   - `/var/lib/tinycloudinit/seed/` on the local filesystem
   - a block device with filesystem label `cidata`/`CIDATA` (iso9660 or vfat) —
     waits up to 10 s for the device to appear
   - any iso9660/vfat block device containing `meta-data`/`user-data`
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

Not implemented (by design, keep it tiny): network config (use DHCP /
NetworkManager), package installation, growpart, multi-part MIME, vendor-data,
network datasources (EC2 IMDS etc.).

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
tinycloudinit [--seed DIR] [--state-dir DIR] [--dry-run] [--force] [--version]
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
