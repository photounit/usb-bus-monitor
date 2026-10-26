//! Builds a fake /sys tree and checks the `--dump` output.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

fn write(dir: &Path, attrs: &[(&str, &str)]) {
    fs::create_dir_all(dir).unwrap();
    for (k, v) in attrs {
        fs::write(dir.join(k), format!("{v}\n")).unwrap();
    }
}

fn usb(root: &Path, pci: &str, name: &str, sub: &str, attrs: &[(&str, &str)]) {
    let dir = root.join("devices").join(pci).join(sub);
    write(&dir, attrs);
    symlink(&dir, root.join("bus/usb/devices").join(name)).unwrap();
}

fn sdr_iface(root: &Path, pci: &str, sub: &str, iface: &str) {
    let dir = root.join("devices").join(pci).join(sub).join(iface);
    fs::create_dir_all(&dir).unwrap();
    let drv = root.join("bus/usb/drivers/usbfs");
    fs::create_dir_all(&drv).unwrap();
    symlink(&drv, dir.join("driver")).unwrap();
}

#[test]
fn dump_fake_system() {
    let root = std::env::temp_dir().join(format!("usbmon-fake-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("bus/usb/devices")).unwrap();

    // Intel chipset controller on the root bus: a USB 2 and a USB 3 bus.
    let intel = "pci0000:00/0000:00:14.0";
    write(&root.join("devices").join(intel), &[("vendor", "0x8086"), ("device", "0x7ae0"), ("subsystem_vendor", "0x1849")]);
    write(&root.join("class/dmi/id"), &[("board_vendor", "ASRock"), ("board_name", "X299 Professional Gaming i9")]);
    usb(&root, intel, "usb1", "usb1", &[("busnum", "1"), ("devnum", "1"), ("speed", "480"), ("bDeviceClass", "09"), ("product", "xHCI Host Controller")]);
    usb(&root, intel, "usb2", "usb2", &[("busnum", "2"), ("devnum", "1"), ("speed", "5000"), ("bDeviceClass", "09")]);
    usb(&root, intel, "1-3", "usb1/1-3", &[("busnum", "1"), ("devnum", "2"), ("speed", "480"), ("bDeviceClass", "09"), ("version", " 2.00")]);
    for (n, dev) in [("1-3.1", "3"), ("1-3.2", "4")] {
        let sub = format!("usb1/1-3/{n}");
        usb(&root, intel, n, &sub, &[("busnum", "1"), ("devnum", dev), ("speed", "480"), ("idVendor", "0bda"), ("idProduct", "2838"), ("product", "RTL2838UHIDIR"), ("version", " 2.00")]);
        sdr_iface(&root, intel, &sub, &format!("{n}:1.0"));
    }
    // A USB 3 drive stuck at USB 2 speed.
    usb(&root, intel, "1-10", "usb1/1-10", &[("busnum", "1"), ("devnum", "5"), ("speed", "480"), ("idVendor", "0781"), ("idProduct", "5581"), ("product", "Ultra"), ("version", " 3.20")]);
    // An unrelated interface entry that must be ignored.
    fs::create_dir_all(root.join("devices").join(intel).join("usb1/1-10/1-10:1.0")).unwrap();
    symlink(root.join("devices").join(intel).join("usb1/1-10/1-10:1.0"), root.join("bus/usb/devices/1-10:1.0")).unwrap();

    // Renesas card in slot 2.
    let card = "pci0000:00/0000:00:1c.0/0000:03:00.0";
    write(&root.join("devices").join(card), &[("vendor", "0x1912"), ("device", "0x0014"), ("current_link_speed", "5.0 GT/s PCIe"), ("current_link_width", "1")]);
    write(&root.join("bus/pci/slots/2"), &[("address", "0000:03:00")]);
    usb(&root, card, "usb3", "usb3", &[("busnum", "3"), ("devnum", "1"), ("speed", "480"), ("bDeviceClass", "09")]);
    usb(&root, card, "3-1", "usb3/3-1", &[("busnum", "3"), ("devnum", "2"), ("speed", "480"), ("idVendor", "1d50"), ("idProduct", "60a1"), ("product", "AIRSPY")]);

    // ASMedia chip behind a bridge with no slot: unknown.
    let asm = "pci0000:00/0000:00:1d.0/0000:05:00.0";
    write(&root.join("devices").join(asm), &[("vendor", "0x1b21"), ("device", "0x2142"), ("subsystem_vendor", "0x1b21")]);
    usb(&root, asm, "usb4", "usb4", &[("busnum", "4"), ("devnum", "1"), ("speed", "10000"), ("bDeviceClass", "09")]);
    // The same chip with the board maker's subsystem ID: onboard.
    let asm2 = "pci0000:00/0000:00:1c.6/0000:0b:00.0";
    write(&root.join("devices").join(asm2), &[("vendor", "0x1b21"), ("device", "0x2142"), ("subsystem_vendor", "0x1849")]);
    usb(&root, asm2, "usb5", "usb5", &[("busnum", "5"), ("devnum", "1"), ("speed", "10000"), ("bDeviceClass", "09")]);

    // Built-in Thunderbolt: its hot-plug port is listed as a slot, but it
    // carries the board maker's subsystem ID.
    let tb = "pci0000:00/0000:00:07.0/0000:20:00.0";
    write(&root.join("devices").join(tb), &[("vendor", "0x8086"), ("device", "0x1138"), ("subsystem_vendor", "0x1849")]);
    write(&root.join("bus/pci/slots/8"), &[("address", "0000:00:07")]);
    usb(&root, tb, "usb20", "usb20", &[("busnum", "20"), ("devnum", "1"), ("speed", "10000"), ("bDeviceClass", "09")]);

    // A four-controller card: PCIe switch 0000:06:00.0 with controllers 0000:08:0N.0.
    for n in 0..4u8 {
        let ctrl = format!("pci0000:00/0000:00:01.0/0000:06:00.0/0000:07:0{n}.0/0000:08:0{n}.0");
        write(&root.join("devices").join(&ctrl), &[("vendor", "0x1912"), ("device", "0x0015")]);
        let bus = 10 + n as u32;
        usb(&root, &ctrl, &format!("usb{bus}"), &format!("usb{bus}"), &[("busnum", &bus.to_string()), ("devnum", "1"), ("speed", "480"), ("bDeviceClass", "09")]);
    }
    // A full-speed cooler that reports USB 2.00: not a problem.
    usb(&root, intel, "1-2", "usb1/1-2", &[("busnum", "1"), ("devnum", "6"), ("speed", "12"), ("idVendor", "1b1c"), ("idProduct", "0c17"), ("product", "H115i Platinum"), ("version", " 2.00")]);

    let out = Command::new(env!("CARGO_BIN_EXE_usb-monitor")).args(["--dump", "--sys-root"]).arg(&root).output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    println!("{text}");
    // Set USBMON_KEEP_FAKE=1 to keep the tree, e.g. to open it in the GUI.
    if std::env::var_os("USBMON_KEEP_FAKE").is_none() {
        fs::remove_dir_all(&root).unwrap();
    }

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("[Motherboard]  PCI 0000:00:14.0  SDRs: 2"), "{text}");
    assert!(text.contains("Bus 1 (USB 2, 480 Mb/s)") && text.contains("Bus 2 (USB 3, 5 Gb/s)"));
    assert!(text.contains("port 3.1: RTL2838UHIDIR [0bda:2838] 480M  SDR: RTL-SDR"));
    assert!(text.contains("port 10: Ultra [0781:5581] 480M  (supports USB 3.20, running slower)"));
    assert!(text.contains("[Add-on card (slot 2)]  PCI 0000:03:00.0  link 5.0 GT/s PCIe x1  SDRs: 1"));
    assert!(text.contains("[Add-on card or extra onboard chip]  PCI 0000:05:00.0"));
    assert!(text.contains("[Motherboard]  PCI 0000:0b:00.0"), "{text}");
    assert!(text.contains("[Motherboard]  PCI 0000:20:00.0"), "{text}");
    assert!(text.starts_with("Board: ASRock X299 Professional Gaming i9\n"), "{text}");
    assert!(!text.contains("1-10:1.0"));
    assert_eq!(text.matches("[Add-on card 06:00 (4 controllers)]").count(), 4, "{text}");
    assert!(text.contains("port 2: H115i Platinum [1b1c:0c17] 12M\n"), "{text}");
    // Port 10 sorts after port 3.
    assert!(text.find("port 3:").unwrap() < text.find("port 10:").unwrap());
}
