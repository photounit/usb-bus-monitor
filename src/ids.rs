//! Vendor and product names from the system's `pci.ids` and `usb.ids` files
//! (package `hwdata` on Ubuntu). Missing files just mean fewer friendly names.

use std::collections::HashMap;
use std::fs;

#[derive(Debug, Default)]
pub struct IdDb {
    pci_vendors: HashMap<u16, String>,
    pci_devices: HashMap<(u16, u16), String>,
    usb_vendors: HashMap<u16, String>,
    usb_products: HashMap<(u16, u16), String>,
}

const PCI_PATHS: &[&str] = &["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids"];
const USB_PATHS: &[&str] = &["/usr/share/hwdata/usb.ids", "/var/lib/usbutils/usb.ids", "/usr/share/misc/usb.ids"];

impl IdDb {
    pub fn load_system() -> Self {
        let mut db = IdDb::default();
        if let Some(text) = PCI_PATHS.iter().find_map(|p| fs::read_to_string(p).ok()) {
            parse(&text, &mut db.pci_vendors, &mut db.pci_devices);
        }
        if let Some(text) = USB_PATHS.iter().find_map(|p| fs::read_to_string(p).ok()) {
            parse(&text, &mut db.usb_vendors, &mut db.usb_products);
        }
        db
    }

    pub fn pci_vendor(&self, v: u16) -> Option<String> {
        self.pci_vendors.get(&v).cloned().or_else(|| fallback_pci_vendor(v).map(String::from))
    }

    pub fn pci_device(&self, v: u16, d: u16) -> Option<String> {
        self.pci_devices.get(&(v, d)).cloned()
    }

    pub fn usb_vendor(&self, v: u16) -> Option<String> {
        self.usb_vendors.get(&v).cloned()
    }

    pub fn usb_product(&self, v: u16, p: u16) -> Option<String> {
        self.usb_products.get(&(v, p)).cloned()
    }
}

// The usual USB controller makers, for systems without hwdata installed.
fn fallback_pci_vendor(v: u16) -> Option<&'static str> {
    Some(match v {
        0x8086 => "Intel",
        0x1022 => "AMD",
        0x1b21 => "ASMedia",
        0x1912 => "Renesas",
        0x1106 => "VIA",
        0x104c => "Texas Instruments",
        0x1b73 => "Fresco Logic",
        0x10de => "NVIDIA",
        _ => return None,
    })
}

/// Parses the shared ids format: `vvvv  Vendor` lines, then tab-indented
/// `\tdddd  Device` lines. Deeper (subsystem) lines and the trailing class
/// section are ignored.
fn parse(text: &str, vendors: &mut HashMap<u16, String>, devices: &mut HashMap<(u16, u16), String>) {
    let mut vendor: Option<u16> = None;
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if line.starts_with("C ") {
            break;
        }
        if let Some(rest) = line.strip_prefix('\t') {
            if rest.starts_with('\t') {
                continue;
            }
            if let (Some(v), Some((id, name))) = (vendor, split_id(rest)) {
                devices.insert((v, id), name);
            }
        } else if let Some((id, name)) = split_id(line) {
            vendor = Some(id);
            vendors.insert(id, name);
        } else {
            vendor = None;
        }
    }
}

fn split_id(s: &str) -> Option<(u16, String)> {
    let (id, name) = s.split_once("  ")?;
    Some((u16::from_str_radix(id, 16).ok()?, name.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ids() {
        let mut v = HashMap::new();
        let mut d = HashMap::new();
        parse("# c\n8086  Intel Corporation\n\ta0ed  Tiger Lake-LP USB 3.2 xHCI\n\t\t1028 0a1f  sub\nC 0c  Serial\n", &mut v, &mut d);
        assert_eq!(v[&0x8086], "Intel Corporation");
        assert_eq!(d[&(0x8086, 0xa0ed)], "Tiger Lake-LP USB 3.2 xHCI");
        assert_eq!(d.len(), 1);
    }
}
