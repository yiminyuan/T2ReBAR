use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::io;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::manifest;
use crate::pci::{self, Bdf};
use crate::plan::{self, Action, Mode, Plan};
use crate::rebar;

type BoxErr = Box<dyn Error>;

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Skip interactive confirmation prompt.
    pub yes: bool,
    /// Ignore safety refusals (active display manager, open DRM handles,
    /// missing pci=realloc). Use with extreme caution.
    pub force: bool,
    /// If Some, use this manifest path instead of the default.
    pub manifest_path: Option<std::path::PathBuf>,
}

pub fn execute(plan: &Plan, opts: Options) -> Result<(), BoxErr> {
    ensure_root()?;
    ensure_pci_realloc(opts.force)?;
    ensure_no_active_consumers(opts.force)?;

    plan::print(plan);
    println!();
    if !opts.yes && !confirm_interactively()? {
        return Err("cancelled by user".into());
    }

    let manifest_path = opts
        .manifest_path
        .clone()
        .unwrap_or_else(manifest::default_path);
    manifest::write(plan, &manifest_path)?;
    eprintln!("[ok] wrote manifest: {}", manifest_path.display());

    run_plan(plan)?;

    if plan.mode == Mode::Current {
        // Nothing to roll back from a no-op cycle.
        let _ = manifest::delete(&manifest_path);
        eprintln!("[ok] dry-cycle complete; manifest removed");
    } else {
        eprintln!("[ok] execute complete; manifest retained for rollback");
    }
    Ok(())
}

pub fn rollback(opts: Options) -> Result<(), BoxErr> {
    ensure_root()?;
    let manifest_path = opts
        .manifest_path
        .clone()
        .unwrap_or_else(manifest::default_path);
    let orig = manifest::read(&manifest_path)?;

    let mut rb = orig.clone();
    for a in &mut rb.actions {
        std::mem::swap(&mut a.original_size_index, &mut a.target_size_index);
    }
    // Mark the rolled-back plan with a distinct mode for the printout.
    rb.mode = Mode::Explicit(0);

    ensure_pci_realloc(opts.force)?;
    ensure_no_active_consumers(opts.force)?;

    println!("Rollback plan (restoring original sizes):");
    plan::print(&rb);
    println!();
    if !opts.yes && !confirm_interactively()? {
        return Err("cancelled by user".into());
    }

    // Don't overwrite the manifest with the swapped plan. If the rollback
    // succeeds, we delete the manifest; if it fails, the original manifest
    // is still on disk so the user can try again.
    run_plan(&rb)?;
    manifest::delete(&manifest_path)?;
    eprintln!("[ok] rollback complete; manifest removed");
    Ok(())
}

pub fn verify_cmd(opts: Options) -> Result<(), BoxErr> {
    let manifest_path = opts
        .manifest_path
        .clone()
        .unwrap_or_else(manifest::default_path);
    let plan = manifest::read(&manifest_path)?;
    plan::print(&plan);
    println!();
    let report = verify_bars(&plan)?;
    for line in &report {
        println!("{line}");
    }
    Ok(())
}

/// The core "unbind → rebar → remove → rescan → rebind → verify" sequence.
/// Called by `execute` and `rollback`; neither writes the manifest here.
fn run_plan(plan: &Plan) -> Result<(), BoxErr> {
    let cut_points: Vec<Bdf> = plan
        .actions
        .iter()
        .map(|a| a.cut_point)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let unbind_targets: Vec<(Bdf, Option<String>)> = collect_unbind_targets(plan);

    let mut guard = RecoveryGuard::new(&plan.actions, &unbind_targets);

    eprintln!("[step 1] unbinding drivers");
    for (bdf, drv) in &unbind_targets {
        if let Some(d) = drv {
            match unbind_driver(*bdf, d) {
                Ok(()) => eprintln!("  unbound {} from {}", d, bdf),
                Err(e) => eprintln!("  warn: unbind {} from {}: {}", d, bdf, e),
            }
        }
    }

    eprintln!("[step 2] writing rebar CTRL registers");
    for a in &plan.actions {
        if a.target_size_index == a.original_size_index {
            eprintln!(
                "  {} BAR {} already at size idx {} — skipping write",
                a.bdf, a.bar_index, a.original_size_index
            );
            continue;
        }
        rebar::write_size_index(
            a.bdf,
            a.rebar_cap_offset,
            a.rebar_entry_index,
            a.target_size_index,
        )?;
        eprintln!(
            "  {} BAR {}: size idx {} → {}",
            a.bdf, a.bar_index, a.original_size_index, a.target_size_index
        );
    }

    eprintln!("[step 3] removing cut-point bridges");
    for cut in &cut_points {
        remove_device(*cut)?;
        eprintln!("  removed {}", cut);
    }

    sleep(Duration::from_secs(2));

    eprintln!("[step 4] rescanning PCI bus");
    pci_rescan()?;

    eprintln!("[step 5] waiting for devices to reappear");
    for a in &plan.actions {
        wait_for_device(a.bdf, Duration::from_secs(15))?;
        eprintln!("  {} is back", a.bdf);
    }

    eprintln!("[step 6] waiting for drivers to bind (udev)");
    for (bdf, drv) in &unbind_targets {
        if let Some(d) = drv {
            match ensure_bound(*bdf, d, Duration::from_secs(15)) {
                Ok(()) => eprintln!("  {} bound to {}", bdf, d),
                Err(e) => eprintln!("  warn: {} could not be bound to {}: {}", bdf, d, e),
            }
        }
    }

    eprintln!("[step 7] verifying BAR sizes");
    let report = verify_bars(plan)?;
    for line in &report {
        eprintln!("  {line}");
    }

    guard.disarm();
    Ok(())
}

fn collect_unbind_targets(plan: &Plan) -> Vec<(Bdf, Option<String>)> {
    let mut out = Vec::new();
    for a in &plan.actions {
        out.push((a.bdf, pci::current_driver(a.bdf).or_else(|| a.driver.clone())));
        for c in &a.companions {
            out.push((*c, pci::current_driver(*c)));
        }
    }
    out
}

fn ensure_root() -> Result<(), BoxErr> {
    let uid = unsafe { getuid() };
    if uid != 0 {
        return Err(format!("must run as root (uid={uid})").into());
    }
    Ok(())
}

fn ensure_pci_realloc(force: bool) -> Result<(), BoxErr> {
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let present = cmdline
        .split_whitespace()
        .any(|t| t == "pci=realloc" || t.starts_with("pci=realloc,"));
    if !present && !force {
        return Err(
            "pci=realloc is not in the kernel cmdline; re-run with --force to override".into(),
        );
    }
    Ok(())
}

/// Refuse (unless --force) if there are signs that a user is actively using
/// the GPUs: open /dev/dri/* or /dev/kfd fds, or an active display manager.
fn ensure_no_active_consumers(force: bool) -> Result<(), BoxErr> {
    let mut warnings: Vec<String> = Vec::new();

    if let Ok(entries) = fs::read_dir("/proc") {
        for e in entries.flatten() {
            let pid = match e.file_name().to_string_lossy().parse::<u32>() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let fd_dir = e.path().join("fd");
            let Ok(fds) = fs::read_dir(&fd_dir) else { continue };
            for fd in fds.flatten() {
                if let Ok(target) = fs::read_link(fd.path()) {
                    let t = target.to_string_lossy().to_string();
                    if t.starts_with("/dev/dri/")
                        || t == "/dev/kfd"
                        || t.starts_with("/dev/nvidia")
                    {
                        let comm = fs::read_to_string(format!("/proc/{pid}/comm"))
                            .unwrap_or_else(|_| String::from("?"));
                        warnings.push(format!(
                            "pid {pid} ({}) has {} open",
                            comm.trim(),
                            t
                        ));
                    }
                }
            }
        }
    }

    for unit in &["display-manager", "gdm", "sddm", "lightdm", "ly"] {
        if is_systemd_active(unit) {
            warnings.push(format!("display manager {unit} is active"));
        }
    }

    if !warnings.is_empty() {
        eprintln!("active GPU consumers detected:");
        for w in &warnings {
            eprintln!("  - {w}");
        }
        if !force {
            return Err(
                "refusing to proceed while GPUs are in use; stop the consumer(s) or re-run with --force"
                    .into(),
            );
        } else {
            eprintln!("(--force set, continuing anyway)");
        }
    }
    Ok(())
}

fn is_systemd_active(unit: &str) -> bool {
    std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn confirm_interactively() -> io::Result<bool> {
    use std::io::{BufRead, Write};
    eprint!("Proceed? Type YES to continue: ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim() == "YES")
}

fn unbind_driver(bdf: Bdf, driver: &str) -> io::Result<()> {
    let path = format!("/sys/bus/pci/drivers/{driver}/unbind");
    if !Path::new(&path).exists() {
        return Ok(());
    }
    match fs::write(&path, format!("{bdf}")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn remove_device(bdf: Bdf) -> io::Result<()> {
    fs::write(bdf.sysfs_dir().join("remove"), "1")
}

fn pci_rescan() -> io::Result<()> {
    fs::write("/sys/bus/pci/rescan", "1")
}

fn wait_for_device(bdf: Bdf, timeout: Duration) -> io::Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if bdf.sysfs_dir().exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(200));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("device {} did not reappear within {:?}", bdf, timeout),
    ))
}

fn ensure_bound(bdf: Bdf, driver: &str, timeout: Duration) -> io::Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if pci::current_driver(bdf).as_deref() == Some(driver) {
            return Ok(());
        }
        sleep(Duration::from_millis(200));
    }
    let bind = format!("/sys/bus/pci/drivers/{driver}/bind");
    if Path::new(&bind).exists() {
        let _ = fs::write(&bind, format!("{bdf}"));
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if pci::current_driver(bdf).as_deref() == Some(driver) {
            return Ok(());
        }
        sleep(Duration::from_millis(200));
    }
    Err(io::Error::new(
        io::ErrorKind::Other,
        format!("{bdf} did not bind to {driver}"),
    ))
}

fn verify_bars(plan: &Plan) -> Result<Vec<String>, BoxErr> {
    let mut lines = Vec::new();
    for a in &plan.actions {
        let dev = pci::read_device(a.bdf)?;
        let actual_size = dev
            .resources
            .get(a.bar_index as usize)
            .map(|r| r.size())
            .unwrap_or(0);
        let expected = rebar::size_bytes(a.target_size_index);
        let marker = if actual_size == expected { "[ OK ]" } else { "[FAIL]" };
        lines.push(format!(
            "{} {} BAR {}: actual {}  expected {}",
            marker,
            a.bdf,
            a.bar_index,
            rebar::format_size(actual_size),
            rebar::format_size(expected)
        ));
    }
    Ok(lines)
}

/// On drop, if armed, attempt best-effort rescan and driver rebind. This is
/// the last line of defence if `run_plan` returns an error or panics between
/// unbind and the rescan/rebind steps.
struct RecoveryGuard {
    armed: bool,
    bdfs: Vec<(Bdf, Option<String>)>,
}

impl RecoveryGuard {
    fn new(_actions: &[Action], unbind_targets: &[(Bdf, Option<String>)]) -> Self {
        Self {
            armed: true,
            bdfs: unbind_targets.to_vec(),
        }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RecoveryGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        eprintln!("[RECOVERY] attempting best-effort rescan and rebind after failure");
        let _ = pci_rescan();
        sleep(Duration::from_secs(3));
        for (bdf, drv) in &self.bdfs {
            if let Some(d) = drv {
                let _ = ensure_bound(*bdf, d, Duration::from_secs(5));
            }
        }
        eprintln!("[RECOVERY] done; inspect `lspci` and `dmesg` for state");
    }
}

extern "C" {
    fn getuid() -> u32;
}
