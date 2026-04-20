use std::fs;

#[derive(Clone, Debug)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

pub fn run_all() -> Vec<Check> {
    vec![
        check_root(),
        check_pci_realloc(),
        check_sysfs(),
    ]
}

fn check_root() -> Check {
    let uid = unsafe { libc_getuid() };
    Check {
        name: "running as root",
        ok: uid == 0,
        detail: if uid == 0 {
            String::from("uid=0")
        } else {
            format!("uid={uid} — config-space reads past 0x40 will return zeros/errors")
        },
    }
}

fn check_pci_realloc() -> Check {
    match fs::read_to_string("/proc/cmdline") {
        Ok(s) => {
            let ok = s.split_whitespace().any(|t| t == "pci=realloc" || t.starts_with("pci=realloc,"));
            Check {
                name: "pci=realloc in kernel cmdline",
                ok,
                detail: if ok {
                    String::from("present")
                } else {
                    String::from("MISSING — required for Phase 2 (bridge window reallocation after resize)")
                },
            }
        }
        Err(e) => Check {
            name: "pci=realloc in kernel cmdline",
            ok: false,
            detail: format!("could not read /proc/cmdline: {e}"),
        },
    }
}

fn check_sysfs() -> Check {
    let path = "/sys/bus/pci/devices";
    let ok = fs::metadata(path).is_ok();
    Check {
        name: "/sys/bus/pci/devices accessible",
        ok,
        detail: if ok { String::from("ok") } else { String::from("not found") },
    }
}

// Minimal libc shim — we only need geteuid/getuid, so avoid the dep.
extern "C" {
    fn getuid() -> u32;
}
unsafe fn libc_getuid() -> u32 {
    unsafe { getuid() }
}
