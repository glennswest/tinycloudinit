use crate::config::{
    decode_content, parse_mode, CloudConfig, Cmd, MetaData, SudoVal, UserEntry, UserSpec, WriteFile,
};
use crate::datasource::Seed;
use crate::Ctx;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

type Result<T> = std::result::Result<T, String>;

pub fn apply(seed: &Seed, meta: &MetaData, ctx: &Ctx) -> Result<()> {
    let user_data = seed.user_data.as_deref().unwrap_or("");
    let trimmed = user_data.trim_start();
    if trimmed.starts_with("#cloud-config") {
        let cfg: CloudConfig = serde_yaml::from_str(user_data)
            .map_err(|e| format!("user-data: invalid cloud-config: {e}"))?;
        apply_cloud_config(&cfg, meta, ctx)
    } else if trimmed.starts_with("#!") {
        crate::growpart::run(None, ctx);
        apply_hostname(None, None, meta, ctx)?;
        run_user_script(user_data, ctx)
    } else {
        if !trimmed.is_empty() {
            println!("tinycloudinit: unrecognized user-data format; ignoring");
        }
        crate::growpart::run(None, ctx);
        apply_hostname(None, None, meta, ctx)
    }
}

fn apply_cloud_config(cfg: &CloudConfig, meta: &MetaData, ctx: &Ctx) -> Result<()> {
    crate::growpart::run(Some(cfg), ctx);
    apply_hostname(cfg.hostname.as_deref(), cfg.fqdn.as_deref(), meta, ctx)?;
    if cfg.manage_etc_hosts.unwrap_or(false) {
        write_etc_hosts(cfg, meta, ctx)?;
    }
    for entry in &cfg.users {
        apply_user(entry, ctx)?;
    }
    if !cfg.ssh_authorized_keys.is_empty() {
        install_ssh_keys("root", &cfg.ssh_authorized_keys, ctx)?;
    }
    for f in &cfg.write_files {
        write_file(f, ctx)?;
    }
    for (i, c) in cfg.runcmd.iter().enumerate() {
        run_cmd(i, c, ctx);
    }
    if let Some(msg) = &cfg.final_message {
        println!("{msg}");
    }
    Ok(())
}

// ---- hostname ----------------------------------------------------------

fn apply_hostname(hostname: Option<&str>, fqdn: Option<&str>, meta: &MetaData, ctx: &Ctx) -> Result<()> {
    let short_from_fqdn = fqdn.map(|f| f.split('.').next().unwrap_or(f));
    let chosen = hostname
        .or(short_from_fqdn)
        .or(meta.local_hostname.as_deref());
    let Some(name) = chosen else { return Ok(()) };
    let name = name.trim();
    if name.is_empty() {
        return Ok(());
    }
    if ctx.dry_run {
        println!("DRY: would set hostname to '{name}'");
        return Ok(());
    }
    fs::write("/etc/hostname", format!("{name}\n")).map_err(|e| format!("write /etc/hostname: {e}"))?;
    sethostname(name).map_err(|e| format!("sethostname({name}): {e}"))?;
    println!("tinycloudinit: hostname set to '{name}'");
    Ok(())
}

fn write_etc_hosts(cfg: &CloudConfig, meta: &MetaData, ctx: &Ctx) -> Result<()> {
    let short = cfg
        .hostname
        .as_deref()
        .or(cfg.fqdn.as_deref().map(|f| f.split('.').next().unwrap_or(f)))
        .or(meta.local_hostname.as_deref())
        .unwrap_or("localhost");
    let fqdn = cfg.fqdn.as_deref().unwrap_or(short);
    let content = format!(
        "127.0.0.1   localhost localhost.localdomain\n::1         localhost localhost.localdomain\n127.0.1.1   {fqdn} {short}\n"
    );
    if ctx.dry_run {
        println!("DRY: would write /etc/hosts for {fqdn}");
        return Ok(());
    }
    fs::write("/etc/hosts", content).map_err(|e| format!("write /etc/hosts: {e}"))
}

#[cfg(target_os = "linux")]
fn sethostname(name: &str) -> std::io::Result<()> {
    let r = unsafe { libc::sethostname(name.as_ptr() as *const libc::c_char, name.len()) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn sethostname(_name: &str) -> std::io::Result<()> {
    Ok(())
}

// ---- users -------------------------------------------------------------

fn apply_user(entry: &UserEntry, ctx: &Ctx) -> Result<()> {
    let spec: UserSpec = match entry {
        UserEntry::Name(n) if n == "default" => {
            println!("tinycloudinit: users: 'default' entry ignored (no default user concept)");
            return Ok(());
        }
        UserEntry::Name(n) => UserSpec {
            name: n.clone(),
            ..Default::default()
        },
        UserEntry::Spec(s) => s.clone(),
    };
    if spec.name.is_empty() {
        println!("tinycloudinit: users: entry without name ignored");
        return Ok(());
    }
    let name = spec.name.as_str();

    if passwd_entry(name).is_none() {
        let mut args: Vec<String> = Vec::new();
        if spec.system.unwrap_or(false) {
            args.push("--system".into());
        } else {
            args.push("-m".into());
        }
        if let Some(g) = &spec.gecos {
            args.push("-c".into());
            args.push(g.clone());
        }
        if let Some(s) = &spec.shell {
            args.push("-s".into());
            args.push(s.clone());
        }
        if let Some(h) = &spec.homedir {
            args.push("-d".into());
            args.push(h.clone());
        }
        if let Some(g) = &spec.groups {
            let joined = g.joined();
            if !joined.is_empty() {
                args.push("-G".into());
                args.push(joined);
            }
        }
        args.push(name.to_string());
        run_tool(ctx, "useradd", &args)?;
    } else {
        println!("tinycloudinit: user '{name}' already exists");
    }

    if let Some(hash) = &spec.passwd {
        chpasswd(name, hash, ctx)?;
        if spec.lock_passwd == Some(false) {
            run_tool(ctx, "usermod", &["-U".into(), name.to_string()])?;
        }
    }

    match &spec.sudo {
        Some(SudoVal::Line(line)) => write_sudoers(name, std::slice::from_ref(line), ctx)?,
        Some(SudoVal::Lines(lines)) => write_sudoers(name, lines, ctx)?,
        Some(SudoVal::Enabled(_)) | None => {}
    }

    if !spec.ssh_authorized_keys.is_empty() {
        install_ssh_keys(name, &spec.ssh_authorized_keys, ctx)?;
    }
    Ok(())
}

fn write_sudoers(name: &str, lines: &[String], ctx: &Ctx) -> Result<()> {
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let path = format!("/etc/sudoers.d/90-tinycloudinit-{safe}");
    let mut content = String::new();
    for l in lines {
        content.push_str(&format!("{name} {l}\n"));
    }
    if ctx.dry_run {
        println!("DRY: would write {path}");
        return Ok(());
    }
    fs::create_dir_all("/etc/sudoers.d").map_err(|e| format!("mkdir /etc/sudoers.d: {e}"))?;
    fs::write(&path, content).map_err(|e| format!("write {path}: {e}"))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o440))
        .map_err(|e| format!("chmod {path}: {e}"))?;
    println!("tinycloudinit: sudoers rule installed for '{name}'");
    Ok(())
}

fn chpasswd(name: &str, hash: &str, ctx: &Ctx) -> Result<()> {
    if ctx.dry_run {
        println!("DRY: would set password hash for '{name}'");
        return Ok(());
    }
    use std::io::Write;
    let mut child = Command::new("chpasswd")
        .arg("-e")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn chpasswd: {e}"))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{name}:{hash}\n").as_bytes())
        .map_err(|e| format!("chpasswd stdin: {e}"))?;
    let st = child.wait().map_err(|e| format!("chpasswd wait: {e}"))?;
    if st.success() {
        println!("tinycloudinit: password set for '{name}'");
        Ok(())
    } else {
        Err(format!("chpasswd -e for '{name}' exited with {st}"))
    }
}

// ---- ssh keys ----------------------------------------------------------

fn install_ssh_keys(user: &str, keys: &[String], ctx: &Ctx) -> Result<()> {
    if ctx.dry_run {
        println!("DRY: would install {} ssh key(s) for '{user}'", keys.len());
        return Ok(());
    }
    let (uid, gid, home) =
        passwd_entry(user).ok_or_else(|| format!("user '{user}' not found for ssh keys"))?;
    let sshdir = Path::new(&home).join(".ssh");
    fs::create_dir_all(&sshdir).map_err(|e| format!("mkdir {}: {e}", sshdir.display()))?;
    fs::set_permissions(&sshdir, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("chmod {}: {e}", sshdir.display()))?;
    let ak = sshdir.join("authorized_keys");
    let mut content = fs::read_to_string(&ak).unwrap_or_default();
    let mut added = 0;
    for key in keys {
        let key = key.trim();
        if key.is_empty() || content.lines().any(|l| l.trim() == key) {
            continue;
        }
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(key);
        content.push('\n');
        added += 1;
    }
    fs::write(&ak, content).map_err(|e| format!("write {}: {e}", ak.display()))?;
    fs::set_permissions(&ak, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod {}: {e}", ak.display()))?;
    std::os::unix::fs::chown(&sshdir, Some(uid), Some(gid))
        .map_err(|e| format!("chown {}: {e}", sshdir.display()))?;
    std::os::unix::fs::chown(&ak, Some(uid), Some(gid))
        .map_err(|e| format!("chown {}: {e}", ak.display()))?;
    println!("tinycloudinit: installed {added} ssh key(s) for '{user}'");
    Ok(())
}

// ---- write_files -------------------------------------------------------

fn write_file(f: &WriteFile, ctx: &Ctx) -> Result<()> {
    if f.path.is_empty() {
        return Err("write_files entry without path".into());
    }
    let data = decode_content(&f.content, f.encoding.as_deref())?;
    let mode = parse_mode(f.permissions.as_ref())?;
    if ctx.dry_run {
        println!("DRY: would write {} ({} bytes, mode {:o})", f.path, data.len(), mode);
        return Ok(());
    }
    let path = Path::new(&f.path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    if f.append {
        use std::io::Write;
        let mut fh = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("open {}: {e}", f.path))?;
        fh.write_all(&data).map_err(|e| format!("append {}: {e}", f.path))?;
    } else {
        fs::write(path, &data).map_err(|e| format!("write {}: {e}", f.path))?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {}: {e}", f.path))?;
    if let Some(owner) = &f.owner {
        let (user, group) = owner.split_once(':').unwrap_or((owner.as_str(), ""));
        let (uid, gid, _) =
            passwd_entry(user).ok_or_else(|| format!("write_files owner '{user}' not found"))?;
        let gid = if group.is_empty() {
            gid
        } else {
            group_id(group).ok_or_else(|| format!("write_files group '{group}' not found"))?
        };
        std::os::unix::fs::chown(path, Some(uid), Some(gid))
            .map_err(|e| format!("chown {}: {e}", f.path))?;
    }
    println!("tinycloudinit: wrote {} ({} bytes)", f.path, data.len());
    Ok(())
}

// ---- runcmd / user scripts --------------------------------------------

fn run_cmd(i: usize, c: &Cmd, ctx: &Ctx) {
    let (prog, args): (String, Vec<String>) = match c {
        Cmd::Shell(s) => ("/bin/sh".into(), vec!["-c".into(), s.clone()]),
        Cmd::Argv(v) if v.is_empty() => return,
        Cmd::Argv(v) => (v[0].clone(), v[1..].to_vec()),
    };
    if ctx.dry_run {
        println!("DRY: runcmd[{i}]: {prog} {}", args.join(" "));
        return;
    }
    println!("tinycloudinit: runcmd[{i}]: {prog} {}", args.join(" "));
    match Command::new(&prog).args(&args).status() {
        Ok(st) if st.success() => {}
        Ok(st) => eprintln!("tinycloudinit: runcmd[{i}] exited with {st}"),
        Err(e) => eprintln!("tinycloudinit: runcmd[{i}] failed to start: {e}"),
    }
}

fn run_user_script(script: &str, ctx: &Ctx) -> Result<()> {
    let path = ctx.state_dir.join("user-script");
    if ctx.dry_run {
        println!("DRY: would run user-data script ({} bytes)", script.len());
        return Ok(());
    }
    fs::create_dir_all(&ctx.state_dir).map_err(|e| format!("mkdir state dir: {e}"))?;
    fs::write(&path, script).map_err(|e| format!("write {}: {e}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    println!("tinycloudinit: running user-data script");
    let st = Command::new(&path)
        .status()
        .map_err(|e| format!("run user script: {e}"))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("user-data script exited with {st}"))
    }
}

// ---- helpers -----------------------------------------------------------

fn run_tool(ctx: &Ctx, program: &str, args: &[String]) -> Result<()> {
    if ctx.dry_run {
        println!("DRY: {program} {}", args.join(" "));
        return Ok(());
    }
    let st = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("{program} {} exited with {st}", args.join(" ")))
    }
}

/// uid, gid, home from /etc/passwd.
fn passwd_entry(name: &str) -> Option<(u32, u32, String)> {
    let data = fs::read_to_string("/etc/passwd").ok()?;
    for line in data.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() >= 6 && f[0] == name {
            return Some((f[2].parse().ok()?, f[3].parse().ok()?, f[5].to_string()));
        }
    }
    None
}

fn group_id(name: &str) -> Option<u32> {
    let data = fs::read_to_string("/etc/group").ok()?;
    for line in data.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() >= 3 && f[0] == name {
            return f[2].parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tci-test-{tag}-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn write_file_plain_and_append() {
        let d = tmpdir("wf");
        let target = d.join("sub/hello.txt");
        let f = WriteFile {
            path: target.to_string_lossy().into_owned(),
            content: "one\n".into(),
            ..Default::default()
        };
        let ctx = Ctx {
            dry_run: false,
            state_dir: d.clone(),
        };
        write_file(&f, &ctx).unwrap();
        let f2 = WriteFile {
            append: true,
            content: "two\n".into(),
            ..f.clone()
        };
        write_file(&f2, &ctx).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "one\ntwo\n");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn dry_run_makes_no_changes() {
        let d = tmpdir("dry");
        let target = d.join("never.txt");
        let f = WriteFile {
            path: target.to_string_lossy().into_owned(),
            content: "x".into(),
            ..Default::default()
        };
        let ctx = Ctx {
            dry_run: true,
            state_dir: d.clone(),
        };
        write_file(&f, &ctx).unwrap();
        assert!(!target.exists());
        fs::remove_dir_all(&d).ok();
    }
}
