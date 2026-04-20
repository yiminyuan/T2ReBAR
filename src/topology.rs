use std::collections::BTreeMap;

use crate::pci::{Bdf, Device};

/// The "cut point" is the PCI bridge that, when removed from sysfs, cascades
/// the entire GPU subtree (PLX switches, AMD/NV internal switches, endpoints).
/// It's the top-most PCI BDF on the sysfs path — the immediate child of the
/// host bridge (the `pci0000:XX/` directory). This matches the shell script's
/// "top-level bridge" approach but derived at runtime from topology.
pub fn cut_point(dev: &Device) -> Option<Bdf> {
    dev.parent_chain.first().copied()
}

/// Group GPUs by their cut point so that a single bridge-remove/rescan
/// handles all GPUs that share a subtree (e.g., the two dies of a Duo MPX).
pub fn group_by_cut_point(gpus: &[&Device]) -> BTreeMap<Bdf, Vec<Bdf>> {
    let mut groups: BTreeMap<Bdf, Vec<Bdf>> = BTreeMap::new();
    for g in gpus {
        if let Some(cut) = cut_point(g) {
            groups.entry(cut).or_default().push(g.bdf);
        }
    }
    groups
}
