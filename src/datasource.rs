use std::fs;
use std::path::Path;

pub struct Seed {
    pub source: String,
    pub meta_data: String,
    pub user_data: Option<String>,
}

/// Locate a NoCloud seed. Search order:
/// 1. explicit `--seed DIR`
/// 2. `<state-dir>/seed/` on the local filesystem
/// 3. (linux) a block device with filesystem label `cidata`/`CIDATA`
/// 4. (linux) any iso9660/vfat block device containing `meta-data`/`user-data`
pub fn find(seed_dir: Option<&str>, state_dir: &str) -> Result<Option<Seed>, String> {
    if let Some(dir) = seed_dir {
        return read_seed_dir(Path::new(dir)).map(Some);
    }
    let local = Path::new(state_dir).join("seed");
    if local.join("meta-data").exists() || local.join("user-data").exists() {
        return read_seed_dir(&local).map(Some);
    }
    #[cfg(target_os = "linux")]
    return linux::find_block_device();
    #[cfg(not(target_os = "linux"))]
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
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    const MNT: &str = "/run/tinycloudinit/mnt";
    const WAIT: Duration = Duration::from_secs(10);

    pub fn find_block_device() -> Result<Option<Seed>, String> {
        fs::create_dir_all(MNT).map_err(|e| format!("mkdir {MNT}: {e}"))?;
        let mnt = Path::new(MNT);
        let deadline = Instant::now() + WAIT;
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
            if Instant::now() >= deadline {
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
