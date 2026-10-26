//! Live USB traffic from the kernel's usbmon binary interface (`/dev/usbmon0`,
//! which sees every bus). A background thread reads one event per completed
//! transfer and adds its byte count to per-device counters; the GUI samples
//! those counters once a second to get rates.
//!
//! Layout and ioctls follow drivers/usb/mon/mon_bin.c in the kernel.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const DEVICE: &str = "/dev/usbmon0";

// _IOW(0x92, 10, struct mon_bin_get), _IOR(0x92, 3, struct mon_bin_stats), _IO(0x92, 4)
const MON_IOCX_GETX: libc::c_ulong = 0x4018_920a;
const MON_IOCG_STATS: libc::c_ulong = 0x8008_9203;
const MON_IOCT_RING_SIZE: libc::c_ulong = 0x9204;

const HDR_LEN: usize = 64;

#[repr(C)]
struct MonBinGet {
    hdr: *mut u8,
    data: *mut u8,
    alloc: libc::size_t,
}

#[repr(C)]
#[derive(Default)]
struct MonBinStats {
    queued: u32,
    dropped: u32,
}

/// Running totals for one device, identified by (bus number, device address).
#[derive(Debug, Clone, Default)]
pub struct DevCounters {
    pub bytes: u64,
    pub errors: u64,
    /// Count per kernel error code (negative errno), for the details panel.
    pub error_codes: HashMap<i32, u64>,
}

#[derive(Debug, Clone, Default)]
pub struct Counters {
    pub devices: HashMap<(u16, u8), DevCounters>,
    /// Events the kernel had to throw away because we read too slowly.
    pub lost_events: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Starting,
    Running,
    /// usbmon module not loaded.
    NotLoaded,
    NoPermission,
    Failed(String),
}

#[derive(Clone)]
pub struct Monitor {
    pub counters: Arc<Mutex<Counters>>,
    pub state: Arc<Mutex<State>>,
}

impl Monitor {
    /// Starts the reader thread. If usbmon isn't available yet it keeps
    /// retrying, so finishing the setup while the window is open just works.
    pub fn start() -> Self {
        let m = Monitor { counters: Arc::default(), state: Arc::new(Mutex::new(State::Starting)) };
        let bg = m.clone();
        thread::spawn(move || loop {
            match File::open(DEVICE) {
                Ok(f) => {
                    *bg.state.lock().unwrap() = State::Running;
                    let err = bg.read_loop(&f);
                    *bg.state.lock().unwrap() = State::Failed(format!("Reading {DEVICE} stopped: {err}"));
                }
                Err(e) => {
                    *bg.state.lock().unwrap() = match e.kind() {
                        io::ErrorKind::NotFound => State::NotLoaded,
                        io::ErrorKind::PermissionDenied => State::NoPermission,
                        _ => State::Failed(format!("Could not open {DEVICE}: {e}")),
                    };
                }
            }
            thread::sleep(Duration::from_secs(2));
        });
        m
    }

    /// Feeds made-up traffic for the given (bus, device, bytes per second)
    /// list. Used for screenshots on machines without real USB hardware.
    pub fn demo(devices: Vec<(u16, u8, u64)>) -> Self {
        let m = Monitor { counters: Arc::default(), state: Arc::new(Mutex::new(State::Running)) };
        let bg = m.clone();
        thread::spawn(move || {
            let start = Instant::now();
            loop {
                thread::sleep(Duration::from_millis(100));
                let wobble = 1.0 + 0.15 * (start.elapsed().as_secs_f64() * 0.7).sin();
                let mut c = bg.counters.lock().unwrap();
                // The first device stalls from 30 to 45 s in, to show off the warnings.
                let t = start.elapsed().as_secs();
                for (i, &(bus, dev, rate)) in devices.iter().enumerate() {
                    let rate = if i == 0 && (30..45).contains(&t) { 0 } else { rate };
                    let mut hdr = [0u8; HDR_LEN];
                    hdr[8] = b'C';
                    hdr[9] = 3;
                    hdr[11] = dev;
                    hdr[12..14].copy_from_slice(&bus.to_le_bytes());
                    hdr[32..36].copy_from_slice(&((rate as f64 * wobble / 10.0) as u32).to_le_bytes());
                    record(&mut c, &hdr);
                }
            }
        });
        m
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    pub fn snapshot(&self) -> Counters {
        self.counters.lock().unwrap().clone()
    }

    fn read_loop(&self, f: &File) -> io::Error {
        let fd = f.as_raw_fd();
        // A bigger kernel buffer means fewer lost events at high data rates.
        // Older kernels cap it at 1.2 MB, newer ones at 64 MB.
        for size in [32 << 20, 8 << 20, 1_228_800] {
            if unsafe { libc::ioctl(fd, MON_IOCT_RING_SIZE, size as libc::c_ulong) } == 0 {
                break;
            }
        }

        let mut hdr = [0u8; HDR_LEN];
        let mut last_stats = Instant::now();
        loop {
            // We only need the header, so ask for zero bytes of payload.
            let mut get = MonBinGet { hdr: hdr.as_mut_ptr(), data: std::ptr::null_mut(), alloc: 0 };
            if unsafe { libc::ioctl(fd, MON_IOCX_GETX, &mut get) } != 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return e;
            }
            let mut c = self.counters.lock().unwrap();
            record(&mut c, &hdr);
            if last_stats.elapsed() >= Duration::from_secs(1) {
                let mut st = MonBinStats::default();
                if unsafe { libc::ioctl(fd, MON_IOCG_STATS, &mut st) } == 0 {
                    c.lost_events += st.dropped as u64;
                }
                last_stats = Instant::now();
            }
        }
    }
}

const ENOENT: i32 = -2;
const EPIPE: i32 = -32;
const ECONNRESET: i32 = -104;
const ESHUTDOWN: i32 = -108;
const XFER_CONTROL: u8 = 2;

/// Adds one usbmon event header to the counters.
pub fn record(c: &mut Counters, hdr: &[u8; HDR_LEN]) {
    let kind = hdr[8];
    let xfer_type = hdr[9];
    let devnum = hdr[11];
    let busnum = u16::from_le_bytes([hdr[12], hdr[13]]);
    let status = i32::from_le_bytes(hdr[28..32].try_into().unwrap());
    let len_urb = u32::from_le_bytes(hdr[32..36].try_into().unwrap());
    let iso_errors = i32::from_le_bytes(hdr[40..44].try_into().unwrap());

    let dev = c.devices.entry((busnum, devnum)).or_default();
    match kind {
        // Completion: len_urb is the number of bytes actually moved.
        b'C' => {
            dev.bytes += len_urb as u64;
            // Cancellations are normal (programs stopping a stream), and devices
            // routinely refuse ("stall") control requests they don't support.
            let benign = matches!(status, 0 | ENOENT | ECONNRESET | ESHUTDOWN) || (status == EPIPE && xfer_type == XFER_CONTROL);
            if !benign {
                dev.errors += 1;
                *dev.error_codes.entry(status).or_default() += 1;
            }
            // Isochronous transfers report per-packet errors separately.
            if xfer_type == 0 && iso_errors > 0 {
                dev.errors += iso_errors as u64;
                *dev.error_codes.entry(-1000).or_default() += iso_errors as u64;
            }
        }
        // Submission failed outright.
        b'E' => {
            dev.errors += 1;
            *dev.error_codes.entry(status).or_default() += 1;
        }
        _ => {}
    }
}

/// Plain-English name for an error code from `DevCounters::error_codes`.
pub fn error_name(code: i32) -> String {
    match code {
        -1000 => "isochronous packet errors".into(),
        -71 => "protocol error (EPROTO), often a bad cable or power".into(),
        -75 => "overflow / babble (EOVERFLOW)".into(),
        -32 => "endpoint stalled (EPIPE)".into(),
        -19 => "device gone (ENODEV)".into(),
        -62 => "timeout (ETIME)".into(),
        -110 => "timeout (ETIMEDOUT)".into(),
        -84 => "illegal byte sequence / CRC (EILSEQ)".into(),
        -121 => "short read (EREMOTEIO)".into(),
        -18 => "transfer incomplete (EXDEV)".into(),
        -12 => "out of memory (ENOMEM)".into(),
        c => format!("error {c}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(kind: u8, xfer: u8, bus: u16, dev: u8, status: i32, len: u32) -> [u8; HDR_LEN] {
        let mut h = [0u8; HDR_LEN];
        h[8] = kind;
        h[9] = xfer;
        h[11] = dev;
        h[12..14].copy_from_slice(&bus.to_le_bytes());
        h[28..32].copy_from_slice(&status.to_le_bytes());
        h[32..36].copy_from_slice(&len.to_le_bytes());
        h
    }

    #[test]
    fn counts_completions_and_errors() {
        let mut c = Counters::default();
        record(&mut c, &hdr(b'S', 3, 1, 4, -115, 262144)); // submission: ignored
        record(&mut c, &hdr(b'C', 3, 1, 4, 0, 262144));
        record(&mut c, &hdr(b'C', 3, 1, 4, 0, 1000));
        record(&mut c, &hdr(b'C', 3, 1, 4, -2, 0)); // cancelled: not an error
        record(&mut c, &hdr(b'C', 2, 1, 4, -32, 0)); // control stall: not an error
        record(&mut c, &hdr(b'C', 3, 1, 4, -71, 0)); // protocol error
        record(&mut c, &hdr(b'E', 3, 2, 7, -19, 0));
        let d = &c.devices[&(1, 4)];
        assert_eq!(d.bytes, 263144);
        assert_eq!(d.errors, 1);
        assert_eq!(d.error_codes[&-71], 1);
        assert_eq!(c.devices[&(2, 7)].errors, 1);
    }

    #[test]
    fn ioctl_numbers_match_kernel_macros() {
        const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
            (dir << 30) | (size << 16) | (0x92 << 8) | nr
        }
        assert_eq!(MON_IOCX_GETX as u64, ioc(1, 10, std::mem::size_of::<MonBinGet>() as u64));
        assert_eq!(MON_IOCG_STATS as u64, ioc(2, 3, std::mem::size_of::<MonBinStats>() as u64));
        assert_eq!(MON_IOCT_RING_SIZE as u64, ioc(0, 4, 0));
    }
}
