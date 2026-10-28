//! Turns topology scans, traffic rates and kernel messages into a list of
//! current issues plus a timestamped history, which is also appended to
//! ~/.local/state/usb-monitor/events.log.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::klog;
use crate::rates::{fmt_rate, usable_bytes_per_sec, Rates};
use crate::sysfs::{self, Controller, Device, Topology};
use crate::traffic;

/// Fraction of usable bandwidth that counts as a bottleneck.
const BUSY: f64 = 0.8;
/// Seconds a link must stay busy before it's reported, so short bursts don't count.
const BUSY_SECS: u32 = 3;
/// SDRs below this rate are treated as idle rather than streaming.
const STREAMING: f64 = 500_000.0;
const ERROR_EVENT_GAP: Duration = Duration::from_secs(30);
const RECENT_ERRORS: Duration = Duration::from_secs(60);
const STOPPED_ACTIVE_FOR: Duration = Duration::from_secs(60);
const MAX_EVENTS: usize = 2000;
/// How long a program's repeated log message is kept to one history entry.
const REPEAT_GAP: Duration = Duration::from_secs(60);
/// How long running out of USB buffer memory stays on the "Right now" list.
const USB_MEMORY_ACTIVE_FOR: Duration = Duration::from_secs(300);
/// Below this usbfs_memory_mb, several SDRs at once can run out of buffers.
const USB_MEMORY_ENOUGH_MB: u32 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
    Problem,
}

impl Severity {
    fn tag(self) -> &'static str {
        match self {
            Severity::Info => "INFO",
            Severity::Warning => "WARNING",
            Severity::Problem => "PROBLEM",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Event {
    pub time: String,
    pub severity: Severity,
    pub text: String,
}

/// Something wrong right now.
#[derive(Clone, Debug)]
pub struct Issue {
    pub severity: Severity,
    pub text: String,
    /// The device or `pci:` controller it's about, for highlighting.
    pub target: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct Known {
    devnum: u32,
    ids: (u16, u16),
    name: String,
    is_sdr: bool,
    place: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Stream {
    Normal,
    Low,
    Stopped(Instant),
}

#[derive(Clone)]
pub struct Watcher {
    known: Option<HashMap<String, Known>>,
    hot: HashMap<String, u32>,
    streams: HashMap<String, Stream>,
    prev_errors: HashMap<(u16, u8), u64>,
    pending_errors: HashMap<String, (u64, Instant)>,
    last_error: HashMap<String, Instant>,
    prev_lost: u64,
    last_lost_event: Option<Instant>,
    /// Program log messages already in the history, to collapse repeats.
    repeats: HashMap<String, (Instant, u32)>,
    last_usb_memory_error: Option<Instant>,
    pub events: VecDeque<Event>,
    pub active: Vec<Issue>,
    log: Option<Arc<File>>,
    pub log_path: Option<PathBuf>,
}

fn log_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("usb-monitor/events.log"))
}

/// Local wall-clock time, e.g. "2026-10-26 16:35:01".
pub fn now_string() -> String {
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn place(d: &Device) -> String {
    format!("bus {} port {}", d.busnum, d.port_path().unwrap_or("?"))
}

fn label(d: &Device) -> String {
    match d.sdr {
        Some(kind) if !d.display_name().contains(kind) => format!("{} ({kind})", d.display_name()),
        _ => d.display_name(),
    }
}

/// Usable bytes per second of a controller's PCIe link, from text like
/// "8.0 GT/s PCIe x1".
pub fn pcie_usable(link: &str) -> Option<f64> {
    let gt: f64 = link.split_whitespace().next()?.parse().ok()?;
    let width: f64 = link.rsplit_once('x')?.1.trim().parse().ok()?;
    // PCIe 1 and 2 use 8b/10b encoding, PCIe 3 and later 128b/130b.
    let encoding = if gt < 8.0 { 0.8 } else { 128.0 / 130.0 };
    // Packet headers and flow control take roughly another 20%.
    Some(gt * 1e9 * encoding / 8.0 * width * 0.8)
}

impl Watcher {
    pub fn new() -> Self {
        Self::with_log(log_path())
    }

    pub fn with_log(log_path: Option<PathBuf>) -> Self {
        let log = log_path.as_ref().and_then(|p| {
            std::fs::create_dir_all(p.parent()?).ok()?;
            OpenOptions::new().create(true).append(true).open(p).ok().map(Arc::new)
        });
        let mut w = Watcher {
            known: None,
            hot: HashMap::new(),
            streams: HashMap::new(),
            prev_errors: HashMap::new(),
            pending_errors: HashMap::new(),
            last_error: HashMap::new(),
            prev_lost: 0,
            last_lost_event: None,
            repeats: HashMap::new(),
            last_usb_memory_error: None,
            events: VecDeque::new(),
            active: Vec::new(),
            log,
            log_path,
        };
        w.event(Severity::Info, "USB Bus Monitor started".into());
        w
    }

    fn event(&mut self, severity: Severity, text: String) {
        self.event_at(now_string(), severity, text);
    }

    fn event_at(&mut self, time: String, severity: Severity, text: String) {
        if let Some(f) = &self.log {
            let _ = writeln!(&**f, "{time}  {:<7}  {text}", severity.tag());
        }
        if self.events.len() == MAX_EVENTS {
            self.events.pop_back();
        }
        self.events.push_front(Event { time, severity, text });
    }

    pub fn problem_count(&self) -> usize {
        self.active.iter().filter(|i| i.severity == Severity::Problem).count()
    }

    /// The worst current issue about a device or controller, if any.
    pub fn issue_for(&self, target: &str) -> Option<&Issue> {
        self.active.iter().filter(|i| i.target.as_deref() == Some(target)).max_by_key(|i| i.severity)
    }

    /// Compares a fresh topology scan with the last one to log plug events.
    pub fn on_scan(&mut self, topo: &Topology) {
        let now: HashMap<String, Known> = topo
            .all_devices()
            .into_iter()
            .filter(|d| !d.sysname.starts_with("usb"))
            .map(|d| {
                let k = Known { devnum: d.devnum, ids: (d.vendor_id, d.product_id), name: label(d), is_sdr: d.sdr.is_some(), place: place(d) };
                (d.sysname.clone(), k)
            })
            .collect();
        let Some(before) = self.known.replace(now.clone()) else { return };

        // Only report the top-most device of a change: when a hub goes, so does
        // everything behind it.
        let parent_changed = |name: &str, set: &dyn Fn(&str) -> bool| sysfs::parent_name(name).is_some_and(|p| set(&p));

        let mut sorted: Vec<&String> = before.keys().chain(now.keys()).collect();
        sorted.sort();
        sorted.dedup();
        for name in sorted {
            match (before.get(name), now.get(name)) {
                (Some(old), None) => {
                    if !parent_changed(name, &|p| before.contains_key(p) && !now.contains_key(p)) {
                        let sev = if old.is_sdr { Severity::Problem } else { Severity::Warning };
                        self.event(sev, format!("Disconnected: {} from {}", old.name, old.place));
                    }
                }
                (None, Some(new)) => {
                    if !parent_changed(name, &|p| now.contains_key(p) && !before.contains_key(p)) {
                        self.event(Severity::Info, format!("Connected: {} on {}", new.name, new.place));
                    }
                }
                (Some(old), Some(new)) if old.devnum != new.devnum || old.ids != new.ids => {
                    let reconnected = |p: &str| matches!((before.get(p), now.get(p)), (Some(a), Some(b)) if a.devnum != b.devnum);
                    if !parent_changed(name, &reconnected) {
                        if old.ids == new.ids {
                            self.event(Severity::Problem, format!("Dropped off and reconnected: {} on {}", new.name, new.place));
                        } else {
                            self.event(Severity::Info, format!("Swapped: {} replaced by {} on {}", old.name, new.name, new.place));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Adds log messages to the history, stamped with the time they were
    /// logged rather than when they arrived here.
    pub fn on_kernel_lines(&mut self, lines: Vec<klog::LogLine>, topo: &Topology) {
        const MAX_PER_SAMPLE: usize = 20;
        let now = Instant::now();
        let mut shown = 0;
        let mut skipped = 0;
        for line in lines {
            if klog::is_usb_memory_error(&line) {
                self.last_usb_memory_error = Some(now);
            }
            if !line.is_kernel() && self.is_repeat(&line, now) {
                continue;
            }
            if shown == MAX_PER_SAMPLE {
                skipped += 1;
                continue;
            }
            shown += 1;
            let sev = if klog::is_serious(&line) { Severity::Problem } else { Severity::Warning };
            let text = if line.is_kernel() {
                format!("Kernel: {}{}", line.text.trim(), device_hint(&line.text, topo))
            } else {
                format!("{}: {}{}", line.source, line.text.trim(), program_hint(&line))
            };
            self.event_at(line.time.unwrap_or_else(now_string), sev, text);
        }
        if skipped > 0 {
            self.event(Severity::Warning, format!("…and {skipped} more USB messages in the same second"));
        }
    }

    /// Programs often log the same error many times a second. Keeps the first
    /// in the history and counts the rest for a minute.
    fn is_repeat(&mut self, line: &klog::LogLine, now: Instant) -> bool {
        let key = format!("{}: {}", line.source, line.text);
        self.repeats.retain(|_, (t, _)| now.duration_since(*t) < REPEAT_GAP);
        match self.repeats.get_mut(&key) {
            Some((_, n)) => {
                *n += 1;
                true
            }
            None => {
                self.repeats.insert(key, (now, 0));
                false
            }
        }
    }

    /// Checks rates and error counters after each one-second sample.
    pub fn on_sample(&mut self, topo: &Topology, rates: &Rates, counters: &traffic::Counters, live: bool) {
        let mut active = Vec::new();
        let now = Instant::now();

        self.check_usb_memory(topo, now, &mut active);

        for d in topo.all_devices() {
            if d.below_capable_speed() {
                active.push(Issue {
                    severity: Severity::Warning,
                    text: format!("{} supports USB {} but is running at {} ({})", label(d), d.usb_version, sysfs::speed_name(d.speed_mbps), place(d)),
                    target: Some(d.sysname.clone()),
                });
            }
        }

        if live {
            for c in &topo.controllers {
                self.check_controller(c, rates, &mut active);
            }
            self.check_streams(topo, rates, now, &mut active);
            self.check_errors(topo, counters, now, &mut active);

            if counters.lost_events > self.prev_lost {
                let n = counters.lost_events - self.prev_lost;
                if self.last_lost_event.is_none_or(|t| now.duration_since(t) > Duration::from_secs(60)) {
                    self.event(Severity::Warning, format!("The kernel dropped {n} traffic events (system busy), so rates may read low"));
                    self.last_lost_event = Some(now);
                }
                self.prev_lost = counters.lost_events;
            }
        }

        active.sort_by(|a, b| b.severity.cmp(&a.severity));
        self.active = active;
    }

    /// Programs reading SDRs through libusb share one kernel cap on transfer
    /// buffer memory (usbfs_memory_mb, 16 MB by default). Fast SDRs can need
    /// more than that between them, and then streams fail or crash.
    fn check_usb_memory(&mut self, topo: &Topology, now: Instant, active: &mut Vec<Issue>) {
        let limit = match topo.usbfs_memory_mb {
            Some(0) => "no limit".to_string(),
            Some(mb) => format!("{mb} MB"),
            None => "unknown".to_string(),
        };
        if self.last_usb_memory_error.is_some_and(|t| now.duration_since(t) < USB_MEMORY_ACTIVE_FOR) {
            active.push(Issue {
                severity: Severity::Problem,
                text: format!(
                    "SDR software ran out of USB buffer memory (libusb errno 12). The kernel's limit is {limit}; raise usbfs_memory_mb (see README)"
                ),
                target: None,
            });
        } else if let Some(mb @ 1..USB_MEMORY_ENOUGH_MB) = topo.usbfs_memory_mb
            && topo.sdr_count() >= 2
        {
            active.push(Issue {
                severity: Severity::Warning,
                text: format!(
                    "USB buffer memory is limited to {mb} MB for all programs, which is often too little for {} SDRs at once; raise usbfs_memory_mb (see README)",
                    topo.sdr_count()
                ),
                target: None,
            });
        }
    }

    /// Records how long a link has been busy and logs it when it first counts.
    fn busy(&mut self, key: String, frac: f64, what: String) -> bool {
        let n = self.hot.entry(key).or_default();
        *n = if frac >= BUSY { *n + 1 } else { 0 };
        let n = *n;
        if n == BUSY_SECS {
            self.event(Severity::Problem, format!("{what} is {:.0}% busy", frac * 100.0));
        }
        n >= BUSY_SECS
    }

    fn check_controller(&mut self, c: &Controller, rates: &Rates, active: &mut Vec<Issue>) {
        let cname = c.name.clone();
        for b in &c.buses {
            let used = rates.buses.get(&b.busnum).map_or(0.0, |s| s.now());
            let usable = usable_bytes_per_sec(b.speed_mbps());
            let frac = used / usable;
            let what = format!("Bus {} on {}", b.busnum, cname);
            if self.busy(format!("bus{}", b.busnum), frac, what.clone()) {
                active.push(Issue {
                    severity: Severity::Problem,
                    text: format!("{what} is {:.0}% busy ({} of about {} usable)", frac * 100.0, fmt_rate(used), fmt_rate(usable)),
                    target: Some(format!("pci:{}", c.pci_addr)),
                });
            }
            let mut hubs = Vec::new();
            b.root.walk(&mut hubs);
            for h in hubs.into_iter().filter(|h| h.is_hub && !h.sysname.starts_with("usb")) {
                let mut below = Vec::new();
                for ch in &h.children {
                    ch.walk(&mut below);
                }
                let used: f64 = below.iter().filter_map(|d| rates.device(d)).map(|s| s.now()).sum();
                let usable = usable_bytes_per_sec(h.speed_mbps);
                let frac = used / usable;
                let what = format!("Hub at {} ({} link)", place(h), sysfs::speed_short(h.speed_mbps));
                if self.busy(h.sysname.clone(), frac, what.clone()) {
                    active.push(Issue {
                        severity: Severity::Problem,
                        text: format!("{what} is {:.0}% busy ({} of about {}); devices behind it share that one link", frac * 100.0, fmt_rate(used), fmt_rate(usable)),
                        target: Some(h.sysname.clone()),
                    });
                }
            }
        }
        if let Some(usable) = pcie_usable(&c.pcie_link) {
            let used = rates.controllers.get(&c.pci_addr).map_or(0.0, |s| s.now());
            let frac = used / usable;
            let what = format!("PCIe link of {cname} ({})", c.pcie_link);
            if self.busy(format!("pcie{}", c.pci_addr), frac, what.clone()) {
                active.push(Issue {
                    severity: Severity::Problem,
                    text: format!("{what} is {:.0}% busy ({} of about {})", frac * 100.0, fmt_rate(used), fmt_rate(usable)),
                    target: Some(format!("pci:{}", c.pci_addr)),
                });
            }
        }
    }

    fn check_streams(&mut self, topo: &Topology, rates: &Rates, now: Instant, active: &mut Vec<Issue>) {
        for d in topo.all_devices().into_iter().filter(|d| d.sdr.is_some()) {
            let Some(s) = rates.device(d) else { continue };
            let h: Vec<f64> = s.history.iter().copied().collect();
            if h.len() < 15 {
                continue;
            }
            let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
            // Compare the last 3 seconds with the 10 seconds before a short gap.
            // Using the highest of the last 3 means one slow second isn't enough.
            let baseline = mean(&h[h.len() - 15..h.len() - 5]);
            let recent = mean(&h[h.len() - 3..]);
            let recent_max = h[h.len() - 3..].iter().copied().fold(0.0, f64::max);
            let last = h[h.len() - 1];
            let prev = self.streams.get(&d.sysname).copied().unwrap_or(Stream::Normal);
            let next = if baseline > STREAMING {
                if recent_max < 0.1 * baseline {
                    match prev {
                        Stream::Stopped(t) => Stream::Stopped(t),
                        _ => Stream::Stopped(now),
                    }
                } else if recent_max < 0.7 * baseline {
                    // Heading for zero: wait a second rather than call it "low" first.
                    if last < 0.1 * baseline { prev } else { Stream::Low }
                } else {
                    Stream::Normal
                }
            } else if last > STREAMING {
                Stream::Normal
            } else {
                prev
            };

            let name = label(d);
            match (prev, next) {
                (Stream::Normal | Stream::Low, Stream::Stopped(_)) => {
                    self.event(Severity::Problem, format!("{name} stopped streaming (was {})", fmt_rate(baseline)));
                }
                (Stream::Normal, Stream::Low) => {
                    self.event(Severity::Warning, format!("{name} data rate dropped from {} to {}", fmt_rate(baseline), fmt_rate(recent)));
                }
                (Stream::Low | Stream::Stopped(_), Stream::Normal) => {
                    self.event(Severity::Info, format!("{name} back to normal at {}", fmt_rate(last)));
                }
                _ => {}
            }
            self.streams.insert(d.sysname.clone(), next);

            match next {
                Stream::Stopped(t) if now.duration_since(t) < STOPPED_ACTIVE_FOR => active.push(Issue {
                    severity: Severity::Problem,
                    text: format!("{name} stopped streaming {}s ago ({})", now.duration_since(t).as_secs(), place(d)),
                    target: Some(d.sysname.clone()),
                }),
                Stream::Low => active.push(Issue {
                    severity: Severity::Warning,
                    text: format!("{name} is running below its usual rate: {} vs {} ({})", fmt_rate(recent), fmt_rate(baseline), place(d)),
                    target: Some(d.sysname.clone()),
                }),
                _ => {}
            }
        }
    }

    fn check_errors(&mut self, topo: &Topology, counters: &traffic::Counters, now: Instant, active: &mut Vec<Issue>) {
        for d in topo.all_devices() {
            let key = (d.busnum as u16, d.devnum as u8);
            let Some(c) = counters.devices.get(&key) else { continue };
            let before = self.prev_errors.insert(key, c.errors).unwrap_or(c.errors);
            let new = c.errors.saturating_sub(before);
            if new > 0 {
                self.last_error.insert(d.sysname.clone(), now);
                let p = self.pending_errors.entry(d.sysname.clone()).or_insert((0, now - ERROR_EVENT_GAP));
                p.0 += new;
                if now.duration_since(p.1) >= ERROR_EVENT_GAP {
                    let n = p.0;
                    *p = (0, now);
                    let worst = c.error_codes.iter().max_by_key(|(_, n)| **n).map(|(code, _)| traffic::error_name(*code)).unwrap_or_default();
                    self.event(Severity::Warning, format!("{n} transfer errors on {} ({}), mostly {worst}", label(d), place(d)));
                }
            }
            if self.last_error.get(&d.sysname).is_some_and(|t| now.duration_since(*t) < RECENT_ERRORS) {
                active.push(Issue {
                    severity: Severity::Warning,
                    text: format!("{} has had transfer errors in the last minute ({})", label(d), place(d)),
                    target: Some(d.sysname.clone()),
                });
            }
        }
    }
}

/// " (bladeRF 2.0)" when a kernel line starts with a device we know, e.g.
/// "usb 2-8.1: reset SuperSpeed USB device".
/// A plain-English note for program messages whose meaning isn't obvious.
fn program_hint(line: &klog::LogLine) -> &'static str {
    if klog::is_usb_memory_error(line) {
        "  [out of USB buffer memory]"
    } else {
        ""
    }
}

fn device_hint(line: &str, topo: &Topology) -> String {
    let Some(rest) = line.strip_prefix("usb ") else { return String::new() };
    let Some((name, _)) = rest.split_once(':') else { return String::new() };
    topo.all_devices().into_iter().find(|d| d.sysname == name).map(|d| format!("  [{}]", label(d))).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::rates::Series;
    use crate::sysfs::{Bus, Controller, ControllerKind};

    fn dev(sysname: &str, devnum: u32, sdr: Option<&'static str>, children: Vec<Device>) -> Device {
        Device {
            sysname: sysname.into(),
            busnum: 2,
            devnum,
            vendor_id: 0x2cf0,
            product_id: 0x5250,
            manufacturer: String::new(),
            product: if sdr.is_some() { "bladeRF 2.0".into() } else { "Hub".into() },
            serial: String::new(),
            speed_mbps: 5000.0,
            usb_version: "3.00".into(),
            max_power: String::new(),
            is_hub: sdr.is_none(),
            drivers: vec![],
            sdr,
            children,
        }
    }

    fn topo(root_children: Vec<Device>) -> Topology {
        let mut root = dev("usb2", 1, None, root_children);
        root.speed_mbps = 20000.0;
        Topology {
            board: String::new(),
            usbfs_memory_mb: None,
            controllers: vec![Controller {
                pci_addr: "0000:00:14.0".into(),
                name: "Intel".into(),
                kind: ControllerKind::Motherboard,
                driver: String::new(),
                pcie_link: String::new(),
                sysfs_path: PathBuf::new(),
                vendor_id: 0x8086,
                subsystem_vendor: 0,
                buses: vec![Bus { busnum: 2, root }],
            }],
        }
    }

    fn texts(w: &Watcher) -> Vec<String> {
        w.events.iter().map(|e| e.text.clone()).collect()
    }

    #[test]
    fn plug_events() {
        let mut w = Watcher::with_log(None);
        let hub = |a, b| dev("2-8", 3, None, vec![dev("2-8.1", a, Some("bladeRF 2.0"), vec![]), dev("2-8.2", b, Some("bladeRF 2.0"), vec![])]);
        w.on_scan(&topo(vec![hub(4, 5)]));
        w.on_scan(&topo(vec![hub(4, 6)]));
        assert_eq!(texts(&w)[0], "Dropped off and reconnected: bladeRF 2.0 on bus 2 port 8.2");
        // The whole hub going away is one event, not three.
        w.on_scan(&topo(vec![]));
        assert_eq!(texts(&w)[0], "Disconnected: Hub from bus 2 port 8");
        assert_eq!(w.events.len(), 3);
        w.on_scan(&topo(vec![hub(7, 8)]));
        assert_eq!(texts(&w)[0], "Connected: Hub on bus 2 port 8");
    }

    #[test]
    fn log_lines_keep_their_time_and_flag_buffer_memory() {
        let mut w = Watcher::with_log(None);
        let mut t = topo(vec![dev("2-8.1", 4, Some("bladeRF 2.0"), vec![]), dev("2-8.2", 5, Some("bladeRF 2.0"), vec![])]);
        t.usbfs_memory_mb = Some(16);
        w.on_sample(&t, &Rates::default(), &traffic::Counters::default(), true);
        assert_eq!(w.active[0].severity, Severity::Warning, "16 MB with two SDRs: {:?}", w.active);

        let line = "2026-10-27T16:37:45-04:00 rdioa sdrplay_apiService[1988964]: libusb: error [submit_iso_transfer] submiturb failed, errno=12";
        let lines: Vec<_> = (0..8).filter_map(|_| klog::parse(line)).collect();
        w.on_kernel_lines(lines, &t);
        // Eight copies of the same message make one history entry, at the logged time.
        assert_eq!(w.events.len(), 2);
        assert_eq!(w.events[0].time, "2026-10-27 16:37:45");
        assert_eq!(w.events[0].severity, Severity::Problem);
        w.on_sample(&t, &Rates::default(), &traffic::Counters::default(), true);
        assert_eq!(w.active[0].severity, Severity::Problem);
        assert!(w.active[0].text.contains("16 MB"), "{}", w.active[0].text);

        let mut w = Watcher::with_log(None);
        t.usbfs_memory_mb = Some(1000);
        w.on_sample(&t, &Rates::default(), &traffic::Counters::default(), true);
        assert!(w.active.is_empty());
    }

    #[test]
    fn stream_stop_and_recover() {
        let mut w = Watcher::with_log(None);
        let t = topo(vec![dev("2-8.2", 5, Some("bladeRF 2.0"), vec![])]);
        let mut rates = Rates::default();
        let feed = |w: &mut Watcher, rates: &mut Rates, v: f64, secs: usize| {
            for _ in 0..secs {
                rates.devices.entry("2-8.2".into()).or_insert_with(Series::default).push(v);
                w.on_sample(&t, rates, &traffic::Counters::default(), true);
            }
        };
        feed(&mut w, &mut rates, 120e6, 20);
        assert!(w.active.is_empty(), "{:?}", w.active);
        feed(&mut w, &mut rates, 0.0, 3);
        assert_eq!(texts(&w)[0], "bladeRF 2.0 stopped streaming (was 120.0 MB/s)");
        assert_eq!(w.problem_count(), 1);
        assert_eq!(w.events.len(), 2, "one event for a sudden stop: {:?}", texts(&w));
        feed(&mut w, &mut rates, 120e6, 20);
        assert!(texts(&w)[0].starts_with("bladeRF 2.0 back to normal"), "{:?}", texts(&w));
        feed(&mut w, &mut rates, 70e6, 4);
        assert!(texts(&w)[0].starts_with("bladeRF 2.0 data rate dropped from 120.0 MB/s"), "{:?}", texts(&w));
    }

    #[test]
    fn pcie_math() {
        // PCIe 2 x1: 5 GT/s * 0.8 / 8 = 500 MB/s raw, 400 MB/s usable.
        assert_eq!(pcie_usable("5.0 GT/s PCIe x1").map(|v| v.round()), Some(400e6));
        assert!(pcie_usable("8.0 GT/s PCIe x4").unwrap() > 3.0e9);
        assert_eq!(pcie_usable(""), None);
    }
}
