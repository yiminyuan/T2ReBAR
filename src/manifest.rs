use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::pci::Bdf;
use crate::plan::{Action, Mode, Plan};

pub const MANIFEST_PATH: &str = "/var/lib/t2rebar/state.txt";

pub fn default_path() -> PathBuf {
    PathBuf::from(MANIFEST_PATH)
}

/// Write a plan to disk as a flat key=value manifest. Creates the parent
/// directory if needed. Overwrites any existing manifest.
pub fn write(plan: &Plan, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    out.push_str("# t2rebar manifest\n");
    out.push_str("schema=1\n");
    out.push_str(&format!("timestamp_epoch={}\n", epoch_seconds()));
    out.push_str(&format!("kernel={}\n", read_kernel_release()));
    out.push_str(&format!("hostname={}\n", read_hostname()));
    out.push_str(&format!("mode={}\n", plan.mode.as_str()));
    out.push('\n');
    for (i, a) in plan.actions.iter().enumerate() {
        out.push_str(&format!("action.{i}.bdf={}\n", a.bdf));
        out.push_str(&format!("action.{i}.vendor=0x{:04x}\n", a.vendor));
        out.push_str(&format!("action.{i}.device=0x{:04x}\n", a.device_id));
        out.push_str(&format!(
            "action.{i}.driver={}\n",
            a.driver.as_deref().unwrap_or("")
        ));
        out.push_str(&format!(
            "action.{i}.rebar_cap_offset=0x{:x}\n",
            a.rebar_cap_offset
        ));
        out.push_str(&format!(
            "action.{i}.rebar_entry_index={}\n",
            a.rebar_entry_index
        ));
        out.push_str(&format!("action.{i}.bar_index={}\n", a.bar_index));
        out.push_str(&format!(
            "action.{i}.original_size_index={}\n",
            a.original_size_index
        ));
        out.push_str(&format!(
            "action.{i}.target_size_index={}\n",
            a.target_size_index
        ));
        out.push_str(&format!("action.{i}.cut_point={}\n", a.cut_point));
        let companions: Vec<String> = a.companions.iter().map(Bdf::to_string).collect();
        out.push_str(&format!(
            "action.{i}.companions={}\n",
            companions.join(",")
        ));
        out.push('\n');
    }
    // Write atomically: write to temp, rename into place.
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, out)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn read(path: &Path) -> io::Result<Plan> {
    let s = fs::read_to_string(path)?;
    let mut kv: HashMap<String, String> = HashMap::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            kv.insert(k.trim().to_string(), v.trim().to_string());
        }
    }

    let mode_str = kv
        .get("mode")
        .ok_or_else(|| bad("missing mode".into()))?
        .clone();
    let mode = Mode::parse(&mode_str).ok_or_else(|| bad(format!("bad mode: {mode_str}")))?;

    // Count actions.
    let mut max_idx: Option<usize> = None;
    for k in kv.keys() {
        if let Some(rest) = k.strip_prefix("action.") {
            if let Some(idx_str) = rest.split('.').next() {
                if let Ok(n) = idx_str.parse::<usize>() {
                    max_idx = Some(max_idx.map_or(n, |m| m.max(n)));
                }
            }
        }
    }
    let count = max_idx.map(|m| m + 1).unwrap_or(0);
    let mut actions = Vec::with_capacity(count);
    for i in 0..count {
        let k = |suf: &str| format!("action.{i}.{suf}");
        let get = |suf: &str| -> io::Result<&str> {
            kv.get(&k(suf))
                .map(String::as_str)
                .ok_or_else(|| bad(format!("missing action.{i}.{suf}")))
        };
        let bdf =
            Bdf::parse(get("bdf")?).ok_or_else(|| bad(format!("bad action.{i}.bdf")))?;
        let cut = Bdf::parse(get("cut_point")?)
            .ok_or_else(|| bad(format!("bad action.{i}.cut_point")))?;
        let driver = {
            let d = get("driver")?;
            if d.is_empty() {
                None
            } else {
                Some(d.to_string())
            }
        };
        let companions: Vec<Bdf> = get("companions")?
            .split(',')
            .filter(|s| !s.is_empty())
            .filter_map(Bdf::parse)
            .collect();
        actions.push(Action {
            bdf,
            vendor: parse_hex16(get("vendor")?)?,
            device_id: parse_hex16(get("device")?)?,
            driver,
            rebar_cap_offset: parse_hex_usize(get("rebar_cap_offset")?)?,
            rebar_entry_index: parse_dec::<u8>(get("rebar_entry_index")?)?,
            bar_index: parse_dec::<u8>(get("bar_index")?)?,
            original_size_index: parse_dec::<u8>(get("original_size_index")?)?,
            target_size_index: parse_dec::<u8>(get("target_size_index")?)?,
            cut_point: cut,
            companions,
        });
    }
    Ok(Plan { mode, actions })
}

pub fn delete(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_kernel_release() -> String {
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| String::from("unknown"))
}

fn read_hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| String::from("unknown"))
}

fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn parse_hex16(s: &str) -> io::Result<u16> {
    let s = s.trim().trim_start_matches("0x");
    u16::from_str_radix(s, 16).map_err(|e| bad(format!("parse hex16 {s:?}: {e}")))
}
fn parse_hex_usize(s: &str) -> io::Result<usize> {
    let s = s.trim().trim_start_matches("0x");
    usize::from_str_radix(s, 16).map_err(|e| bad(format!("parse hex {s:?}: {e}")))
}
fn parse_dec<T: std::str::FromStr>(s: &str) -> io::Result<T>
where
    T::Err: std::fmt::Display,
{
    s.trim()
        .parse::<T>()
        .map_err(|e| bad(format!("parse dec {s:?}: {e}")))
}
