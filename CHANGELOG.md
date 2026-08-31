# Changelog

## [Unreleased]

## [v0.1.0] — 2026-08-30

### Added
- **feat:** Initial implementation — NoCloud datasource (seed dir, `cidata`/`CIDATA` labeled block device, block-device scan with 10 s wait).
- **feat:** Run-once-per-instance semantics via `/var/lib/tinycloudinit/instance-id` (`--force` to override).
- **feat:** `#cloud-config` subset: `hostname`/`fqdn`, `manage_etc_hosts`, `users` (groups, sudo, passwd hash, ssh keys), top-level `ssh_authorized_keys` (root), `write_files` (plain/b64, permissions, owner, append), `runcmd`, `final_message`.
- **feat:** `#!` shell-script user-data support.
- **feat:** `--dry-run`, `--seed`, `--state-dir` CLI options.
- **feat:** systemd oneshot unit ordered before `sshd.service`.
- **docs:** README with seed ISO instructions and supported-keys table.
