use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

pub struct Seed {
    pub source: String,
    pub meta_data: String,
    pub user_data: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsMode {
    Auto,
    NoCloud,
    Ec2,
}

/// How long the parallel probes race in Auto mode.
const PARALLEL_WAIT: Duration = Duration::from_secs(10);

/// Locate a seed. Search order (mode Auto):
/// 1. explicit `--seed DIR`
/// 2. `<state-dir>/seed/` on the local filesystem
/// 3. (linux) one immediate pass over `cidata`/`CIDATA` labels and
///    iso9660/vfat block devices containing `meta-data`/`user-data`
/// 4. NoCloud device wait loop and EC2 IMDS (IMDSv2, v1 fallback) probed
///    in parallel for up to 10 s — the first seed found wins
pub fn find(seed_dir: Option<&str>, state_dir: &str, mode: DsMode) -> Result<Option<Seed>, String> {
    if let Some(dir) = seed_dir {
        return read_seed_dir(Path::new(dir)).map(Some);
    }
    if mode != DsMode::Ec2 {
        let local = Path::new(state_dir).join("seed");
        if local.join("meta-data").exists() || local.join("user-data").exists() {
            return read_seed_dir(&local).map(Some);
        }
    }
    match mode {
        DsMode::NoCloud => nocloud_device(Duration::from_secs(10), &AtomicBool::new(false)),
        DsMode::Ec2 => Ok(crate::ec2::fetch(Duration::from_secs(30), &AtomicBool::new(false))),
        DsMode::Auto => {
            // Fast path: a cidata device already present needs no threads.
            if let Some(seed) = nocloud_device(Duration::ZERO, &AtomicBool::new(false))? {
                return Ok(Some(seed));
            }
            race_datasources()
        }
    }
}

/// Probe NoCloud (device wait) and EC2 IMDS concurrently; first seed wins
/// and the losing probe is cancelled.
fn race_datasources() -> Result<Option<Seed>, String> {
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Result<Option<Seed>, String>>();
    {
        let tx = tx.clone();
        let cancel = Arc::clone(&cancel);
        std::thread::spawn(move || {
            let _ = tx.send(nocloud_device(PARALLEL_WAIT, &cancel));
        });
    }
    {
        let cancel = Arc::clone(&cancel);
        std::thread::spawn(move || {
            let _ = tx.send(Ok(crate::ec2::fetch(PARALLEL_WAIT, &cancel)));
        });
    }
    let mut first_err: Option<String> = None;
    for _ in 0..2 {
        match rx.recv() {
            Ok(Ok(Some(seed))) => {
                cancel.store(true, Ordering::Relaxed);
                return Ok(Some(seed));
            }
            Ok(Ok(None)) => {}
            Ok(Err(e)) => first_err = first_err.or(Some(e)),
            Err(_) => break,
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

#[cfg(target_os = "linux")]
fn nocloud_device(wait: Duration, cancel: &AtomicBool) -> Result<Option<Seed>, String> {
    linux::find_block_device(wait, cancel)
}

#[cfg(not(target_os = "linux"))]
fn nocloud_device(_wait: Duration, _cancel: &AtomicBool) -> Result<Option<Seed>, String> {
    Ok(None)
}

fn read_seed_dir(dir: &Path) -> Result<Seed, String> {
    let meta_data = fs::read_to_string(dir.join("meta-data")).unwrap_or_default();
    let user_data = fs::read_to_string(dir.join("user-data")).ok();
    if meta_data.is_empty() && user_data.is_none() {
        return Err(format!(
            "seed directory {} has neither meta-data nor user-data",
            dir.display()
        ));
    }
    Ok(Seed {
        source: dir.display().to_string(),
        meta_data,
        user_data,
    })
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{read_seed_dir, Seed};
    use std::ffi::CString;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    const MNT: &str = "/run/tinycloudinit/mnt";

    pub fn find_block_device(wait: Duration, cancel: &AtomicBool) -> Result<Option<Seed>, String> {
        fs::create_dir_all(MNT).map_err(|e| format!("mkdir {MNT}: {e}"))?;
        let mnt = Path::new(MNT);
        let deadline = Instant::now() + wait;
        loop {
            for label in ["cidata", "CIDATA"] {
                let dev = PathBuf::from("/dev/disk/by-label").join(label);
                if dev.exists() {
                    if let Some(seed) = try_device(&dev, mnt)? {
                        return Ok(Some(seed));
                    }
                }
            }
            if let Some(seed) = scan_block_devices(mnt)? {
                return Ok(Some(seed));
            }
            if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn scan_block_devices(mnt: &Path) -> Result<Option<Seed>, String> {
        let entries = match fs::read_dir("/sys/class/block") {
            Ok(e) => e,
            Err(_) => return Ok(None),
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy().to_string();
            if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram") || name.starts_with("dm-") {
                continue;
            }
            let size = fs::read_to_string(entry.path().join("size")).unwrap_or_default();
            if size.trim() == "0" || size.is_empty() {
                continue;
            }
            let dev = PathBuf::from("/dev").join(&name);
            if !dev.exists() {
                continue;
            }
            if let Some(seed) = try_device(&dev, mnt)? {
                return Ok(Some(seed));
            }
        }
        Ok(None)
    }

    /// Mount `dev` read-only and, if it carries seed files, read them.
    fn try_device(dev: &Path, mnt: &Path) -> Result<Option<Seed>, String> {
        for fstype in ["iso9660", "vfat"] {
            if !mount_ro(dev, mnt, fstype) {
                continue;
            }
            let has_seed = mnt.join("meta-data").exists() || mnt.join("user-data").exists();
            let result = if has_seed {
                match read_seed_dir(mnt) {
                    Ok(mut seed) => {
                        seed.source = format!("{} ({})", dev.display(), fstype);
                        Some(seed)
                    }
                    Err(_) => None,
                }
            } else {
                None
            };
            umount(mnt);
            if result.is_some() {
                return Ok(result);
            }
        }
        Ok(None)
    }

    fn cstr(p: &Path) -> CString {
        CString::new(p.as_os_str().as_bytes()).expect("path with NUL")
    }

    fn mount_ro(dev: &Path, mnt: &Path, fstype: &str) -> bool {
        let d = cstr(dev);
        let m = cstr(mnt);
        let f = CString::new(fstype).unwrap();
        let flags = libc::MS_RDONLY | libc::MS_NODEV | libc::MS_NOSUID;
        unsafe { libc::mount(d.as_ptr(), m.as_ptr(), f.as_ptr(), flags, std::ptr::null()) == 0 }
    }

    fn umount(mnt: &Path) {
        let m = cstr(mnt);
        unsafe {
            libc::umount(m.as_ptr());
        }
    }
}
