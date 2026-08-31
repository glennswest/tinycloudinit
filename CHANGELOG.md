# Changelog

## [Unreleased]

## [v0.2.0] — 2026-08-30

### Added
- **feat:** EC2 IMDS datasource — IMDSv2 session token with IMDSv1 fallback, fetching `instance-id`, `local-hostname`, and `user-data` from `169.254.169.254`; built-in minimal HTTP/1.1 client (no new dependencies), Content-Length and chunked responses supported.
- **feat:** `--datasource auto|nocloud|ec2` CLI flag; auto order is seed dir → local seed → NoCloud device (immediate pass) → EC2 IMDS (~5 s) → NoCloud device wait (10 s).

### Changed
- **refactor:** systemd unit now waits for `network-online.target` (required for IMDS) and no longer orders before `network-pre.target`.

### Documentation
- **docs:** README updated with EC2 datasource, `--datasource` flag, and revised search order.

## [v0.1.0] — 2026-08-30

### Added
- **feat:** Initial implementation — NoCloud datasource (seed dir, `cidata`/`CIDATA` labeled block device, block-device scan with 10 s wait).
- **feat:** Run-once-per-instance semantics via `/var/lib/tinycloudinit/instance-id` (`--force` to override).
- **feat:** `#cloud-config` subset: `hostname`/`fqdn`, `manage_etc_hosts`, `users` (groups, sudo, passwd hash, ssh keys), top-level `ssh_authorized_keys` (root), `write_files` (plain/b64, permissions, owner, append), `runcmd`, `final_message`.
- **feat:** `#!` shell-script user-data support.
- **feat:** `--dry-run`, `--seed`, `--state-dir` CLI options.
- **feat:** systemd oneshot unit ordered before `sshd.service`.
- **docs:** README with seed ISO instructions and supported-keys table.
