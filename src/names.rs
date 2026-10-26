//! Names the user gives controllers, kept in ~/.config/usb-monitor/names.txt
//! as `PCI address = name` lines.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Default)]
pub struct Names {
    map: BTreeMap<String, String>,
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("usb-monitor/names.txt"))
}

impl Names {
    pub fn load() -> Self {
        let text = path().and_then(|p| fs::read_to_string(p).ok()).unwrap_or_default();
        let map = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .filter(|(k, v)| !k.is_empty() && !v.is_empty())
            .collect();
        Names { map }
    }

    pub fn get(&self, pci_addr: &str) -> Option<&str> {
        self.map.get(pci_addr).map(String::as_str)
    }

    /// Sets (or with an empty name, clears) a name and saves the file.
    pub fn set(&mut self, pci_addr: &str, name: &str) -> std::io::Result<()> {
        let name = name.trim();
        if name.is_empty() {
            self.map.remove(pci_addr);
        } else {
            self.map.insert(pci_addr.to_string(), name.to_string());
        }
        let Some(p) = path() else { return Ok(()) };
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir)?;
        }
        let text: String = self.map.iter().map(|(k, v)| format!("{k} = {v}\n")).collect();
        fs::write(p, text)
    }
}
