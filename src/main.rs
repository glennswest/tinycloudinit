mod apply;
mod config;
mod datasource;
mod ec2;
mod growpart;

use datasource::DsMode;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const STATE_DIR: &str = "/var/lib/tinycloudinit";
const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Ctx {
    pub dry_run: bool,
    pub state_dir: PathBuf,
}

fn main() -> ExitCode {
    let mut seed_dir: Option<String> = None;
    let mut state_dir = STATE_DIR.to_string();
    let mut dry_run = false;
    let mut force = false;
    let mut mode = DsMode::Auto;
    let mut grow: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seed" => match args.next() {
                Some(v) => seed_dir = Some(v),
                None => return usage_error("--seed requires a directory"),
            },
            "--datasource" => match args.next().as_deref() {
                Some("auto") => mode = DsMode::Auto,
                Some("nocloud") => mode = DsMode::NoCloud,
                Some("ec2") => mode = DsMode::Ec2,
                Some(other) => {
                    return usage_error(&format!(
                        "--datasource must be auto, nocloud or ec2 (got '{other}')"
                    ))
                }
                None => return usage_error("--datasource requires a value"),
            },
            "--state-dir" => match args.next() {
                Some(v) => state_dir = v,
                None => return usage_error("--state-dir requires a directory"),
            },
            "--grow" => match args.next() {
                Some(v) => grow = Some(v),
                None => return usage_error("--grow requires a mountpoint or partition device"),
            },
            "--dry-run" => dry_run = true,
            "--force" => force = true,
            "--version" | "-V" => {
                println!("tinycloudinit {VERSION}");
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            other => return usage_error(&format!("unknown argument: {other}")),
        }
    }
    if let Some(target) = grow {
        return match growpart::standalone(&target, dry_run) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("tinycloudinit: error: {e}");
                ExitCode::FAILURE
            }
        };
    }
    match run(seed_dir.as_deref(), &state_dir, mode, dry_run, force) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tinycloudinit: error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("tinycloudinit: {msg}");
    print_help();
    ExitCode::from(2)
}

fn print_help() {
    println!(
        "tinycloudinit {VERSION} — tiny cloud-init (NoCloud) for small images

USAGE:
    tinycloudinit [OPTIONS]

OPTIONS:
    --seed <DIR>        Use DIR as the seed (must contain meta-data/user-data)
    --datasource <DS>   auto (default), nocloud, or ec2
    --grow <TARGET>     Just grow the partition backing TARGET (a mountpoint
                        like / or a partition device) and resize its
                        filesystem, then exit
    --state-dir <DIR>   State directory (default {STATE_DIR})
    --dry-run           Show what would be done without changing anything
    --force             Run even if this instance-id was already initialized
    -V, --version       Print version
    -h, --help          Print this help"
    );
}

fn run(
    seed_dir: Option<&str>,
    state_dir: &str,
    mode: DsMode,
    dry_run: bool,
    force: bool,
) -> Result<(), String> {
    let seed = match datasource::find(seed_dir, state_dir, mode)? {
        Some(s) => s,
        None => {
            println!("tinycloudinit: no datasource found; nothing to do");
            return Ok(());
        }
    };
    println!("tinycloudinit: datasource: {}", seed.source);

    let meta: config::MetaData = if seed.meta_data.trim().is_empty() {
        Default::default()
    } else {
        match serde_yaml::from_str(&seed.meta_data) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("tinycloudinit: warning: meta-data not parseable ({e}); ignoring");
                Default::default()
            }
        }
    };
    let instance_id = meta
        .instance_id
        .clone()
        .unwrap_or_else(|| "nocloud".to_string());

    let marker = Path::new(state_dir).join("instance-id");
    if !force && !dry_run {
        if let Ok(prev) = fs::read_to_string(&marker) {
            if prev.trim() == instance_id {
                println!(
                    "tinycloudinit: instance '{instance_id}' already initialized; nothing to do"
                );
                return Ok(());
            }
        }
    }

    let ctx = Ctx {
        dry_run,
        state_dir: PathBuf::from(state_dir),
    };
    apply::apply(&seed, &meta, &ctx)?;

    if !dry_run {
        fs::create_dir_all(state_dir).map_err(|e| format!("mkdir {state_dir}: {e}"))?;
        fs::write(&marker, format!("{instance_id}\n"))
            .map_err(|e| format!("write {}: {e}", marker.display()))?;
    }
    println!("tinycloudinit {VERSION}: instance '{instance_id}' initialized");
    Ok(())
}
