//! Follows the kernel log for USB trouble the topology and traffic counters
//! can't see: resets, enumeration failures, over-current, xHCI and UAS errors,
//! and disk I/O errors from USB drives.
//!
//! Uses `journalctl -k -f`, which Ubuntu lets members of the `adm` group read
//! (the first account on a desktop install is one).

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Clone, Default)]
pub struct KernelLog {
    lines: Arc<Mutex<Vec<String>>>,
    pub running: Arc<Mutex<bool>>,
}

impl KernelLog {
    pub fn start() -> Self {
        let k = KernelLog::default();
        let bg = k.clone();
        thread::spawn(move || {
            let child = Command::new("journalctl")
                .args(["-k", "-f", "-n", "0", "-o", "cat", "--no-pager"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut child) = child else { return };
            *bg.running.lock().unwrap() = true;
            if let Some(out) = child.stdout.take() {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    if is_interesting(&line) {
                        bg.lines.lock().unwrap().push(line);
                    }
                }
            }
            *bg.running.lock().unwrap() = false;
        });
        k
    }

    /// Takes the lines collected since the last call.
    pub fn drain(&self) -> Vec<String> {
        std::mem::take(&mut *self.lines.lock().unwrap())
    }
}

/// Whether a kernel message is a USB problem worth showing. Plain connects
/// and disconnects are left out because the topology scan reports those.
pub fn is_interesting(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        "reset high-speed",
        "reset superspeed",
        "reset full-speed",
        "reset low-speed",
        "over-current",
        "device descriptor read",
        "device not accepting address",
        "unable to enumerate",
        "cannot enable",
        "can't set config",
        "uas_",
        "host controller died",
        "hc died",
        "halted",
        "babble",
        "transfer event for disabled endpoint",
        "not responding",
        "i/o error",
        "usb_reset",
        "disabled by hub",
    ];
    let usb_related = l.starts_with("usb ") || l.contains("xhci") || l.contains("uas") || l.contains("hub ") || l.contains("i/o error") || l.contains("usbfs");
    usb_related && PATTERNS.iter().any(|p| l.contains(p))
}

/// Problem lines are the ones that mean data was lost or a controller gave up.
pub fn is_serious(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    ["over-current", "died", "halted", "i/o error", "unable to enumerate", "disabled by hub"].iter().any(|p| l.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_usb_trouble() {
        assert!(is_interesting("usb 2-8.1: reset SuperSpeed USB device number 5 using xhci_hcd"));
        assert!(is_interesting("usb 6-2: device descriptor read/64, error -71"));
        assert!(is_interesting("I/O error, dev sdb, sector 1234 op 0x0:(READ)"));
        assert!(is_interesting("xhci_hcd 0000:00:14.0: xHCI host controller not responding, assume dead"));
        assert!(is_interesting("sd 6:0:0:0: [sdb] tag#3 uas_eh_abort_handler 0 uas-tag 1 inflight: CMD IN"));
        assert!(!is_interesting("usb 2-8.1: new SuperSpeed USB device number 5 using xhci_hcd"));
        assert!(!is_interesting("usb 1-4: USB disconnect, device number 3"));
        assert!(!is_interesting("audit: type=1400 apparmor=\"DENIED\""));
        assert!(is_serious("I/O error, dev sdb, sector 1234"));
        assert!(!is_serious("usb 2-8.1: reset SuperSpeed USB device number 5 using xhci_hcd"));
    }
}
