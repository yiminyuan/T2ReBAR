use std::collections::HashMap;
use std::error::Error;
use std::io::IsTerminal;
use std::process::Command;

use crate::pci::{self, Bdf, Device};
use crate::preflight::{self, Check};
use crate::rebar::{self, Rebar, RebarEntry};
use crate::topology;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Target sizes the user asked for in CLAUDE.md, keyed by (vendor, device).
fn preferred_size_index(vendor: u16, device: u16) -> Option<(u8, &'static str)> {
    match (vendor, device) {
        (0x10de, 0x2bb1) => Some((17, "RTX PRO 6000 Blackwell → 128 GiB (covers 96 GiB VRAM)")),
        (0x1002, 0x73e3) => Some((13, "Radeon PRO W6600 → 8 GiB (matches VRAM)")),
        _ => None,
    }
}

pub fn run() -> Result<()> {
    let color = std::io::stdout().is_terminal();
    let st = Style::new(color);

    let checks = preflight::run_all();
    let all = pci::enumerate()?;
    let labels = load_labels();

    println!();
    println!("{}", st.bold("=== t2rebar diagnose ==="));
    println!();

    print_preflight(&st, &checks);

    let by_bdf: HashMap<Bdf, &Device> = all.iter().map(|d| (d.bdf, d)).collect();
    let gpus: Vec<&Device> = all.iter().filter(|d| d.is_gpu()).collect();

    println!();
    println!("{} GPU(s) detected.", gpus.len());

    for gpu in &gpus {
        print_gpu(&st, gpu, &by_bdf, &labels);
    }

    print_groups(&st, &gpus, &labels);
    print_phase2_readiness(&st, &checks, &gpus);

    Ok(())
}

fn print_preflight(st: &Style, checks: &[Check]) {
    println!("{}", st.bold("Host pre-flight:"));
    for c in checks {
        let marker = if c.ok { st.green("[ OK ]") } else { st.yellow("[WARN]") };
        println!("  {marker} {} — {}", c.name, c.detail);
    }
}

fn print_gpu(
    st: &Style,
    gpu: &Device,
    by_bdf: &HashMap<Bdf, &Device>,
    labels: &HashMap<Bdf, String>,
) {
    println!();
    println!(
        "{}",
        st.bold(&format!(
            "── GPU {} {}",
            gpu.bdf,
            labels.get(&gpu.bdf).map(String::as_str).unwrap_or("")
        ))
    );
    println!(
        "    vendor:device  {:04x}:{:04x}   class 0x{:06x}",
        gpu.vendor, gpu.device_id, gpu.class
    );
    println!(
        "    driver         {}",
        gpu.driver.as_deref().unwrap_or("(none bound)")
    );

    // Cut point
    let cut = topology::cut_point(gpu);
    if let Some(c) = cut {
        let label = labels.get(&c).cloned().unwrap_or_default();
        println!("    cut point      {}   {}", c, label);
    } else {
        println!("    cut point      (none — device attached directly to host bridge?)");
    }

    // Parent chain
    if !gpu.parent_chain.is_empty() {
        let chain: Vec<String> = gpu.parent_chain.iter().map(|b| b.to_string()).collect();
        println!("    parent chain   {}", chain.join(" → "));
    }

    // Current BARs
    println!("    current BARs:");
    let mut any = false;
    for (i, r) in gpu.resources.iter().enumerate().take(6) {
        if !r.is_assigned() {
            continue;
        }
        any = true;
        println!(
            "      BAR {i} {:<11}  {:>12}  @ 0x{:012x}",
            r.kind_label(),
            rebar::format_size(r.size()),
            r.start
        );
    }
    if !any {
        println!("      (none assigned)");
    }

    // Rebar capability (requires root)
    match rebar::read_rebar(gpu.bdf) {
        Ok(Some(reb)) => print_rebar(st, gpu, &reb),
        Ok(None) => println!("    rebar          not advertised by device"),
        Err(e) => println!(
            "    rebar          could not read extended config space: {} {}",
            e,
            st.dim("(run as root)")
        ),
    }

    // Parent bridge window (cut point) for context
    if let Some(c) = cut {
        if let Some(parent) = by_bdf.get(&c) {
            if let Some(win) = bridge_prefetch_window(parent) {
                println!(
                    "    parent pref window ({}):  {}",
                    c,
                    rebar::format_size(win)
                );
            }
        }
    }
}

/// Read the prefetchable memory window of a PCI bridge from sysfs. The kernel
/// lays out 17 lines per `resource` file; bridge windows live at
/// PCI_BRIDGE_RESOURCES (13) + {0: I/O, 1: mem, 2: pref mem}. See
/// include/linux/pci.h.
fn bridge_prefetch_window(bridge: &Device) -> Option<u64> {
    const PCI_BRIDGE_PREF_MEM_WINDOW: usize = 15;
    let contents = std::fs::read_to_string(bridge.bdf.sysfs_dir().join("resource")).ok()?;
    let line = contents.lines().nth(PCI_BRIDGE_PREF_MEM_WINDOW)?;
    let mut parts = line.split_whitespace();
    let start = u64::from_str_radix(parts.next()?.trim_start_matches("0x"), 16).ok()?;
    let end = u64::from_str_radix(parts.next()?.trim_start_matches("0x"), 16).ok()?;
    let flags = u64::from_str_radix(parts.next()?.trim_start_matches("0x"), 16).ok()?;
    if flags == 0 || end < start {
        return None;
    }
    Some(end - start + 1)
}

fn print_rebar(st: &Style, gpu: &Device, reb: &Rebar) {
    println!("    rebar          cap @ 0x{:x}, {} entries", reb.cap_offset, reb.entries.len());
    for e in &reb.entries {
        let cur_bytes = rebar::size_bytes(e.current_size_index);
        let largest = e
            .largest_supported()
            .map(rebar::format_size_index)
            .unwrap_or_else(|| "?".into());
        let supported: Vec<String> = e
            .supported_indices()
            .into_iter()
            .map(rebar::format_size_index)
            .collect();
        println!(
            "      BAR {}: current idx {} ({}), supports up to {} — [{}]",
            e.bar_index,
            e.current_size_index,
            rebar::format_size(cur_bytes),
            largest,
            supported.join(", ")
        );
    }

    // Target evaluation
    if let Some((target_idx, why)) = preferred_size_index(gpu.vendor, gpu.device_id) {
        // Find the entry whose current BAR is the largest prefetchable 64-bit
        // BAR — that's the framebuffer. Fall back to the first entry.
        let fb = pick_framebuffer_entry(gpu, reb);
        match fb {
            Some(e) if e.supports(target_idx) => {
                let ok = st.green("[ OK ]");
                println!(
                    "    target         {ok} BAR {}: size idx {} ({}) — {}",
                    e.bar_index,
                    target_idx,
                    rebar::format_size_index(target_idx),
                    why
                );
            }
            Some(e) => {
                let fail = st.red("[FAIL]");
                println!(
                    "    target         {fail} BAR {}: size idx {} ({}) not in supported set — {}",
                    e.bar_index,
                    target_idx,
                    rebar::format_size_index(target_idx),
                    why
                );
                if let Some(lg) = e.largest_supported() {
                    println!(
                        "                   ceiling for this BAR: {} (idx {lg})",
                        rebar::format_size_index(lg)
                    );
                }
            }
            None => {
                println!("    target         no suitable rebar entry found for framebuffer BAR");
            }
        }
    }
}

fn pick_framebuffer_entry<'a>(gpu: &Device, reb: &'a Rebar) -> Option<&'a RebarEntry> {
    // Framebuffer BAR = largest prefetchable 64-bit BAR whose index has a
    // matching rebar entry.
    let mut best: Option<(&RebarEntry, u64)> = None;
    for e in &reb.entries {
        let res = gpu.resources.get(e.bar_index as usize)?;
        if res.is_prefetchable() && res.is_mem64() {
            let size = res.size();
            if best.map(|(_, s)| size > s).unwrap_or(true) {
                best = Some((e, size));
            }
        }
    }
    best.map(|(e, _)| e).or_else(|| reb.entries.first())
}

fn print_groups(st: &Style, gpus: &[&Device], labels: &HashMap<Bdf, String>) {
    let groups = topology::group_by_cut_point(gpus);
    println!();
    println!("{}", st.bold("Cut-point groups (Phase 2 will remove/rescan each bridge once):"));
    for (cut, members) in &groups {
        let label = labels.get(cut).map(String::as_str).unwrap_or("");
        let m: Vec<String> = members.iter().map(Bdf::to_string).collect();
        println!("  cut {} {} → [{}]", cut, label, m.join(", "));
    }
    if groups.is_empty() {
        println!("  (no GPUs with an identifiable cut point)");
    }
}

fn print_phase2_readiness(st: &Style, checks: &[Check], gpus: &[&Device]) {
    println!();
    println!("{}", st.bold("Phase 2 readiness:"));

    let realloc = checks
        .iter()
        .find(|c| c.name.starts_with("pci=realloc"))
        .map(|c| c.ok)
        .unwrap_or(false);
    line(st, realloc, "pci=realloc present");

    let root = checks.iter().find(|c| c.name == "running as root").map(|c| c.ok).unwrap_or(false);
    if !root {
        line(st, false, "root privileges required to read full config space");
    }

    let mut all_targets_ok = true;
    for gpu in gpus {
        if let Some((target, _)) = preferred_size_index(gpu.vendor, gpu.device_id) {
            match rebar::read_rebar(gpu.bdf) {
                Ok(Some(reb)) => {
                    let fb = pick_framebuffer_entry(gpu, &reb);
                    let ok = fb.map(|e| e.supports(target)).unwrap_or(false);
                    let label = format!(
                        "{:04x}:{:04x} at {} supports target size idx {}",
                        gpu.vendor, gpu.device_id, gpu.bdf, target
                    );
                    line(st, ok, &label);
                    if !ok {
                        all_targets_ok = false;
                    }
                }
                _ => {
                    line(st, false, &format!("could not read rebar for {}", gpu.bdf));
                    all_targets_ok = false;
                }
            }
        }
    }

    println!();
    if realloc && root && all_targets_ok {
        println!("  {} Ready to proceed to Phase 2 (execute mode — not yet implemented).", st.green("✓"));
    } else {
        println!("  {} Not ready for Phase 2. Resolve the items above first.", st.yellow("!"));
    }
}

fn line(st: &Style, ok: bool, label: &str) {
    let marker = if ok { st.green("[ OK ]") } else { st.red("[FAIL]") };
    println!("  {marker} {label}");
}

/// Shell out to `lspci -mm -D -nn` once to build a BDF → human label map.
/// If lspci isn't available, returns an empty map; callers fall back to numeric.
fn load_labels() -> HashMap<Bdf, String> {
    let mut out = HashMap::new();
    let Ok(output) = Command::new("lspci").args(["-mm", "-D", "-nn"]).output() else {
        return out;
    };
    if !output.status.success() {
        return out;
    }
    let s = String::from_utf8_lossy(&output.stdout);
    for line in s.lines() {
        // Format: "DDDD:BB:DD.F \"class [xxxx]\" \"vendor [xxxx]\" \"device [xxxx]\" ..."
        let (bdf_s, rest) = match line.split_once(' ') {
            Some(p) => p,
            None => continue,
        };
        let Some(bdf) = Bdf::parse(bdf_s) else { continue };
        let fields = split_quoted(rest);
        // fields[0] = class, fields[1] = vendor, fields[2] = device
        let vendor = fields.get(1).map(strip_id).unwrap_or_default();
        let device = fields.get(2).map(strip_id).unwrap_or_default();
        let label = if !vendor.is_empty() || !device.is_empty() {
            format!("({} — {})", vendor, device)
        } else {
            String::new()
        };
        out.insert(bdf, label);
    }
    out
}

fn split_quoted(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_q = false;
    let mut cur = String::new();
    for ch in s.chars() {
        match ch {
            '"' => {
                if in_q {
                    out.push(std::mem::take(&mut cur));
                }
                in_q = !in_q;
            }
            _ if in_q => cur.push(ch),
            _ => {}
        }
    }
    out
}

fn strip_id(s: &String) -> String {
    // "NVIDIA Corporation [10de]" → "NVIDIA Corporation"
    match s.rfind(" [") {
        Some(i) => s[..i].trim().to_string(),
        None => s.trim().to_string(),
    }
}

struct Style {
    color: bool,
}
impl Style {
    fn new(color: bool) -> Self {
        Self { color }
    }
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
    fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    fn green(&self, s: &str) -> String {
        self.wrap("32", s)
    }
    fn yellow(&self, s: &str) -> String {
        self.wrap("33", s)
    }
    fn red(&self, s: &str) -> String {
        self.wrap("31", s)
    }
}
