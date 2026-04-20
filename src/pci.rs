use std::fs;
use std::io;
use std::path::PathBuf;

pub const SYSFS_PCI_DEVICES: &str = "/sys/bus/pci/devices";

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Ord, PartialOrd)]
pub struct Bdf {
    pub domain: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl Bdf {
    pub fn parse(s: &str) -> Option<Self> {
        // Format: DDDD:BB:DD.F
        let (dom, rest) = s.split_once(':')?;
        let (bus, rest) = rest.split_once(':')?;
        let (dev, func) = rest.split_once('.')?;
        Some(Bdf {
            domain: u16::from_str_radix(dom, 16).ok()?,
            bus: u8::from_str_radix(bus, 16).ok()?,
            device: u8::from_str_radix(dev, 16).ok()?,
            function: u8::from_str_radix(func, 16).ok()?,
        })
    }

    pub fn sysfs_dir(&self) -> PathBuf {
        PathBuf::from(format!("{SYSFS_PCI_DEVICES}/{self}"))
    }
}

impl std::fmt::Display for Bdf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:04x}:{:02x}:{:02x}.{:x}",
            self.domain, self.bus, self.device, self.function
        )
    }
}

/// Kernel IORESOURCE_* flag bits. See include/linux/ioport.h.
pub mod res_flag {
    pub const IO: u64 = 0x0000_0100;
    pub const MEM: u64 = 0x0000_0200;
    pub const PREFETCH: u64 = 0x0000_2000;
    pub const MEM_64: u64 = 0x0010_0000;
}

#[derive(Clone, Copy, Debug)]
pub struct Resource {
    pub start: u64,
    pub end: u64,
    pub flags: u64,
}

impl Resource {
    pub fn is_assigned(&self) -> bool {
        self.flags != 0 && self.end >= self.start && !(self.start == 0 && self.end == 0)
    }
    pub fn size(&self) -> u64 {
        if !self.is_assigned() {
            return 0;
        }
        self.end - self.start + 1
    }
    pub fn is_mem(&self) -> bool {
        self.flags & res_flag::MEM != 0
    }
    pub fn is_io(&self) -> bool {
        self.flags & res_flag::IO != 0
    }
    pub fn is_prefetchable(&self) -> bool {
        self.flags & res_flag::PREFETCH != 0
    }
    pub fn is_mem64(&self) -> bool {
        self.flags & res_flag::MEM_64 != 0
    }
    pub fn kind_label(&self) -> &'static str {
        if self.is_io() {
            "io"
        } else if self.is_mem() {
            if self.is_prefetchable() && self.is_mem64() {
                "mem64 pref"
            } else if self.is_prefetchable() {
                "mem32 pref"
            } else if self.is_mem64() {
                "mem64"
            } else {
                "mem32"
            }
        } else {
            "?"
        }
    }
}

#[derive(Clone, Debug)]
pub struct Device {
    pub bdf: Bdf,
    pub class: u32,
    pub vendor: u16,
    pub device_id: u16,
    pub driver: Option<String>,
    pub resources: Vec<Resource>,
    pub parent_chain: Vec<Bdf>, // root-most first, self not included
}

impl Device {
    pub fn is_gpu(&self) -> bool {
        let c = self.class >> 8;
        c == 0x0300 || c == 0x0302 // VGA or 3D controller
    }
}

fn read_hex(path: &std::path::Path) -> io::Result<u32> {
    let s = fs::read_to_string(path)?;
    let s = s.trim().trim_start_matches("0x");
    u32::from_str_radix(s, 16)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("parse {path:?}: {e}")))
}

fn read_resources(bdf: Bdf) -> io::Result<Vec<Resource>> {
    let contents = fs::read_to_string(bdf.sysfs_dir().join("resource"))?;
    let mut out = Vec::with_capacity(7);
    for (idx, line) in contents.lines().take(7).enumerate() {
        let mut parts = line.split_whitespace();
        let s = parts.next().ok_or_else(|| bad(format!("resource[{idx}] missing start")))?;
        let e = parts.next().ok_or_else(|| bad(format!("resource[{idx}] missing end")))?;
        let f = parts.next().ok_or_else(|| bad(format!("resource[{idx}] missing flags")))?;
        out.push(Resource {
            start: hex64(s)?,
            end: hex64(e)?,
            flags: hex64(f)?,
        });
    }
    Ok(out)
}

fn hex64(s: &str) -> io::Result<u64> {
    let s = s.trim_start_matches("0x");
    u64::from_str_radix(s, 16).map_err(|e| bad(format!("hex parse {s:?}: {e}")))
}

fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn read_driver(bdf: Bdf) -> Option<String> {
    let link = fs::read_link(bdf.sysfs_dir().join("driver")).ok()?;
    link.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
}

/// Resolve the sysfs path of a device and walk its ancestor PCI BDFs.
/// Returns parents ordered root → leaf (closest-to-root first), excluding `bdf` itself.
fn read_parent_chain(bdf: Bdf) -> io::Result<Vec<Bdf>> {
    let resolved = fs::canonicalize(bdf.sysfs_dir())?;
    let mut chain = Vec::new();
    for component in resolved.components() {
        if let Some(s) = component.as_os_str().to_str() {
            if let Some(b) = Bdf::parse(s) {
                if b != bdf {
                    chain.push(b);
                }
            }
        }
    }
    Ok(chain)
}

pub fn read_device(bdf: Bdf) -> io::Result<Device> {
    let dir = bdf.sysfs_dir();
    Ok(Device {
        bdf,
        class: read_hex(&dir.join("class"))?,
        vendor: read_hex(&dir.join("vendor"))? as u16,
        device_id: read_hex(&dir.join("device"))? as u16,
        driver: read_driver(bdf),
        resources: read_resources(bdf)?,
        parent_chain: read_parent_chain(bdf)?,
    })
}

/// Return all BDFs that share the same domain:bus:device as `bdf`, excluding
/// `bdf` itself. These are the companion functions (audio, USB, etc.) that
/// must be unbound alongside the primary function before removing the parent
/// bridge.
pub fn companions(bdf: Bdf) -> io::Result<Vec<Bdf>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(SYSFS_PCI_DEVICES)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(b) = Bdf::parse(&name) else { continue };
        if b != bdf
            && b.domain == bdf.domain
            && b.bus == bdf.bus
            && b.device == bdf.device
        {
            out.push(b);
        }
    }
    out.sort();
    Ok(out)
}

/// Get the currently bound driver for a BDF, or None if none is bound.
pub fn current_driver(bdf: Bdf) -> Option<String> {
    read_driver(bdf)
}

pub fn enumerate() -> io::Result<Vec<Device>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(SYSFS_PCI_DEVICES)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(bdf) = Bdf::parse(&name) else { continue };
        match read_device(bdf) {
            Ok(dev) => out.push(dev),
            Err(e) => eprintln!("warn: read {bdf}: {e}"),
        }
    }
    out.sort_by_key(|d| d.bdf);
    Ok(out)
}
