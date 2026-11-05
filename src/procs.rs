//! Finds which program has each USB device open (through /dev/bus/usb) and
//! how hard its busiest thread is working.
//!
//! SDR programs read samples on one or a few threads. If one of those threads
//! needs a whole CPU core, it falls behind and the SDR library throws samples
//! away inside the computer, even though USB delivered every one of them. The
//! PC's overall CPU figure hides this: one maxed-out thread on a 36-thread CPU
//! is under 3%.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

/// A program that has a USB device open.
#[derive(Clone, Debug, PartialEq)]
pub struct User {
    pub pid: u32,
    pub name: String,
    /// The busiest thread's name and its share of one CPU core (100 = a full core).
    pub busiest_thread: String,
    pub busiest_pct: f64,
    /// The whole program, in cores x 100.
    pub total_pct: f64,
}

/// Thread id -> (thread name, CPU ticks used so far).
type ThreadTicks = HashMap<u32, (String, u64)>;

#[derive(Clone, Default)]
pub struct Tracker {
    /// (bus, device number) -> pid of a program that has it open.
    owners: HashMap<(u32, u32), u32>,
    /// pid -> (when, per-thread CPU ticks) from the last sample.
    prev: HashMap<u32, (Instant, ThreadTicks)>,
    /// Latest result per (bus, device number).
    pub users: HashMap<(u32, u32), User>,
}

fn clock_ticks() -> f64 {
    let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if t > 0 { t as f64 } else { 100.0 }
}

/// "/dev/bus/usb/009/002" -> (9, 2).
pub fn parse_usb_path(target: &str) -> Option<(u32, u32)> {
    let rest = target.strip_prefix("/dev/bus/usb/")?;
    let (bus, dev) = rest.split_once('/')?;
    Some((bus.parse().ok()?, dev.parse().ok()?))
}

/// Thread name and CPU ticks used (user + system) from a `/proc/.../stat` line.
pub fn parse_stat(stat: &str) -> Option<(String, u64)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_string();
    let fields: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    // After the name: state is field 0, utime is 11, stime is 12.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((name, utime + stime))
}

fn threads(proc_root: &Path, pid: u32) -> ThreadTicks {
    let mut out = HashMap::new();
    let Ok(dir) = fs::read_dir(proc_root.join(pid.to_string()).join("task")) else { return out };
    for t in dir.flatten() {
        let Some(tid) = t.file_name().to_str().and_then(|s| s.parse().ok()) else { continue };
        if let Some(s) = fs::read_to_string(t.path().join("stat")).ok().and_then(|s| parse_stat(&s)) {
            out.insert(tid, s);
        }
    }
    out
}

impl Tracker {
    /// Looks through every process's open files for USB devices. Only
    /// processes of the same user (or all, as root) can be seen.
    pub fn find_owners(&mut self, proc_root: &Path) {
        let mut owners = HashMap::new();
        if let Ok(dir) = fs::read_dir(proc_root) {
            for p in dir.flatten() {
                let Some(pid) = p.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
                let Ok(fds) = fs::read_dir(p.path().join("fd")) else { continue };
                for fd in fds.flatten() {
                    if let Some(key) = fs::read_link(fd.path()).ok().and_then(|t| parse_usb_path(&t.to_string_lossy())) {
                        owners.insert(key, pid);
                    }
                }
            }
        }
        self.owners = owners;
    }

    /// Updates CPU use of the programs found by `find_owners`. Call about once a second.
    pub fn sample(&mut self, proc_root: &Path) {
        let ticks = clock_ticks();
        let now = Instant::now();
        let mut pids: Vec<u32> = self.owners.values().copied().collect();
        pids.sort();
        pids.dedup();
        let mut per_pid: HashMap<u32, User> = HashMap::new();
        let mut prev = HashMap::new();
        for pid in pids {
            let cur = threads(proc_root, pid);
            if cur.is_empty() {
                continue;
            }
            if let Some((t0, old)) = self.prev.get(&pid) {
                let dt = now.duration_since(*t0).as_secs_f64().max(0.1);
                let mut busiest = (String::new(), 0.0);
                let mut total = 0.0;
                for (tid, (name, used)) in &cur {
                    let before = old.get(tid).map_or(*used, |(_, u)| *u);
                    let pct = used.saturating_sub(before) as f64 / ticks / dt * 100.0;
                    total += pct;
                    if pct > busiest.1 {
                        busiest = (name.clone(), pct);
                    }
                }
                let name = fs::read_to_string(proc_root.join(pid.to_string()).join("comm")).map(|s| s.trim().to_string()).unwrap_or_default();
                per_pid.insert(pid, User { pid, name, busiest_thread: busiest.0, busiest_pct: busiest.1, total_pct: total });
            }
            prev.insert(pid, (now, cur));
        }
        self.prev = prev;
        self.users = self.owners.iter().filter_map(|(k, pid)| Some((*k, per_pid.get(pid)?.clone()))).collect();
    }

    pub fn user(&self, busnum: u32, devnum: u32) -> Option<&User> {
        self.users.get(&(busnum, devnum))
    }

    /// Whether some program had the device open at the last look, even if its
    /// CPU numbers aren't in yet.
    pub fn has_owner(&self, busnum: u32, devnum: u32) -> bool {
        self.owners.contains_key(&(busnum, devnum))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_paths_and_stat_lines() {
        assert_eq!(parse_usb_path("/dev/bus/usb/009/002"), Some((9, 2)));
        assert_eq!(parse_usb_path("/dev/null"), None);
        // Thread names can contain spaces and brackets.
        let stat = "4242 (airspy rx) thread) S 1 4242 4242 0 -1 4194560 100 0 0 0 750 250 0 0 20 0 9 0 1234 0 0";
        assert_eq!(parse_stat(stat), Some(("airspy rx) thread".into(), 1000)));
    }

    #[test]
    fn finds_owner_and_busiest_thread() {
        let dir = std::env::temp_dir().join(format!("usbmon-procs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let p = dir.join("4242");
        fs::create_dir_all(p.join("fd")).unwrap();
        std::os::unix::fs::symlink("/dev/bus/usb/009/002", p.join("fd/7")).unwrap();
        std::os::unix::fs::symlink("/dev/null", p.join("fd/0")).unwrap();
        fs::write(p.join("comm"), "trunk-recorder\n").unwrap();
        let task = |tid: u32, name: &str, ticks: u64| {
            let t = p.join("task").join(tid.to_string());
            fs::create_dir_all(&t).unwrap();
            fs::write(t.join("stat"), format!("{tid} ({name}) S 1 1 1 0 -1 0 0 0 0 0 {ticks} 0 0 0 20 0 1 0 1 0 0")).unwrap();
        };
        task(4242, "trunk-recorder", 0);
        task(4243, "airspy", 0);

        let mut t = Tracker::default();
        t.find_owners(&dir);
        assert!(t.has_owner(9, 2));
        t.sample(&dir);
        assert!(t.user(9, 2).is_none(), "needs two samples");
        // The airspy thread uses a whole core's worth of ticks.
        let ticks = clock_ticks() as u64;
        task(4243, "airspy", ticks * 5);
        task(4242, "trunk-recorder", ticks / 10);
        t.prev.get_mut(&4242).unwrap().0 -= std::time::Duration::from_secs(5);
        t.sample(&dir);
        let u = t.user(9, 2).unwrap();
        assert_eq!(u.name, "trunk-recorder");
        assert_eq!(u.busiest_thread, "airspy");
        assert!((95.0..105.0).contains(&u.busiest_pct), "{}", u.busiest_pct);
        let _ = fs::remove_dir_all(&dir);
    }
}
