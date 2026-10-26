//! Reads the USB topology from Linux sysfs.
//!
//! Everything here is plain file reads under `/sys`, so no root access is
//! needed. The root path is a parameter so tests can point it at a fake tree.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::ids::IdDb;
use crate::sdr;

/// A PCI USB host controller and the buses it drives.
#[derive(Debug, Clone)]
pub struct Controller {
    /// PCI address, e.g. `0000:00:14.0`.
    pub pci_addr: String,
    pub name: String,
    pub kind: ControllerKind,
    pub driver: String,
    /// PCIe link the controller sits on, e.g. "8.0 GT/s PCIe x1". Empty when unknown.
    pub pcie_link: String,
    /// Canonical sysfs path of the PCI device.
    pub sysfs_path: PathBuf,
    pub vendor_id: u16,
    /// Who built the board the chip is on (PCI subsystem vendor).
    pub subsystem_vendor: u16,
    pub buses: Vec<Bus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerKind {
    Motherboard,
    AddOnCard { slot: String },
    /// One of several controllers behind the same PCIe switch, i.e. a
    /// multi-controller card. `card` is the switch's PCI address.
    SharedCard { card: String, count: usize },
    /// Behind a PCIe bridge with no slot listed: either an add-on card or an
    /// extra controller soldered to the board (common with ASMedia chips).
    Unknown,
}

impl ControllerKind {
    pub fn label(&self) -> String {
        match self {
            ControllerKind::Motherboard => "Motherboard".into(),
            ControllerKind::AddOnCard { slot } => format!("Add-on card (slot {slot})"),
            ControllerKind::SharedCard { card, count } => format!("Add-on card {card} ({count} controllers)"),
            ControllerKind::Unknown => "Add-on card or extra onboard chip".into(),
        }
    }
}

/// One USB bus, i.e. one root hub. xHCI controllers expose two: a USB 2 bus
/// and a USB 3 bus that share the same physical ports.
#[derive(Debug, Clone)]
pub struct Bus {
    pub busnum: u32,
    pub root: Device,
}

impl Bus {
    pub fn speed_mbps(&self) -> f64 {
        self.root.speed_mbps
    }

    pub fn label(&self) -> String {
        format!("Bus {} ({})", self.busnum, speed_name(self.root.speed_mbps))
    }
}

#[derive(Debug, Clone)]
pub struct Device {
    /// sysfs name, e.g. `usb1`, `1-4`, `1-4.2`.
    pub sysname: String,
    pub busnum: u32,
    pub devnum: u32,
    pub vendor_id: u16,
    pub product_id: u16,
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
    /// Negotiated link speed in Mbit/s (1.5, 12, 480, 5000, 10000, 20000...).
    pub speed_mbps: f64,
    /// USB version the device says it supports (bcdUSB), e.g. "3.20".
    pub usb_version: String,
    pub max_power: String,
    pub is_hub: bool,
    pub drivers: Vec<String>,
    pub sdr: Option<&'static str>,
    pub children: Vec<Device>,
}

impl Device {
    pub fn display_name(&self) -> String {
        if !self.product.is_empty() {
            self.product.clone()
        } else if self.is_hub {
            "USB hub".into()
        } else {
            format!("Device {:04x}:{:04x}", self.vendor_id, self.product_id)
        }
    }

    /// The port path shown to the user, e.g. "port 4.2" for `1-4.2`.
    pub fn port_path(&self) -> Option<&str> {
        self.sysname.split_once('-').map(|(_, p)| p)
    }

    /// True when a USB 3 device only got a USB 2 (or slower) link, e.g. through
    /// a USB 2 port, cable or hub. USB 2 devices aren't checked: plenty of
    /// full-speed devices report version 2.00 without supporting 480 Mb/s.
    pub fn below_capable_speed(&self) -> bool {
        let ver: f64 = self.usb_version.parse().unwrap_or(0.0);
        ver >= 3.0 && self.speed_mbps <= 480.0
    }

    pub fn walk<'a>(&'a self, out: &mut Vec<&'a Device>) {
        out.push(self);
        for c in &self.children {
            c.walk(out);
        }
    }
}

pub fn speed_name(mbps: f64) -> &'static str {
    match mbps as u64 {
        0..=2 => "USB 1 low speed, 1.5 Mb/s",
        3..=12 => "USB 1 full speed, 12 Mb/s",
        13..=480 => "USB 2, 480 Mb/s",
        481..=5000 => "USB 3, 5 Gb/s",
        5001..=10000 => "USB 3, 10 Gb/s",
        10001..=20000 => "USB 3, 20 Gb/s",
        _ => "USB4",
    }
}

pub fn speed_short(mbps: f64) -> &'static str {
    match mbps as u64 {
        0..=2 => "1.5M",
        3..=12 => "12M",
        13..=480 => "480M",
        481..=5000 => "5G",
        5001..=10000 => "10G",
        10001..=20000 => "20G",
        _ => "40G+",
    }
}

/// A flat, scanned picture of the system.
#[derive(Debug, Clone, Default)]
pub struct Topology {
    /// Motherboard maker and model, e.g. "ASRock X299 Professional Gaming i9".
    pub board: String,
    pub controllers: Vec<Controller>,
}

impl Topology {
    pub fn all_devices(&self) -> Vec<&Device> {
        let mut out = Vec::new();
        for c in &self.controllers {
            for b in &c.buses {
                b.root.walk(&mut out);
            }
        }
        out
    }

    pub fn sdr_count(&self) -> usize {
        self.all_devices().iter().filter(|d| d.sdr.is_some()).count()
    }
}

impl Controller {
    pub fn sdrs(&self) -> Vec<&Device> {
        let mut all = Vec::new();
        for b in &self.buses {
            b.root.walk(&mut all);
        }
        all.into_iter().filter(|d| d.sdr.is_some()).collect()
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn read_hex(path: &Path) -> u16 {
    u16::from_str_radix(read(path).trim_start_matches("0x"), 16).unwrap_or(0)
}

fn link_name(path: &Path) -> String {
    fs::read_link(path)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// USB device names look like `usb3` (root hub) or `3-1.4` (device). Interface
/// entries such as `3-1.4:1.0` are skipped.
fn is_device_name(name: &str) -> bool {
    !name.contains(':') && (name.starts_with("usb") || name.contains('-'))
}

/// The sysfs name of a device's parent: `1-4.2` -> `1-4`, `1-4` -> `usb1`.
pub fn parent_name(name: &str) -> Option<String> {
    let (bus, port) = name.split_once('-')?;
    Some(match port.rsplit_once('.') {
        Some((up, _)) => format!("{bus}-{up}"),
        None => format!("usb{bus}"),
    })
}

fn read_device(dir: &Path, sysname: &str, ids: &IdDb) -> Device {
    let vendor_id = read_hex(&dir.join("idVendor"));
    let product_id = read_hex(&dir.join("idProduct"));
    let mut manufacturer = read(&dir.join("manufacturer"));
    let mut product = read(&dir.join("product"));
    if manufacturer.is_empty() {
        manufacturer = ids.usb_vendor(vendor_id).unwrap_or_default();
    }
    if product.is_empty() {
        product = ids.usb_product(vendor_id, product_id).unwrap_or_default();
    }

    // Drivers bound to the device's interfaces (`1-4:1.0/driver`).
    let mut drivers = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with(sysname) && n.contains(':') {
                let d = link_name(&e.path().join("driver"));
                if !d.is_empty() && !drivers.contains(&d) {
                    drivers.push(d);
                }
            }
        }
    }
    drivers.sort();

    let is_hub = read(&dir.join("bDeviceClass")) == "09";
    let sdr = sdr::identify(vendor_id, product_id, &product);
    Device {
        sysname: sysname.to_string(),
        busnum: read(&dir.join("busnum")).parse().unwrap_or(0),
        devnum: read(&dir.join("devnum")).parse().unwrap_or(0),
        vendor_id,
        product_id,
        manufacturer,
        product,
        serial: read(&dir.join("serial")),
        speed_mbps: read(&dir.join("speed")).parse().unwrap_or(0.0),
        usb_version: read(&dir.join("version")),
        max_power: read(&dir.join("bMaxPower")),
        is_hub,
        drivers,
        sdr,
        children: Vec::new(),
    }
}

fn attach_children(dev: &mut Device, by_parent: &mut BTreeMap<String, Vec<Device>>) {
    let mut kids = by_parent.remove(&dev.sysname).unwrap_or_default();
    for k in &mut kids {
        attach_children(k, by_parent);
    }
    kids.sort_by_key(|k| port_sort_key(&k.sysname));
    dev.children = kids;
}

// Sort `1-10` after `1-9`.
fn port_sort_key(name: &str) -> Vec<u32> {
    name.split(['-', '.']).filter_map(|p| p.parse().ok()).collect()
}

/// Reads the controller the root hub hangs off. `pci_dir` is the canonical
/// sysfs path of the PCI device, e.g. `/sys/devices/pci0000:00/0000:00:14.0`.
fn read_controller(sys_root: &Path, pci_dir: &Path, ids: &IdDb) -> Controller {
    let pci_addr = pci_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let vendor = read_hex(&pci_dir.join("vendor"));
    let device = read_hex(&pci_dir.join("device"));
    let name = match (ids.pci_vendor(vendor), ids.pci_device(vendor, device)) {
        (Some(v), Some(d)) => format!("{v} {d}"),
        (Some(v), None) => format!("{v} USB controller {device:04x}"),
        _ => format!("USB controller {vendor:04x}:{device:04x}"),
    };

    let speed = read(&pci_dir.join("current_link_speed"));
    let width = read(&pci_dir.join("current_link_width"));
    let pcie_link = if speed.is_empty() || speed.starts_with("Unknown") {
        String::new()
    } else {
        format!("{speed} x{width}")
    };

    Controller {
        kind: classify(sys_root, pci_dir, &pci_addr, vendor),
        pci_addr,
        name,
        driver: link_name(&pci_dir.join("driver")),
        pcie_link,
        sysfs_path: pci_dir.to_path_buf(),
        vendor_id: vendor,
        subsystem_vendor: read_hex(&pci_dir.join("subsystem_vendor")),
        buses: Vec::new(),
    }
}

/// Works out whether a controller is built into the motherboard or on a card.
fn classify(sys_root: &Path, pci_dir: &Path, pci_addr: &str, vendor: u16) -> ControllerKind {
    // A physical PCIe slot lists the address of whatever is plugged into it.
    // On cards with a PCIe switch that's the switch, so check every PCI
    // device between the controller and the CPU.
    let slots: Vec<(String, String)> = fs::read_dir(sys_root.join("bus/pci/slots"))
        .map(|rd| rd.flatten().map(|s| (read(&s.path().join("address")), s.file_name().to_string_lossy().into_owned())).collect())
        .unwrap_or_default();
    let mut addr = pci_addr.to_string();
    let mut dir = pci_dir;
    loop {
        let slot_addr = addr.rsplit_once('.').map(|(a, _)| a).unwrap_or(&addr);
        if let Some((_, slot)) = slots.iter().find(|(a, _)| a == slot_addr) {
            return ControllerKind::AddOnCard { slot: slot.clone() };
        }
        match dir.parent() {
            Some(p) if is_pci_addr(p) => {
                dir = p;
                addr = p.file_name().unwrap().to_string_lossy().into_owned();
            }
            _ => break,
        }
    }
    // Sitting directly on the root complex (bus 00) means chipset.
    let on_root_bus = pci_dir
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n.to_string_lossy().starts_with("pci"));
    // Intel and AMD controllers behind an internal bridge are part of the CPU or chipset.
    const INTEL: u16 = 0x8086;
    const AMD: u16 = 0x1022;
    if on_root_bus || vendor == INTEL || vendor == AMD {
        ControllerKind::Motherboard
    } else {
        ControllerKind::Unknown
    }
}

fn is_pci_addr(p: &Path) -> bool {
    p.file_name().is_some_and(|n| {
        let n = n.to_string_lossy();
        n.len() == 12 && n.as_bytes()[4] == b':' && n.as_bytes()[7] == b':' && n.as_bytes()[10] == b'.'
    })
}

/// The PCIe switch a controller sits behind, if any. On a switch the path is
/// root port / switch upstream port / switch downstream port / controller.
fn switch_of(pci_dir: &Path) -> Option<PathBuf> {
    let down = pci_dir.parent()?;
    let up = down.parent()?;
    let root_port = up.parent()?;
    (is_pci_addr(down) && is_pci_addr(up) && is_pci_addr(root_port)).then(|| up.to_path_buf())
}

/// Scans `<sys_root>/bus/usb/devices`. Pass `/sys` on a real system.
pub fn scan(sys_root: &Path, ids: &IdDb) -> std::io::Result<Topology> {
    let devices_dir = sys_root.join("bus/usb/devices");
    let mut roots: Vec<(Device, PathBuf)> = Vec::new();
    let mut by_parent: BTreeMap<String, Vec<Device>> = BTreeMap::new();

    for e in fs::read_dir(&devices_dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !is_device_name(&name) {
            continue;
        }
        let Ok(real) = fs::canonicalize(e.path()) else { continue };
        let dev = read_device(&real, &name, ids);
        if name.starts_with("usb") {
            roots.push((dev, real));
        } else if let Some(p) = parent_name(&name) {
            by_parent.entry(p).or_default().push(dev);
        }
    }

    let mut controllers: BTreeMap<String, Controller> = BTreeMap::new();
    for (mut root, real) in roots {
        attach_children(&mut root, &mut by_parent);
        let pci_dir = real.parent().map(Path::to_path_buf).unwrap_or_default();
        let key = pci_dir.to_string_lossy().into_owned();
        let ctrl = controllers.entry(key).or_insert_with(|| read_controller(sys_root, &pci_dir, ids));
        ctrl.buses.push(Bus { busnum: root.busnum, root });
    }

    let mut controllers: Vec<Controller> = controllers.into_values().collect();
    for c in &mut controllers {
        c.buses.sort_by_key(|b| b.busnum);
    }

    // Several controllers behind one PCIe switch are almost always one card.
    let mut per_switch: BTreeMap<PathBuf, usize> = BTreeMap::new();
    for c in &controllers {
        if let Some(sw) = switch_of(&c.sysfs_path) {
            *per_switch.entry(sw).or_default() += 1;
        }
    }
    for c in &mut controllers {
        if c.kind != ControllerKind::Unknown {
            continue;
        }
        if let Some(sw) = switch_of(&c.sysfs_path) {
            let count = per_switch[&sw];
            if count > 1 {
                let addr = sw.file_name().unwrap().to_string_lossy();
                // Drop the "0000:" domain and ".0" function for a shorter label.
                let card = addr.get(5..10).unwrap_or(&addr).to_string();
                c.kind = ControllerKind::SharedCard { card, count };
            }
        }
    }

    // Extra USB chips soldered to the motherboard carry the board maker's
    // subsystem ID, the same one the chipset's own controller has. Plug-in
    // cards carry the card maker's.
    let board_ids: Vec<u16> = controllers
        .iter()
        .filter(|c| c.kind == ControllerKind::Motherboard && c.subsystem_vendor != 0 && c.subsystem_vendor != c.vendor_id)
        .map(|c| c.subsystem_vendor)
        .collect();
    for c in &mut controllers {
        // This also covers built-in Thunderbolt, whose hot-plug port shows up as a slot.
        let maybe_onboard = matches!(c.kind, ControllerKind::Unknown | ControllerKind::AddOnCard { .. });
        if maybe_onboard && board_ids.contains(&c.subsystem_vendor) {
            c.kind = ControllerKind::Motherboard;
        }
    }

    let dmi = sys_root.join("class/dmi/id");
    let board = format!("{} {}", read(&dmi.join("board_vendor")), read(&dmi.join("board_name"))).trim().to_string();
    controllers.sort_by_key(|c| c.buses.first().map(|b| b.busnum).unwrap_or(0));
    Ok(Topology { board, controllers })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_names() {
        assert_eq!(parent_name("1-4").as_deref(), Some("usb1"));
        assert_eq!(parent_name("1-4.2.3").as_deref(), Some("1-4.2"));
        assert_eq!(parent_name("usb1"), None);
        assert!(!is_device_name("1-4:1.0"));
        assert!(is_device_name("usb2"));
    }
}
