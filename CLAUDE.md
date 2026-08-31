# tinycloudinit — Project Context

Tiny cloud-init replacement in Rust for small Fedora images. NoCloud
datasource + `#cloud-config` subset. Single static musl binary.

## Version

- Current: **0.1.0**
- Version locations: `Cargo.toml` only (binary reports it via `CARGO_PKG_VERSION`).

## Build & Release

All builds run on `root@dev.g8.lo` (never on the Mac):

```bash
ssh root@dev.g8.lo
cd /root/tinycloudinit && git pull
export CARGO_TARGET_DIR=/build/cargo/tinycloudinit
cargo test
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl   # if cross linker available
```

Release process:
1. Bump version in `Cargo.toml`, update `CHANGELOG.md`, commit `chore(release): vX.Y.Z`.
2. Build release binaries on dev (musl, static).
3. Package: `tinycloudinit-vX.Y.Z-<arch>-linux-musl.tar.gz` containing the
   binary + `systemd/tinycloudinit.service` + `README.md`.
4. `gh release create vX.Y.Z <tarballs> --title vX.Y.Z --notes ...` (creates the tag).

Build assets are staged on dev under `/build/assets/tinycloudinit/`.

## Architecture

- `src/main.rs` — CLI, instance-id gate, orchestration.
- `src/datasource.rs` — seed discovery; linux-only mount(2) code behind `cfg(target_os = "linux")`.
- `src/config.rs` — serde structs for meta-data and cloud-config; parsing helpers + tests.
- `src/apply.rs` — modules: hostname, /etc/hosts, users, sudoers, ssh keys, write_files, runcmd, user scripts.
- `systemd/tinycloudinit.service` — oneshot, before sshd.

External tools used on the target: `useradd`, `usermod`, `chpasswd` (shadow-utils), `/bin/sh`.

## Work Plan

- [x] v0.1.0: initial implementation, tests, systemd unit, README
- [x] Build + test on dev.g8.lo
- [x] GitHub release with binary assets
- [ ] Ideas / not planned: growpart support, network-config v2 subset, EC2 IMDS datasource, package module
