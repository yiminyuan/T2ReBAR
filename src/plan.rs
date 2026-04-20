use std::error::Error;
use std::fmt;

use crate::pci::{self, Bdf, Device};
use crate::rebar::{self, Rebar, RebarEntry};
use crate::topology;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Use the preferred size index for each recognised vendor:device.
    Planned,
    /// Set rebar targets equal to the current size — tests the full unbind/
    /// remove/rescan/rebind cycle without changing any BAR sizes. Used to
    /// validate the pipeline before committing to a real resize.
    Current,
    /// Explicit size index override (applied to every GPU's framebuffer BAR).
    Explicit(u8),
}

impl Mode {
    pub fn as_str(&self) -> String {
        match self {
            Mode::Planned => String::from("planned"),
            Mode::Current => String::from("current"),
            Mode::Explicit(i) => format!("explicit={i}"),
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        if s == "planned" {
            return Some(Mode::Planned);
        }
        if s == "current" {
            return Some(Mode::Current);
        }
        if let Some(rest) = s.strip_prefix("explicit=") {
            if let Ok(n) = rest.parse::<u8>() {
                return Some(Mode::Explicit(n));
            }
        }
        None
    }
}

#[derive(Clone, Debug)]
pub struct Action {
    pub bdf: Bdf,
    pub vendor: u16,
    pub device_id: u16,
    pub driver: Option<String>,
    pub rebar_cap_offset: usize,
    /// Index within the REBAR capability's entry list (not the BAR number).
    pub rebar_entry_index: u8,
    pub bar_index: u8,
    pub original_size_index: u8,
    pub target_size_index: u8,
    pub cut_point: Bdf,
    pub companions: Vec<Bdf>,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub mode: Mode,
    pub actions: Vec<Action>,
}

#[derive(Debug)]
pub struct PlanError(pub String);
impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl Error for PlanError {}

/// Preferred size index per recognised GPU. Kept here rather than in
/// diagnose so the plan builder can reuse it.
pub fn preferred_size_index(vendor: u16, device: u16) -> Option<u8> {
    match (vendor, device) {
        (0x10de, 0x2bb1) => Some(17), // RTX PRO 6000 Blackwell → 128 GiB
        (0x1002, 0x73e3) => Some(13), // Radeon PRO W6600 → 8 GiB
        _ => None,
    }
}

/// Pick the framebuffer rebar entry — the largest prefetchable 64-bit BAR
/// that the device advertises as resizable.
pub fn pick_framebuffer_entry<'a>(
    gpu: &Device,
    reb: &'a Rebar,
) -> Option<(usize, &'a RebarEntry)> {
    let mut best: Option<(usize, &RebarEntry, u64)> = None;
    for (i, e) in reb.entries.iter().enumerate() {
        let res = gpu.resources.get(e.bar_index as usize)?;
        if res.is_prefetchable() && res.is_mem64() {
            let size = res.size();
            if best.map(|(_, _, s)| size > s).unwrap_or(true) {
                best = Some((i, e, size));
            }
        }
    }
    if let Some((i, e, _)) = best {
        return Some((i, e));
    }
    reb.entries.first().map(|e| (0, e))
}

pub fn build(mode: Mode) -> Result<Plan, Box<dyn Error>> {
    let devs = pci::enumerate()?;
    let gpus: Vec<&Device> = devs.iter().filter(|d| d.is_gpu()).collect();
    if gpus.is_empty() {
        return Err(Box::new(PlanError(String::from("no GPUs detected"))));
    }

    let mut actions = Vec::new();
    for gpu in &gpus {
        let rebar = rebar::read_rebar(gpu.bdf)?.ok_or_else(|| {
            PlanError(format!(
                "{}: device does not advertise a Resizable BAR capability",
                gpu.bdf
            ))
        })?;
        let (entry_idx, fb) = pick_framebuffer_entry(gpu, &rebar)
            .ok_or_else(|| PlanError(format!("{}: no framebuffer rebar entry", gpu.bdf)))?;

        let target = match mode {
            Mode::Planned => preferred_size_index(gpu.vendor, gpu.device_id).ok_or_else(|| {
                PlanError(format!(
                    "{}: no preferred size for vendor:device {:04x}:{:04x} — add a mapping or use --target=current/--size=N",
                    gpu.bdf, gpu.vendor, gpu.device_id
                ))
            })?,
            Mode::Current => fb.current_size_index,
            Mode::Explicit(idx) => idx,
        };
        if !fb.supports(target) && mode != Mode::Current {
            return Err(Box::new(PlanError(format!(
                "{}: target size index {} not in supported set {:?}",
                gpu.bdf,
                target,
                fb.supported_indices()
            ))));
        }

        let cut = topology::cut_point(gpu).ok_or_else(|| {
            PlanError(format!("{}: no cut point (attached directly to host?)", gpu.bdf))
        })?;

        let companions = pci::companions(gpu.bdf)?;

        actions.push(Action {
            bdf: gpu.bdf,
            vendor: gpu.vendor,
            device_id: gpu.device_id,
            driver: gpu.driver.clone(),
            rebar_cap_offset: rebar.cap_offset,
            rebar_entry_index: entry_idx as u8,
            bar_index: fb.bar_index,
            original_size_index: fb.current_size_index,
            target_size_index: target,
            cut_point: cut,
            companions,
        });
    }

    Ok(Plan { mode, actions })
}

pub fn print(plan: &Plan) {
    println!("Mode: {}", plan.mode.as_str());
    println!("Actions: {}", plan.actions.len());
    for (i, a) in plan.actions.iter().enumerate() {
        let from_b = rebar::size_bytes(a.original_size_index);
        let to_b = rebar::size_bytes(a.target_size_index);
        let arrow = if a.original_size_index == a.target_size_index {
            "(unchanged)".to_string()
        } else {
            format!(
                "{} → {}",
                rebar::format_size(from_b),
                rebar::format_size(to_b)
            )
        };
        println!();
        println!("  [{i}] {} {:04x}:{:04x}", a.bdf, a.vendor, a.device_id);
        println!(
            "      driver       {}",
            a.driver.as_deref().unwrap_or("(none)")
        );
        println!(
            "      rebar        cap@0x{:x}, entry {}, BAR {}",
            a.rebar_cap_offset, a.rebar_entry_index, a.bar_index
        );
        println!(
            "      size index   {} → {}  {}",
            a.original_size_index, a.target_size_index, arrow
        );
        println!("      cut point    {}", a.cut_point);
        let comp: Vec<String> = a.companions.iter().map(Bdf::to_string).collect();
        println!(
            "      companions   [{}]",
            if comp.is_empty() { String::from("none") } else { comp.join(", ") }
        );
    }
}
