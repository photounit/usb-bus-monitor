mod gui;
mod ids;
mod install;
mod issues;
mod klog;
mod names;
mod rates;
mod sdr;
mod sysfs;
mod traffic;

use std::path::PathBuf;

use ids::IdDb;
use sysfs::{Device, Topology};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut sys_root = PathBuf::from("/sys");
    let mut dump = false;
    let mut demo = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dump" => dump = true,
            "--install" => return Ok(install::install()?),
            "--uninstall" => return Ok(install::uninstall()?),
            // Made-up traffic, for screenshots on machines without SDRs.
            "--demo-traffic" => demo = true,
            "--sys-root" => sys_root = args.next().ok_or("--sys-root needs a path")?.into(),
            "-V" | "--version" => {
                println!("USB Bus Monitor {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                println!(
                    "usb-monitor [--install | --uninstall | --dump | --version] [--sys-root PATH]\n\n  \
                     --install        add USB Bus Monitor to the app menu (no sudo needed)\n  \
                     --uninstall      remove it from the app menu\n  \
                     --dump           print the USB layout as text instead of opening the window\n  \
                     --version        print the version number\n  \
                     --sys-root PATH  read a copy of /sys from PATH (for testing)"
                );
                return Ok(());
            }
            other => return Err(format!("unknown option {other}").into()),
        }
    }

    let ids = IdDb::load_system();
    if dump {
        let topo = sysfs::scan(&sys_root, &ids)?;
        print!("{}", render_text(&topo));
        return Ok(());
    }
    gui::run(sys_root, ids, demo)?;
    Ok(())
}

/// Plain-text version of the tree, for `--dump`.
pub fn render_text(topo: &Topology) -> String {
    let mut s = String::new();
    if !topo.board.is_empty() {
        s += &format!("Board: {}\n", topo.board);
    }
    for c in &topo.controllers {
        s += &format!("{}  [{}]  PCI {}", c.name, c.kind.label(), c.pci_addr);
        if !c.pcie_link.is_empty() {
            s += &format!("  link {}", c.pcie_link);
        }
        let sdrs = c.sdrs().len();
        if sdrs > 0 {
            s += &format!("  SDRs: {sdrs}");
        }
        s += "\n";
        for b in &c.buses {
            s += &format!("  {}\n", b.label());
            for d in &b.root.children {
                dump_device(&mut s, d, 2);
            }
        }
    }
    s
}

fn dump_device(s: &mut String, d: &Device, depth: usize) {
    let indent = "  ".repeat(depth);
    let port = d.port_path().unwrap_or("?");
    s.push_str(&format!("{indent}port {port}: {} [{:04x}:{:04x}] {}", d.display_name(), d.vendor_id, d.product_id, sysfs::speed_short(d.speed_mbps)));
    if let Some(kind) = d.sdr {
        s.push_str(&format!("  SDR: {kind}"));
    }
    if d.below_capable_speed() {
        s.push_str(&format!("  (supports USB {}, running slower)", d.usb_version));
    }
    s.push('\n');
    for c in &d.children {
        dump_device(s, c, depth + 1);
    }
}
