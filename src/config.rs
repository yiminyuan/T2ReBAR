use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::pci::Bdf;

/// Read a 32-bit little-endian dword from PCI config space at `offset`.
///
/// Offsets >= 0x40 require CAP_SYS_ADMIN (root). Extended config space
/// (>= 0x100) also requires root on most kernels.
pub fn read_dword(bdf: Bdf, offset: usize) -> io::Result<u32> {
    let path = bdf.sysfs_dir().join("config");
    let mut f = File::open(&path)?;
    f.seek(SeekFrom::Start(offset as u64))?;
    let mut buf = [0u8; 4];
    f.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

/// Write a 32-bit little-endian dword to PCI config space at `offset`.
/// Requires root. Note that the kernel enforces PCI config-space write
/// restrictions (read-only bits are silently ignored).
pub fn write_dword(bdf: Bdf, offset: usize, value: u32) -> io::Result<()> {
    let path = bdf.sysfs_dir().join("config");
    let mut f = OpenOptions::new().read(true).write(true).open(&path)?;
    f.seek(SeekFrom::Start(offset as u64))?;
    f.write_all(&value.to_le_bytes())?;
    Ok(())
}
