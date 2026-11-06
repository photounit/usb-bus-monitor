//! How much free memory the kernel has in large unbroken blocks.
//!
//! USB transfers need physically contiguous buffers. After a long uptime with
//! little RAM to spare, free memory breaks up into small pieces, and SDR
//! software (the SDRplay service in particular) starts failing with libusb
//! errno 12 and can crash, even though plenty of memory is free in total.
//! /proc/buddyinfo shows this before it happens.

use std::path::Path;

/// Blocks this big or bigger count as "large": 2^7 pages, 512 KB with 4 KB pages.
const LARGE_ORDER: usize = 7;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemInfo {
    /// Free memory in blocks of 512 KB or more, in bytes.
    pub large_free: u64,
    /// MemAvailable from /proc/meminfo, in bytes.
    pub available: u64,
    /// Swap in use, in bytes.
    pub swap_used: u64,
}

/// Reads /proc (or a copy of it under `proc_root`, for testing).
pub fn read(proc_root: &Path) -> Option<MemInfo> {
    let buddy = std::fs::read_to_string(proc_root.join("buddyinfo")).ok()?;
    let meminfo = std::fs::read_to_string(proc_root.join("meminfo")).ok()?;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let (available, swap_used) = parse_meminfo(&meminfo);
    Some(MemInfo { large_free: large_free_bytes(&buddy, page), available, swap_used })
}

/// Sums free blocks of LARGE_ORDER and up in the zones normal allocations use.
/// Each line is "Node 0, zone   Normal  <count of order 0> <order 1> ...".
pub fn large_free_bytes(buddyinfo: &str, page: u64) -> u64 {
    let mut total = 0;
    for line in buddyinfo.lines() {
        let Some((_, rest)) = line.split_once("zone") else { continue };
        let mut fields = rest.split_whitespace();
        // The DMA zone is the first 16 MB, kept for old devices.
        if matches!(fields.next(), Some("DMA") | None) {
            continue;
        }
        for (order, count) in fields.enumerate().skip(LARGE_ORDER) {
            total += count.parse::<u64>().unwrap_or(0) * (page << order);
        }
    }
    total
}

fn parse_meminfo(text: &str) -> (u64, u64) {
    let kb = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    (kb("MemAvailable:"), kb("SwapTotal:").saturating_sub(kb("SwapFree:")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rdioa_before_and_after_reboot() {
        // rdioa on 2026-11-06, minutes before the SDRplay service crashed...
        let before = "Node 0, zone      DMA      1      1      1      1      1      1      1      1      0      2      2 \n\
                      Node 0, zone    DMA32    674    550    410    284    156     97     52      1      1      0      0 \n\
                      Node 0, zone   Normal  89461 135487  48760  13713   2952    162     10      0      0      0      0 \n";
        assert_eq!(large_free_bytes(before, 4096), 512 * 1024 + 1024 * 1024);
        // ...and right after a reboot.
        let after = "Node 0, zone      DMA      1      1      1      1      1      1      1      1      0      2      2 \n\
                     Node 0, zone    DMA32      6     10      9      8      7      8      8      9      9      6     86 \n\
                     Node 0, zone   Normal   1355   1364   3901   2904   1449    242     52    507    227     16    949 \n";
        assert!(large_free_bytes(after, 4096) > 4_000_000_000);

        let (avail, swap) = parse_meminfo("MemTotal: 16000000 kB\nMemAvailable:    5600000 kB\nSwapTotal:  4194300 kB\nSwapFree:   1153400 kB\n");
        assert_eq!(avail, 5_600_000 * 1024);
        assert_eq!(swap, (4_194_300 - 1_153_400) * 1024);
    }
}
