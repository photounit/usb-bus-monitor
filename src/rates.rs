//! Per-second traffic rates with a minute of history, worked out from the
//! traffic monitor's running byte counts.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use crate::sysfs::{Device, Topology};
use crate::traffic::{self, Counters};

pub const HISTORY_SECS: usize = 60;

/// Realistic top speed for bulk data on a bus of the given link speed, in
/// bytes per second. Protocol overhead means nobody gets the full link rate.
pub fn usable_bytes_per_sec(link_mbps: f64) -> f64 {
    const MB: f64 = 1_000_000.0;
    match link_mbps as u64 {
        0..=2 => 0.15 * MB,
        3..=12 => 1.0 * MB,
        13..=480 => 40.0 * MB,
        481..=5000 => 400.0 * MB,
        5001..=10000 => 900.0 * MB,
        _ => 1800.0 * MB,
    }
}

pub fn fmt_rate(bps: f64) -> String {
    if bps >= 1e6 {
        format!("{:.1} MB/s", bps / 1e6)
    } else if bps >= 1e3 {
        format!("{:.0} kB/s", bps / 1e3)
    } else if bps > 0.0 {
        format!("{bps:.0} B/s")
    } else {
        "idle".into()
    }
}

/// One rate with its last minute of history.
#[derive(Default)]
pub struct Series {
    pub history: VecDeque<f64>,
}

impl Series {
    pub fn push(&mut self, v: f64) {
        if self.history.len() == HISTORY_SECS {
            self.history.pop_front();
        }
        self.history.push_back(v);
    }

    pub fn now(&self) -> f64 {
        self.history.back().copied().unwrap_or(0.0)
    }

    pub fn peak(&self) -> f64 {
        self.history.iter().copied().fold(0.0, f64::max)
    }
}

/// Turns the monitor's running byte counts into per-second rates.
#[derive(Default)]
pub struct Rates {
    prev: Option<(Instant, Counters)>,
    pub latest: Counters,
    pub devices: HashMap<String, Series>,
    pub buses: HashMap<u32, Series>,
    pub controllers: HashMap<String, Series>,
}

impl Rates {
    pub fn sample(&mut self, topo: &Topology, counters: Counters) {
        let now = Instant::now();
        let Some((t0, prev)) = self.prev.take() else {
            self.prev = Some((now, counters));
            return;
        };
        let dt = now.duration_since(t0).as_secs_f64().max(0.001);
        let rate = |bus: u16, dev: u8| {
            let cur = counters.devices.get(&(bus, dev)).map_or(0, |c| c.bytes);
            let old = prev.devices.get(&(bus, dev)).map_or(0, |c| c.bytes);
            cur.saturating_sub(old) as f64 / dt
        };

        let mut bus_totals: HashMap<u32, f64> = HashMap::new();
        for &(bus, dev) in counters.devices.keys() {
            *bus_totals.entry(bus as u32).or_default() += rate(bus, dev);
        }
        for d in topo.all_devices() {
            if d.sysname.starts_with("usb") {
                continue;
            }
            self.devices.entry(d.sysname.clone()).or_default().push(rate(d.busnum as u16, d.devnum as u8));
        }
        for c in &topo.controllers {
            let mut total = 0.0;
            for b in &c.buses {
                let r = bus_totals.get(&b.busnum).copied().unwrap_or(0.0);
                total += r;
                self.buses.entry(b.busnum).or_default().push(r);
            }
            self.controllers.entry(c.pci_addr.clone()).or_default().push(total);
        }
        self.latest = counters.clone();
        self.prev = Some((now, counters));
    }

    pub fn device(&self, d: &Device) -> Option<&Series> {
        self.devices.get(&d.sysname)
    }

    pub fn errors(&self, d: &Device) -> Option<&traffic::DevCounters> {
        self.latest.devices.get(&(d.busnum as u16, d.devnum as u8)).filter(|c| c.errors > 0)
    }
}

