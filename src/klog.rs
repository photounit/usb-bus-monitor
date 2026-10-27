//! Follows the system log for USB trouble the topology and traffic counters
//! can't see: kernel resets, enumeration failures, over-current, xHCI and UAS
//! errors, disk I/O errors from USB drives, plus libusb errors and crashes
//! reported by SDR programs and services.
//!
//! Uses `journalctl -f`, which Ubuntu lets members of the `adm` group read
//! (the first account on a desktop install is one).

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

/// One log message worth showing.
#[derive(Clone, Debug, PartialEq)]
pub struct LogLine {
    /// When it was logged, e.g. "2026-10-27 16:38:10", local time.
    pub time: Option<String>,
    /// "kernel", or the program that logged it, e.g. "sdrplay_apiService".
    pub source: String,
    pub text: String,
}

impl LogLine {
    pub fn is_kernel(&self) -> bool {
        self.source == "kernel"
    }
}

#[derive(Clone, Default)]
pub struct KernelLog {
    lines: Arc<Mutex<Vec<LogLine>>>,
    pub running: Arc<Mutex<bool>>,
}

impl KernelLog {
    pub fn start() -> Self {
        let k = KernelLog::default();
        let bg = k.clone();
        thread::spawn(move || {
            let child = Command::new("journalctl")
                .args(["-f", "-n", "0", "-o", "short-iso", "--no-pager"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut child) = child else { return };
            *bg.running.lock().unwrap() = true;
            if let Some(out) = child.stdout.take() {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    if let Some(l) = parse(&line).filter(is_interesting) {
                        bg.lines.lock().unwrap().push(l);
                    }
                }
            }
            *bg.running.lock().unwrap() = false;
        });
        k
    }

    /// Takes the lines collected since the last call.
    pub fn drain(&self) -> Vec<LogLine> {
        std::mem::take(&mut *self.lines.lock().unwrap())
    }
}

/// Splits a `journalctl -o short-iso` line such as
/// "2026-10-27T16:38:10-04:00 rdioa kernel: usb 2-8.1: reset ...".
pub fn parse(line: &str) -> Option<LogLine> {
    let mut parts = line.splitn(3, ' ');
    let stamp = parts.next()?;
    let _host = parts.next()?;
    let (ident, text) = parts.next()?.split_once(": ")?;
    // The journal prints local time; keep "YYYY-MM-DD HH:MM:SS".
    let time = (stamp.len() >= 19 && stamp.as_bytes()[10] == b'T').then(|| stamp[..19].replacen('T', " ", 1));
    let source = ident.split('[').next().unwrap_or(ident).to_string();
    Some(LogLine { time, source, text: text.to_string() })
}

/// Whether a log message is a USB problem worth showing.
pub fn is_interesting(l: &LogLine) -> bool {
    if l.is_kernel() {
        is_interesting_kernel(&l.text)
    } else {
        is_interesting_program(&l.text)
    }
}

/// Messages from programs: libusb errors (SDR software uses libusb) and
/// services that crashed.
fn is_interesting_program(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.starts_with("libusb: error") || t.contains("libusb_error") || t.contains("exited, code=dumped")
}

/// Kernel messages. Plain connects and disconnects are left out because the
/// topology scan reports those.
fn is_interesting_kernel(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    if l.contains("segfault") && l.contains("libusb") {
        return true;
    }
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
pub fn is_serious(line: &LogLine) -> bool {
    let l = line.text.to_ascii_lowercase();
    ["over-current", "died", "halted", "i/o error", "unable to enumerate", "disabled by hub", "segfault", "code=dumped", "errno=12"].iter().any(|p| l.contains(p))
}

/// libusb's "out of memory" (ENOMEM), which almost always means the kernel's
/// usbfs_memory_mb cap on USB buffer memory was reached.
pub fn is_usb_memory_error(line: &LogLine) -> bool {
    let l = line.text.to_ascii_lowercase();
    l.starts_with("libusb: error") && (l.contains("errno=12") || l.contains("no_mem"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel(text: &str) -> LogLine {
        LogLine { time: None, source: "kernel".into(), text: text.into() }
    }

    fn is_interesting_text(text: &str) -> bool {
        is_interesting(&kernel(text))
    }

    #[test]
    fn parses_journal_lines() {
        let l = parse("2026-10-27T16:38:10-04:00 rdioa kernel: usb 2-8.1: reset SuperSpeed USB device number 5 using xhci_hcd").unwrap();
        assert_eq!(l.time.as_deref(), Some("2026-10-27 16:38:10"));
        assert!(l.is_kernel());
        assert_eq!(l.text, "usb 2-8.1: reset SuperSpeed USB device number 5 using xhci_hcd");
        assert!(is_interesting(&l));

        let l = parse("2026-10-27T16:37:45-04:00 rdioa sdrplay_apiService[1988964]: libusb: error [submit_iso_transfer] submiturb failed, errno=12").unwrap();
        assert_eq!(l.source, "sdrplay_apiService");
        assert!(is_interesting(&l) && is_serious(&l) && is_usb_memory_error(&l));

        let l = parse("2026-10-27T16:37:59-04:00 rdioa systemd[2857]: trunk-pro.service: Main process exited, code=dumped, status=6/ABRT").unwrap();
        assert!(is_interesting(&l) && is_serious(&l));

        let l = parse("2026-10-27T16:37:41-04:00 rdioa trunk-pro[1989069]: [2026-10-27 16:37:41.213973] (info) Concluding Recorded Call").unwrap();
        assert!(!is_interesting(&l));

        assert!(is_interesting_text("sdrplay_apiServ[1989141]: segfault at 20 ip 00007073e755299e sp 00007073dda7ab70 error 4 in libusb-1.0.so.0.5.0[d99e,7073e754a000+11000]"));
        assert!(parse("-- No entries --").is_none());
    }

    #[test]
    fn filters_usb_trouble() {
        assert!(is_interesting_text("usb 2-8.1: reset SuperSpeed USB device number 5 using xhci_hcd"));
        assert!(is_interesting_text("usb 6-2: device descriptor read/64, error -71"));
        assert!(is_interesting_text("I/O error, dev sdb, sector 1234 op 0x0:(READ)"));
        assert!(is_interesting_text("xhci_hcd 0000:00:14.0: xHCI host controller not responding, assume dead"));
        assert!(is_interesting_text("sd 6:0:0:0: [sdb] tag#3 uas_eh_abort_handler 0 uas-tag 1 inflight: CMD IN"));
        assert!(!is_interesting_text("usb 2-8.1: new SuperSpeed USB device number 5 using xhci_hcd"));
        assert!(!is_interesting_text("usb 1-4: USB disconnect, device number 3"));
        assert!(!is_interesting_text("audit: type=1400 apparmor=\"DENIED\""));
        assert!(is_serious(&kernel("I/O error, dev sdb, sector 1234")));
        assert!(!is_serious(&kernel("usb 2-8.1: reset SuperSpeed USB device number 5 using xhci_hcd")));
    }
}
