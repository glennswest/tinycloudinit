//! Grow a partition to fill its disk, then grow the filesystem on it.
//!
//! The partition-table editing (GPT and MBR) is implemented natively so the
//! target image needs neither cloud-utils-growpart nor sgdisk. The kernel is
//! told about the new size with the BLKPG resize ioctl (works while the
//! partition is mounted), then the filesystem is grown with resize2fs /
//! xfs_growfs / btrfs depending on what is mounted.
//!
//! The table editing operates on any file, which is how the tests exercise
//! it against sfdisk-created images without touching a real disk.

use crate::config::{CloudConfig, GrowpartVal};
use crate::Ctx;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

type Result<T> = std::result::Result<T, String>;

#[derive(Debug, PartialEq)]
pub struct GrowReport {
    pub partnum: u32,
    /// sectors
    pub start: u64,
    pub old_end: u64,
    pub new_end: u64,
    pub sector: u64,
}

/// Entry point from the apply phase. Defaults match cloud-init: grow "/",
/// resize the filesystem, unless `growpart` turns it off. Failures are
/// logged, never fatal — a boot must not be lost to a full disk staying full.
pub fn run(cfg: Option<&CloudConfig>, ctx: &Ctx) {
    let default_devices = vec!["/".to_string()];
    let (off, devices) = match cfg.and_then(|c| c.growpart.as_ref()) {
        None => (false, default_devices),
        Some(GrowpartVal::Enabled(e)) => (!e, default_devices),
        Some(GrowpartVal::Cfg(g)) => (
            g.is_off(),
            if g.devices.is_empty() {
                default_devices
            } else {
                g.devices.clone()
            },
        ),
    };
    if off {
        println!("tinycloudinit: growpart disabled");
        return;
    }
    let resize_fs = cfg.map_or(true, |c| c.resize_rootfs_enabled());
    for dev in &devices {
        if let Err(e) = grow_target(dev, resize_fs, ctx) {
            eprintln!("tinycloudinit: growpart {dev}: {e}");
        }
    }
}

/// `--grow` CLI entry point.
pub fn standalone(target: &str, dry_run: bool) -> Result<()> {
    let ctx = Ctx {
        dry_run,
        state_dir: std::path::PathBuf::from("/var/lib/tinycloudinit"),
    };
    grow_target(target, true, &ctx)
}

// ---- portable partition-table core ------------------------------------

/// Grow partition `partnum` (1-based) of the disk (device or image file) at
/// `path` to fill the available space. With `commit` false nothing is
/// written. Returns None when there is nothing to gain.
pub fn grow_partition(path: &Path, partnum: u32, commit: bool) -> Result<Option<GrowReport>> {
    let f = OpenOptions::new()
        .read(true)
        .write(commit)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let (sector, total_bytes) = disk_geometry(&f)?;
    if sector == 0 || total_bytes < sector * 3 {
        return Err("disk too small or unreadable geometry".into());
    }
    let total = total_bytes / sector;
    let mut lba1 = vec![0u8; sector as usize];
    f.read_exact_at(&mut lba1, sector)
        .map_err(|e| format!("read GPT header: {e}"))?;
    if &lba1[0..8] == b"EFI PART" {
        return grow_gpt(&f, &lba1, sector, total, partnum, commit);
    }
    let mut lba0 = vec![0u8; sector as usize];
    f.read_exact_at(&mut lba0, 0)
        .map_err(|e| format!("read MBR: {e}"))?;
    if lba0[510] == 0x55 && lba0[511] == 0xAA {
        return grow_mbr(&f, &mut lba0, total, sector, partnum, commit);
    }
    Err("no GPT or MBR partition table found".into())
}

fn grow_gpt(
    f: &File,
    lba1: &[u8],
    sector: u64,
    total: u64,
    partnum: u32,
    commit: bool,
) -> Result<Option<GrowReport>> {
    let hdr_size = ru32(lba1, 12) as usize;
    if !(92..=sector as usize).contains(&hdr_size) {
        return Err(format!("implausible GPT header size {hdr_size}"));
    }
    let stored_crc = ru32(lba1, 16);
    let mut hdr = lba1[..hdr_size].to_vec();
    hdr[16..20].fill(0);
    if crc32(&hdr) != stored_crc {
        return Err("primary GPT header CRC mismatch — refusing to touch the disk".into());
    }
    let entries_lba = ru64(lba1, 72);
    let num_entries = ru32(lba1, 80);
    let esize = ru32(lba1, 84) as usize;
    let entries_bytes = num_entries as usize * esize;
    if esize < 128 || entries_bytes == 0 || entries_bytes > 1 << 20 {
        return Err(format!(
            "implausible GPT entry layout ({num_entries} x {esize})"
        ));
    }
    let entries_sectors = (entries_bytes as u64).div_ceil(sector);
    let mut entries = vec![0u8; (entries_sectors * sector) as usize];
    f.read_exact_at(&mut entries[..entries_bytes], entries_lba * sector)
        .map_err(|e| format!("read GPT entries: {e}"))?;
    if crc32(&entries[..entries_bytes]) != ru32(lba1, 88) {
        return Err("GPT entries CRC mismatch — refusing to touch the disk".into());
    }

    let idx = partnum
        .checked_sub(1)
        .filter(|i| *i < num_entries)
        .ok_or_else(|| format!("no partition {partnum}"))? as usize;
    let off = idx * esize;
    if entries[off..off + 16].iter().all(|b| *b == 0) {
        return Err(format!("partition {partnum} is empty"));
    }
    let start = ru64(&entries, off + 32);
    let old_end = ru64(&entries, off + 40);
    for i in 0..num_entries as usize {
        let o = i * esize;
        if i != idx && !entries[o..o + 16].iter().all(|b| *b == 0) && ru64(&entries, o + 32) > old_end
        {
            return Err(format!(
                "partition {partnum} is not the last partition; not growing"
            ));
        }
    }
    let new_last_usable = total
        .checked_sub(2 + entries_sectors)
        .ok_or("disk too small for GPT")?;
    if new_last_usable <= old_end {
        return Ok(None);
    }
    let report = GrowReport {
        partnum,
        start,
        old_end,
        new_end: new_last_usable,
        sector,
    };
    if !commit {
        return Ok(Some(report));
    }

    wu64(&mut entries, off + 40, new_last_usable);
    let ecrc = crc32(&entries[..entries_bytes]);

    let backup_entries_lba = total - 1 - entries_sectors;
    let mut primary = lba1.to_vec();
    wu64(&mut primary, 32, total - 1); // alternate LBA
    wu64(&mut primary, 48, new_last_usable);
    wu32(&mut primary, 88, ecrc);
    seal_gpt_crc(&mut primary, hdr_size);

    let mut backup = primary.clone();
    wu64(&mut backup, 24, total - 1); // current LBA
    wu64(&mut backup, 32, 1); // alternate points at primary
    wu64(&mut backup, 72, backup_entries_lba);
    seal_gpt_crc(&mut backup, hdr_size);

    // Backup structures first: if we die mid-write the primary is intact.
    let werr = |what: &str| move |e: std::io::Error| format!("write {what}: {e}");
    f.write_all_at(&entries, backup_entries_lba * sector)
        .map_err(werr("backup GPT entries"))?;
    f.write_all_at(&backup, (total - 1) * sector)
        .map_err(werr("backup GPT header"))?;
    f.write_all_at(&entries, entries_lba * sector)
        .map_err(werr("GPT entries"))?;
    f.write_all_at(&primary, sector)
        .map_err(werr("GPT header"))?;
    f.sync_all().map_err(|e| format!("sync: {e}"))?;
    Ok(Some(report))
}

fn seal_gpt_crc(hdr: &mut [u8], hdr_size: usize) {
    hdr[16..20].fill(0);
    let c = crc32(&hdr[..hdr_size]);
    wu32(hdr, 16, c);
}

fn grow_mbr(
    f: &File,
    lba0: &mut [u8],
    total: u64,
    sector: u64,
    partnum: u32,
    commit: bool,
) -> Result<Option<GrowReport>> {
    if !(1..=4).contains(&partnum) {
        return Err(format!(
            "MBR partition {partnum}: only primary partitions 1-4 supported"
        ));
    }
    let off = 446 + 16 * (partnum as usize - 1);
    let ptype = lba0[off + 4];
    if ptype == 0 {
        return Err(format!("partition {partnum} is empty"));
    }
    if ptype == 0x05 || ptype == 0x0f {
        return Err(format!("partition {partnum} is an extended partition; not growing"));
    }
    let start = ru32(lba0, off + 8) as u64;
    let size = ru32(lba0, off + 12) as u64;
    if start == 0 || size == 0 {
        return Err(format!("partition {partnum} has no LBA geometry"));
    }
    let old_end = start + size - 1;
    for i in 0..4u32 {
        let o = 446 + 16 * i as usize;
        if i + 1 != partnum && lba0[o + 4] != 0 && ru32(lba0, o + 8) as u64 > old_end {
            return Err(format!(
                "partition {partnum} is not the last partition; not growing"
            ));
        }
    }
    let new_size = (total - start).min(u32::MAX as u64);
    if new_size <= size {
        return Ok(None);
    }
    let report = GrowReport {
        partnum,
        start,
        old_end,
        new_end: start + new_size - 1,
        sector,
    };
    if !commit {
        return Ok(Some(report));
    }
    wu32(lba0, off + 12, new_size as u32);
    f.write_all_at(lba0, 0).map_err(|e| format!("write MBR: {e}"))?;
    f.sync_all().map_err(|e| format!("sync: {e}"))?;
    Ok(Some(report))
}

// ---- byte helpers ------------------------------------------------------

fn ru32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn ru64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn wu32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn wu64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// CRC-32 (IEEE 802.3), as used by GPT.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = !0;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

// ---- geometry ----------------------------------------------------------

#[cfg(target_os = "linux")]
fn disk_geometry(f: &File) -> Result<(u64, u64)> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileTypeExt;
    let md = f.metadata().map_err(|e| format!("stat: {e}"))?;
    if md.file_type().is_block_device() {
        const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
        const BLKSSZGET: libc::c_ulong = 0x1268;
        let mut bytes: u64 = 0;
        let mut ssz: libc::c_int = 0;
        let fd = f.as_raw_fd();
        if unsafe { libc::ioctl(fd, BLKGETSIZE64, &mut bytes) } != 0
            || unsafe { libc::ioctl(fd, BLKSSZGET, &mut ssz) } != 0
        {
            return Err(format!("block ioctl: {}", std::io::Error::last_os_error()));
        }
        return Ok((ssz as u64, bytes));
    }
    Ok((512, md.len()))
}

#[cfg(not(target_os = "linux"))]
fn disk_geometry(f: &File) -> Result<(u64, u64)> {
    let md = f.metadata().map_err(|e| format!("stat: {e}"))?;
    Ok((512, md.len()))
}

// ---- linux plumbing: resolve, kernel notify, fs resize -----------------

#[cfg(target_os = "linux")]
fn grow_target(target: &str, resize_fs: bool, ctx: &Ctx) -> Result<()> {
    let (disk, partnum, partdev, mount) = resolve_target(target)?;
    let disk_path = Path::new(&disk);
    let report = match grow_partition(disk_path, partnum, false)? {
        Some(r) => r,
        None => {
            println!("tinycloudinit: growpart {target}: {partdev} already fills {disk}");
            return Ok(());
        }
    };
    let gain_mib = (report.new_end - report.old_end) * report.sector / (1 << 20);
    if ctx.dry_run {
        println!(
            "DRY: would grow {partdev} (partition {partnum} of {disk}) by {gain_mib} MiB and resize the filesystem"
        );
        return Ok(());
    }
    grow_partition(disk_path, partnum, true)?;
    println!("tinycloudinit: grew {partdev} by {gain_mib} MiB (end {} -> {})", report.old_end, report.new_end);
    if let Err(e) = kernel_resize(&disk, &report) {
        eprintln!("tinycloudinit: growpart {target}: kernel not updated ({e}); a reboot may be needed before the filesystem can grow");
    }
    if resize_fs {
        match &mount {
            Some((mountpoint, fstype)) => resize_filesystem(&partdev, mountpoint, fstype, ctx),
            None => {
                println!("tinycloudinit: growpart {target}: {partdev} not mounted; skipping filesystem resize");
                Ok(())
            }
        }?;
    }
    Ok(())
}

/// Resolve "/" (or any mountpoint / partition device) to
/// (disk device, partition number, partition device, Some((mountpoint, fstype))).
#[cfg(target_os = "linux")]
fn resolve_target(target: &str) -> Result<(String, u32, String, Option<(String, String)>)> {
    use std::fs;
    let mountinfo =
        fs::read_to_string("/proc/self/mountinfo").map_err(|e| format!("mountinfo: {e}"))?;
    // A mountpoint target: find its majmin + fstype from mountinfo.
    let mut majmin: Option<String> = None;
    let mut mount: Option<(String, String)> = None;
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let Some(sep) = fields.iter().position(|f| *f == "-") else {
            continue;
        };
        if fields.len() < 5 || fields.len() < sep + 3 {
            continue;
        }
        let (mp, fstype, source) = (fields[4], fields[sep + 1], fields[sep + 2]);
        if mp == target || source == target {
            majmin = Some(fields[2].to_string());
            mount = Some((mp.to_string(), fstype.to_string()));
            break;
        }
    }
    let sys = match (&majmin, target.strip_prefix("/dev/")) {
        (Some(mm), _) => format!("/sys/dev/block/{mm}"),
        (None, Some(name)) => format!("/sys/class/block/{name}"),
        (None, None) => return Err(format!("'{target}' is not mounted and is not a device")),
    };
    let sys = std::fs::canonicalize(&sys).map_err(|e| format!("resolve {sys}: {e}"))?;
    let partnum: u32 = std::fs::read_to_string(sys.join("partition"))
        .map_err(|_| {
            format!(
                "{} is not a partition (LVM/RAID/whole-disk roots are not supported)",
                sys.display()
            )
        })?
        .trim()
        .parse()
        .map_err(|e| format!("partition number: {e}"))?;
    let partname = sys
        .file_name()
        .ok_or("bad sysfs path")?
        .to_string_lossy()
        .into_owned();
    let diskname = sys
        .parent()
        .and_then(|p| p.file_name())
        .ok_or("no parent disk in sysfs")?
        .to_string_lossy()
        .into_owned();
    Ok((format!("/dev/{diskname}"), partnum, format!("/dev/{partname}"), mount))
}

/// Tell the kernel the partition's new size (BLKPG resize — allowed while
/// the partition is mounted).
#[cfg(target_os = "linux")]
fn kernel_resize(disk: &str, r: &GrowReport) -> Result<()> {
    use std::os::fd::AsRawFd;
    const BLKPG: libc::c_ulong = 0x1269;
    const BLKPG_RESIZE_PARTITION: libc::c_int = 3;
    #[repr(C)]
    struct BlkpgPartition {
        start: i64,
        length: i64,
        pno: libc::c_int,
        devname: [u8; 64],
        volname: [u8; 64],
    }
    #[repr(C)]
    struct BlkpgIoctlArg {
        op: libc::c_int,
        flags: libc::c_int,
        datalen: libc::c_int,
        data: *mut libc::c_void,
    }
    let f = OpenOptions::new()
        .read(true)
        .open(disk)
        .map_err(|e| format!("open {disk}: {e}"))?;
    let mut part = BlkpgPartition {
        start: (r.start * r.sector) as i64,
        length: ((r.new_end - r.start + 1) * r.sector) as i64,
        pno: r.partnum as libc::c_int,
        devname: [0; 64],
        volname: [0; 64],
    };
    let mut arg = BlkpgIoctlArg {
        op: BLKPG_RESIZE_PARTITION,
        flags: 0,
        datalen: std::mem::size_of::<BlkpgPartition>() as libc::c_int,
        data: &mut part as *mut _ as *mut libc::c_void,
    };
    if unsafe { libc::ioctl(f.as_raw_fd(), BLKPG, &mut arg) } != 0 {
        return Err(format!("BLKPG resize: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn resize_filesystem(partdev: &str, mountpoint: &str, fstype: &str, ctx: &Ctx) -> Result<()> {
    let (prog, args): (&str, Vec<&str>) = match fstype {
        "ext2" | "ext3" | "ext4" => ("resize2fs", vec![partdev]),
        "xfs" => ("xfs_growfs", vec![mountpoint]),
        "btrfs" => ("btrfs", vec!["filesystem", "resize", "max", mountpoint]),
        other => {
            println!("tinycloudinit: growpart: no resize support for {other}; partition grown, filesystem untouched");
            return Ok(());
        }
    };
    if ctx.dry_run {
        println!("DRY: {prog} {}", args.join(" "));
        return Ok(());
    }
    let st = std::process::Command::new(prog)
        .args(&args)
        .status()
        .map_err(|e| format!("run {prog}: {e}"))?;
    if st.success() {
        println!("tinycloudinit: filesystem on {partdev} resized ({fstype})");
        Ok(())
    } else {
        Err(format!("{prog} exited with {st}"))
    }
}

#[cfg(not(target_os = "linux"))]
fn grow_target(target: &str, _resize_fs: bool, _ctx: &Ctx) -> Result<()> {
    Err(format!("growpart {target}: only supported on linux"))
}

// ---- tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn crc32_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    fn mbr_image(total_sectors: u64, parts: &[(u8, u32, u32)]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "tci-mbr-{}-{}.img",
            std::process::id(),
            parts.len()
        ));
        let mut lba0 = vec![0u8; 512];
        for (i, (ptype, start, size)) in parts.iter().enumerate() {
            let off = 446 + 16 * i;
            lba0[off + 4] = *ptype;
            wu32(&mut lba0, off + 8, *start);
            wu32(&mut lba0, off + 12, *size);
        }
        lba0[510] = 0x55;
        lba0[511] = 0xAA;
        let mut f = File::create(&p).unwrap();
        f.write_all(&lba0).unwrap();
        f.set_len(total_sectors * 512).unwrap();
        p
    }

    #[test]
    fn mbr_grow_last_partition() {
        let img = mbr_image(20480, &[(0x83, 2048, 4096)]);
        let r = grow_partition(&img, 1, true).unwrap().unwrap();
        assert_eq!(r.old_end, 2048 + 4096 - 1);
        assert_eq!(r.new_end, 20479);
        // idempotent
        assert!(grow_partition(&img, 1, true).unwrap().is_none());
        std::fs::remove_file(&img).ok();
    }

    #[test]
    fn mbr_refuses_non_last() {
        let img = mbr_image(20480, &[(0x83, 2048, 2048), (0x83, 8192, 2048)]);
        let err = grow_partition(&img, 1, true).unwrap_err();
        assert!(err.contains("not the last"), "{err}");
        std::fs::remove_file(&img).ok();
    }

    #[test]
    fn mbr_dry_run_writes_nothing() {
        let img = mbr_image(20480, &[(0x83, 2048, 4096)]);
        let before = std::fs::read(&img).unwrap();
        let r = grow_partition(&img, 1, false).unwrap().unwrap();
        assert_eq!(r.new_end, 20479);
        assert_eq!(std::fs::read(&img).unwrap(), before);
        std::fs::remove_file(&img).ok();
    }

    /// Full GPT round-trip cross-checked against util-linux: sfdisk creates
    /// the table, we grow it after the "disk" is enlarged, sfdisk verifies.
    #[cfg(target_os = "linux")]
    #[test]
    fn gpt_grow_verified_by_sfdisk() {
        use std::process::{Command, Stdio};
        let have_sfdisk = Command::new("sfdisk")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !have_sfdisk {
            eprintln!("sfdisk not available; skipping");
            return;
        }
        let img = std::env::temp_dir().join(format!("tci-gpt-{}.img", std::process::id()));
        File::create(&img).unwrap().set_len(64 << 20).unwrap();
        let mut child = Command::new("sfdisk")
            .arg("--no-reread")
            .arg("--no-tell-kernel")
            .arg(&img)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"label: gpt\nstart=2048, size=32768, type=L\n")
            .unwrap();
        assert!(child.wait().unwrap().success());

        // "Copy the image to a bigger disk", then grow.
        OpenOptions::new()
            .write(true)
            .open(&img)
            .unwrap()
            .set_len(128 << 20)
            .unwrap();
        let r = grow_partition(&img, 1, true).unwrap().unwrap();
        let total = (128 << 20) / 512u64;
        assert_eq!(r.new_end, total - 34);
        assert!(grow_partition(&img, 1, true).unwrap().is_none(), "idempotent");

        let verify = Command::new("sfdisk").arg("--verify").arg(&img).output().unwrap();
        assert!(
            verify.status.success(),
            "sfdisk --verify failed: {}{}",
            String::from_utf8_lossy(&verify.stdout),
            String::from_utf8_lossy(&verify.stderr)
        );
        let dump = Command::new("sfdisk").arg("-d").arg(&img).output().unwrap();
        let dump = String::from_utf8_lossy(&dump.stdout).into_owned();
        assert!(
            dump.contains(&format!("size= {}", r.new_end - 2048 + 1))
                || dump.contains(&format!("size={}", r.new_end - 2048 + 1)),
            "unexpected sfdisk dump:\n{dump}"
        );
        std::fs::remove_file(&img).ok();
    }

    #[test]
    fn rejects_blank_disk() {
        let p = std::env::temp_dir().join(format!("tci-blank-{}.img", std::process::id()));
        File::create(&p).unwrap().set_len(1 << 20).unwrap();
        assert!(grow_partition(&p, 1, true).is_err());
        std::fs::remove_file(&p).ok();
    }
}
