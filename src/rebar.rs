use std::io;

use crate::config;
use crate::pci::Bdf;

pub const PCI_EXT_CAP_START: usize = 0x100;
pub const PCI_EXT_CAP_ID_REBAR: u16 = 0x0015;

/// Walk the PCIe extended capability linked list and return the offset of
/// the first entry matching `cap_id`, or `None` if not found.
pub fn find_ext_cap(bdf: Bdf, cap_id: u16) -> io::Result<Option<usize>> {
    let mut pos = PCI_EXT_CAP_START;
    let mut seen = 0usize;
    loop {
        let header = config::read_dword(bdf, pos)?;
        // Devices without extended caps return 0 or all-ones at 0x100.
        if header == 0 || header == 0xFFFF_FFFF {
            return Ok(None);
        }
        let id = (header & 0xFFFF) as u16;
        let next = ((header >> 20) & 0xFFF) as usize;
        if id == cap_id {
            return Ok(Some(pos));
        }
        if next == 0 || next < PCI_EXT_CAP_START {
            return Ok(None);
        }
        pos = next;
        seen += 1;
        if seen > 48 {
            return Ok(None); // paranoid cycle guard
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RebarEntry {
    /// Which BAR this entry controls (0..=5).
    pub bar_index: u8,
    /// Current BAR size as a size index: size_bytes = 1 << (20 + size_index).
    pub current_size_index: u8,
    /// Supported sizes bitmask: bit `i` set ⇒ size_index `i` is supported.
    /// Covers size indices 0..=27 (1 MiB … 128 TiB).
    pub supported_sizes_mask: u32,
}

#[derive(Clone, Debug)]
pub struct Rebar {
    pub cap_offset: usize,
    pub entries: Vec<RebarEntry>,
}

impl RebarEntry {
    pub fn supports(&self, size_index: u8) -> bool {
        size_index < 32 && (self.supported_sizes_mask >> size_index) & 1 != 0
    }

    pub fn supported_indices(&self) -> Vec<u8> {
        (0u8..28)
            .filter(|&i| self.supports(i))
            .collect()
    }

    pub fn largest_supported(&self) -> Option<u8> {
        (0u8..28).rev().find(|&i| self.supports(i))
    }
}

/// Write a new size index into the REBAR CTRL register of the given entry.
/// Verifies by reading back. Returns an error if the readback does not
/// match the requested index (e.g. device rejected the value).
///
/// `cap_offset` is the offset of the REBAR capability header as returned
/// by `find_ext_cap`. `entry_index` is the 0-based entry number within
/// the capability (not the BAR number).
pub fn write_size_index(
    bdf: Bdf,
    cap_offset: usize,
    entry_index: u8,
    new_size_index: u8,
) -> io::Result<()> {
    let ctrl_off = cap_offset + 8 + 8 * entry_index as usize;
    let cur = config::read_dword(bdf, ctrl_off)?;
    // Clear bits [13:8] (6-bit BAR Size field) and set the new index.
    let cleared = cur & !0x0000_3F00;
    let new = cleared | ((new_size_index as u32 & 0x3F) << 8);
    config::write_dword(bdf, ctrl_off, new)?;
    let readback = config::read_dword(bdf, ctrl_off)?;
    let read_idx = ((readback >> 8) & 0x3F) as u8;
    if read_idx != new_size_index {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "rebar write rejected: wrote size idx {} but device reports {} (ctrl=0x{:08x})",
                new_size_index, read_idx, readback
            ),
        ));
    }
    Ok(())
}

pub fn read_rebar(bdf: Bdf) -> io::Result<Option<Rebar>> {
    let Some(pos) = find_ext_cap(bdf, PCI_EXT_CAP_ID_REBAR)? else {
        return Ok(None);
    };
    // Entry 0's Control register carries the NBAR field.
    let ctrl0 = config::read_dword(bdf, pos + 8)?;
    let nbars = ((ctrl0 >> 5) & 0x7) as usize;
    if nbars == 0 {
        return Ok(Some(Rebar { cap_offset: pos, entries: vec![] }));
    }
    let mut entries = Vec::with_capacity(nbars);
    for i in 0..nbars {
        let entry_pos = pos + 8 * i;
        let cap = config::read_dword(bdf, entry_pos + 4)?;
        let ctrl = config::read_dword(bdf, entry_pos + 8)?;
        let bar_idx = (ctrl & 0x7) as u8;
        let cur_size_idx = ((ctrl >> 8) & 0x3F) as u8;
        // CAP bits [4:31] → size indices 0..=27.
        let supported = (cap >> 4) & 0x0FFF_FFFF;
        entries.push(RebarEntry {
            bar_index: bar_idx,
            current_size_index: cur_size_idx,
            supported_sizes_mask: supported,
        });
    }
    Ok(Some(Rebar { cap_offset: pos, entries }))
}

/// Convert a size index to bytes: 2^(20 + idx).
pub fn size_bytes(size_index: u8) -> u64 {
    1u64 << (20 + size_index as u32)
}

pub fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    const TIB: u64 = GIB * 1024;
    if bytes >= TIB && bytes % TIB == 0 {
        format!("{} TiB", bytes / TIB)
    } else if bytes >= GIB && bytes % GIB == 0 {
        format!("{} GiB", bytes / GIB)
    } else if bytes >= MIB && bytes % MIB == 0 {
        format!("{} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{} B", bytes)
    }
}

pub fn format_size_index(size_index: u8) -> String {
    format_size(size_bytes(size_index))
}
