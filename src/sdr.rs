//! Recognises common SDR receivers by USB vendor/product ID.

const KNOWN: &[(u16, u16, &str)] = &[
    (0x0bda, 0x2838, "RTL-SDR"),
    (0x0bda, 0x2832, "RTL-SDR"),
    (0x1d50, 0x60a1, "Airspy"),
    (0x03eb, 0x800c, "Airspy HF+"),
    (0x1d50, 0x6089, "HackRF"),
    (0x1d50, 0xcc15, "HackRF (rad1o)"),
    (0x1df7, 0x2500, "SDRplay RSP1"),
    (0x1df7, 0x3000, "SDRplay RSP"),
    (0x1df7, 0x3010, "SDRplay RSP2"),
    (0x1df7, 0x3020, "SDRplay RSPduo"),
    (0x1df7, 0x3030, "SDRplay RSPdx"),
    (0x1df7, 0x3050, "SDRplay RSP1B"),
    (0x1df7, 0x3060, "SDRplay RSPdx-R2"),
    (0x0403, 0x601f, "LimeSDR"),
    (0x1d50, 0x6108, "LimeSDR"),
    (0x2cf0, 0x5246, "bladeRF"),
    (0x2cf0, 0x5250, "bladeRF 2.0"),
    (0x2500, 0x0020, "USRP B200/B210"),
    (0x2500, 0x0021, "USRP B200mini"),
    (0x2500, 0x0022, "USRP B205mini"),
    (0x04d8, 0xfb56, "FUNcube Dongle"),
    (0x04d8, 0xfb31, "FUNcube Dongle Pro+"),
];

/// Returns a friendly SDR type name when the device looks like an SDR.
pub fn identify(vendor: u16, product: u16, product_name: &str) -> Option<&'static str> {
    if let Some((_, _, name)) = KNOWN.iter().find(|(v, p, _)| *v == vendor && *p == product) {
        return Some(name);
    }
    // Many RTL2832U clones use other IDs but keep the chip name in the product string.
    let lower = product_name.to_ascii_lowercase();
    if lower.contains("rtl28") {
        Some("RTL-SDR")
    } else if lower.contains("sdr") {
        Some("SDR")
    } else {
        None
    }
}

/// What an SDR's USB data rate means in samples, e.g. "6.0 MS/s, packed".
#[derive(Clone, Debug, PartialEq)]
pub struct SampleRate {
    /// Complex (I/Q) samples per second the program gets.
    pub msps: f64,
    pub note: &'static str,
}

/// Works out the sample rate an SDR is running at from its USB data rate.
/// Only for receivers whose data format is fixed or easy to tell apart, so the
/// answer is never a guess between two quite different rates.
pub fn sample_rate(kind: &str, bytes_per_sec: f64) -> Option<SampleRate> {
    if bytes_per_sec < 100_000.0 {
        return None;
    }
    match kind {
        // Airspy R2 and Mini send real 12-bit ADC samples at twice the I/Q rate,
        // either in 16-bit words (4 bytes per I/Q sample) or packed (3 bytes).
        // Their rates come from a short list, which tells the two apart.
        "Airspy" => {
            const RATES: &[f64] = &[2.5, 3.0, 6.0, 10.0];
            let mut best: Option<(f64, SampleRate)> = None;
            for &msps in RATES {
                for (bytes, note) in [(3.0, "packed 12-bit"), (4.0, "16-bit")] {
                    let expected = msps * 1e6 * bytes;
                    let off = (bytes_per_sec - expected).abs() / expected;
                    if off < 0.06 && best.as_ref().is_none_or(|(o, _)| off < *o) {
                        best = Some((off, SampleRate { msps, note }));
                    }
                }
            }
            best.map(|(_, r)| r)
        }
        // 8-bit I and Q: 2 bytes per sample.
        "RTL-SDR" | "HackRF" | "HackRF (rad1o)" => Some(SampleRate { msps: bytes_per_sec / 2e6, note: "8-bit" }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn airspy_rates() {
        // What Bill's two Airspys showed on rdio: 9.0 and 17.9 MB/s.
        assert_eq!(sample_rate("Airspy", 9.0e6), Some(SampleRate { msps: 3.0, note: "packed 12-bit" }));
        assert_eq!(sample_rate("Airspy", 17.9e6), Some(SampleRate { msps: 6.0, note: "packed 12-bit" }));
        assert_eq!(sample_rate("Airspy", 24.0e6), Some(SampleRate { msps: 6.0, note: "16-bit" }));
        assert_eq!(sample_rate("Airspy", 40.0e6), Some(SampleRate { msps: 10.0, note: "16-bit" }));
        // Well below any normal rate: no answer rather than a wrong one.
        assert_eq!(sample_rate("Airspy", 14.0e6), None);
        assert_eq!(sample_rate("bladeRF 2.0", 120e6), None);
        assert_eq!(sample_rate("RTL-SDR", 4.8e6).map(|r| r.msps), Some(2.4));
    }
}
