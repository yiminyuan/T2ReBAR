# T2ReBAR

A small Rust tool that reconfigures PCIe Resizable BAR on a Mac Pro 2019 running
Linux, so GPU VRAM is fully CPU-visible.

## Background

Mac Pro 2019's firmware does not implement the Resizable BAR protocol. By
default, Linux leaves every GPU's framebuffer BAR at the legacy 256 MiB size,
regardless of how much VRAM the card actually has. The kernel knows how to
resize BARs — but only at PCI-enumeration time, which on this box means the
firmware's initial enumeration, which is the broken one.

The workaround is to do it ourselves after boot:

1. Unbind the GPU drivers (and companion audio/USB functions on the same slot).
2. Write a larger size index into each GPU's Resizable BAR CTRL register.
3. Remove the top-level PCI bridge above each GPU so the kernel discards its
   current bridge-window allocation.
4. Rescan the PCI bus with `pci=realloc`, which makes the kernel re-walk the
   tree, see the new BAR-size requests, and allocate bridge windows big enough
   to satisfy them.
5. Let udev rebind the drivers (or bind them explicitly).

## Status

Tested on:

- CachyOS Linux, kernel 7.0
- NVIDIA RTX PRO 6000 Blackwell (96 GB VRAM) with `nvidia-open-dkms` 595.58.03
  → 128 GiB BAR
- Radeon PRO W6600 (8 GB VRAM) with `amdgpu`
  → 8 GiB BAR

Support for other vendor:device pairs requires a one-line addition to
`preferred_size_index` in `src/plan.rs`. Auto-detection of VRAM is on the wish
list but not yet implemented.

## Requirements

- Linux with sysfs (`/sys/bus/pci`) and config-space access
  (`/sys/bus/pci/devices/*/config`)
- Kernel cmdline must include `pci=realloc` — without it the kernel will not
  reallocate bridge windows on rescan, and the new large BAR will fail to
  place. `t2rebar execute` refuses to run unless this flag is present.
- Root privileges
- A GPU that advertises the PCIe Resizable BAR extended capability (every
  modern discrete GPU does)

## Build

```
cargo build --release
```

Zero external crates; just the standard library.

## Usage

```
sudo t2rebar diagnose                       Read-only report on GPUs + rebar
sudo t2rebar plan     [--target=<t>]        Build and print an action plan
sudo t2rebar execute  [flags]               Run the plan (DESTRUCTIVE)
sudo t2rebar rollback [flags]               Restore original sizes from manifest
sudo t2rebar verify   [flags]               Compare live BAR sizes vs manifest
```

### Flags

| Flag | Meaning |
| --- | --- |
| `--target=planned` | (default) Use the hard-coded preferred size per GPU |
| `--target=current` | Dry-cycle: unbind/remove/rescan/rebind *without* resizing |
| `--target=<N>`     | Explicit size index (advanced; all GPUs same size) |
| `--yes`, `-y`      | Skip the interactive confirmation prompt |
| `--force`          | Bypass safety refusals (missing `pci=realloc`, active consumers) |
| `--manifest=<p>`   | Override manifest path (default `/var/lib/t2rebar/state.txt`) |

Size index `N` maps to `2^(20+N)` bytes. Common values:

| idx | size    |
| --- | ------- |
| 8   | 256 MiB |
| 13  | 8 GiB   |
| 15  | 32 GiB  |
| 16  | 64 GiB  |
| 17  | 128 GiB |

### Manual per-boot procedure

Applied manually after login:

```
sudo systemctl stop display-manager
sudo t2rebar execute --target=planned --yes
sudo systemctl start display-manager
```

Stopping the display manager is necessary because its X/Wayland session holds
open handles to `/dev/dri/*`, which blocks driver unbind.

If you use [T2FanRD](https://github.com/yiminyuan/T2FanRD) and have both an NVIDIA GPU and its driver installed, you also need to stop T2FanRD (see quirks below):

```
sudo systemctl stop t2fanrd
```

### First-time validation

Before trusting the tool with a real resize on a new host, do a **dry cycle**
first. This runs the full teardown/rebuild sequence with the target size equal
to the current size, so no BARs actually change:

```
sudo t2rebar execute --target=current --yes
```

If every GPU comes back bound to its original driver, the pipeline works on
your hardware.

## Boot integration (systemd)

Once the manual flow works reliably, install the bundled systemd unit so the
resize happens automatically on every boot, before display-manager and T2FanRD
come up.

```
cd T2ReBAR
cargo build --release
sudo install -m 0755 target/release/t2rebar /usr/local/bin/t2rebar
sudo install -m 0644 systemd/t2rebar.service /etc/systemd/system/t2rebar.service
sudo systemctl daemon-reload
sudo systemctl enable t2rebar.service
```

The unit is `Type=oneshot` with `RemainAfterExit=yes`, ordered:

- `After=sysinit.target` — udev coldplug has run, drivers are bound and safe to
  cycle
- `Before=display-manager.service t2fanrd.service graphical.target` — beat any
  consumer that would block unbind
- `ConditionKernelCommandLine=pci=realloc` — skip cleanly on boots without the
  flag, rather than failing
- `WantedBy=multi-user.target` — only runs in normal boots, not rescue/emergency

Inspect after reboot:

```
systemctl status t2rebar.service
journalctl -u t2rebar.service -b
```

Disable (to stop auto-resize on next boot):

```
sudo systemctl disable t2rebar.service
```

Interaction with `rollback`: if you run `t2rebar rollback` to return to 256 MiB
BARs, that lasts until the next boot — at which point the enabled service will
resize again. `disable` the service first if you want the rollback to stick.

## Recovery

`execute` writes a manifest to `/var/lib/t2rebar/state.txt` *before* making
any changes. The manifest captures every action's original and target size
indices, the driver name, the cut-point bridge BDF, and the companion function
BDFs.

- **Normal path**: `t2rebar rollback --yes` reads the manifest, swaps
  original and target, runs the same teardown/rebuild cycle, then deletes the
  manifest on success.
- **If something went wrong mid-cycle**: the in-memory `RecoveryGuard` fires
  from its `Drop` impl, issuing a best-effort rescan and driver rebind before
  the process exits. After the process is gone, you can still use `rollback`.
- **If the system is hung**: a hard reboot returns the firmware-chosen 256 MiB
  BARs; nothing persists across reboot. There is no NVRAM write.
- **If `rollback` fails**: the manifest is preserved on disk, so you can try
  again (possibly with `--force`).

## Design notes

- **No hardcoded topology.** Cut-point bridges and companion functions are
  discovered by walking sysfs. Moving a GPU to a different slot, adding or
  removing cards, or switching GPUs does not require code changes.
- **Dynamic REBAR capability discovery.** Different vendors put the Resizable
  BAR extended capability at different config-space offsets (NVIDIA at 0x134,
  AMD at 0x200 on our hardware). The tool walks the extended-capability linked
  list at offset 0x100 rather than assuming a fixed offset.
- **Framebuffer BAR auto-selection.** The BAR to resize is chosen as the
  largest prefetchable 64-bit BAR the device advertises as resizable — which
  is what every modern GPU uses for its framebuffer aperture.
- **Safety refusals** before any destructive action:
  - Not root
  - `pci=realloc` missing from kernel cmdline
  - Anything has `/dev/dri/*`, `/dev/kfd`, or `/dev/nvidia*` open
  - Display manager active (gdm/sddm/lightdm/ly)

  Each can be overridden with `--force` at your own risk.

## Known quirks (Mac Pro 7,1)

- **t2fanrd holds NVIDIA handles.** [T2FanRD](https://github.com/yiminyuan/T2FanRD) opens
  `/dev/nvidia0`, `/dev/nvidia-uvm`, and a CUDA context (presumably to read GPU
  thermals). Without stopping it, `nv_pci_remove_helper` spins forever in
  `os_delay` with `NVRM: Attempting to remove device ... with non-zero usage
  count!` in dmesg. `ensure_no_active_consumers` detects and refuses.
- **amdgpu auto-resizes on bind.** The AMD driver calls
  `pci_resize_resource()` during its bind path and will overwrite CTRL to its
  own preferred size, regardless of what was written before bind. For our
  planned target (8 GiB on W6600) this doesn't matter; for `--target=current`
  on AMD, `verify` will report `[FAIL]` because amdgpu resized it to its
  preferred size. This is not a bug in `t2rebar`.
- **NVIDIA bridge window is tight.** On our machine the NVIDIA GPU's upstream
  bridge prefetch window was only 288 MiB at boot. `pci=realloc` successfully
  expanded it to fit a 128 GiB BAR on rescan, but this is the most likely
  source of trouble on other configurations. If the resize succeeds at the
  REBAR register level but the BAR doesn't end up mapped, suspect this.

## Source layout

```
src/
  main.rs        CLI dispatch and flag parsing
  config.rs      PCI config-space read/write primitives
  pci.rs         BDF parsing, device enumeration, resource parsing
  rebar.rs       Resizable BAR extended capability walk + CTRL writes
  topology.rs    Cut-point selection, companion grouping
  preflight.rs   Root/cmdline/sysfs checks
  diagnose.rs    Human-readable report
  plan.rs        Mode → Action list (includes preferred_size_index table)
  manifest.rs    Flat-text manifest write/read for rollback
  execute.rs     execute / rollback / verify — the destructive path
```

## Disclaimer

This tool reconfigures live PCIe hardware. A mistake can wedge the display,
fail to rebind drivers, or in rare cases require a hard reboot. Tested only on
the author's machine. Read the code before running it on yours.

## License

GPL-3.0-only. See `LICENSE`.
