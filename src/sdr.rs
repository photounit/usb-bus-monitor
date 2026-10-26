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
