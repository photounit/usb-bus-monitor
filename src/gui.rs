//! The window: controller/bus/device tree on the left, details on the right.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Stroke};

use crate::ids::IdDb;
use crate::issues::{Severity, Watcher};
use crate::klog::KernelLog;
use crate::names::Names;
use crate::sysfs::{self, Controller, ControllerKind, Device, Topology};
use crate::rates::{fmt_rate, usable_bytes_per_sec, Rates, Series, HISTORY_SECS};
use crate::traffic::{self, Monitor, State};

const RESCAN_EVERY: Duration = Duration::from_secs(2);
const SAMPLE_EVERY: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_millis(200);
const SETUP_SCRIPT: &str = include_str!("../setup-usbmon.sh");

const GREEN: Color32 = Color32::from_rgb(60, 170, 90);
const AMBER: Color32 = Color32::from_rgb(220, 150, 30);
const RED: Color32 = Color32::from_rgb(220, 70, 60);
const BLUE: Color32 = Color32::from_rgb(70, 130, 220);
const PURPLE: Color32 = Color32::from_rgb(150, 100, 210);

pub fn run(sys_root: PathBuf, ids: IdDb, demo: bool) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 760.0]).with_title("USB Bus Monitor").with_app_id(crate::install::APP_ID),
        ..Default::default()
    };
    eframe::run_native("USB Bus Monitor", options, Box::new(move |cc| Ok(Box::new(App::new(sys_root, ids, demo, cc.egui_ctx.clone())))))
}

/// Scanning, sampling and issue checks. These run on their own thread, so
/// monitoring and the history carry on while the window is minimized or the
/// screen is locked and nothing is being drawn.
struct Engine {
    sys_root: PathBuf,
    ids: IdDb,
    topo: Topology,
    error: Option<String>,
    last_scan: Instant,
    last_sample: Instant,
    monitor: Monitor,
    rates: Rates,
    watcher: Watcher,
    klog: KernelLog,
    /// Bumped on every change, so the window only copies new data.
    generation: u64,
}

impl Engine {
    fn rescan(&mut self) {
        match sysfs::scan(&self.sys_root, &self.ids) {
            Ok(t) => {
                self.watcher.on_scan(&t);
                self.topo = t;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("Could not read {}: {e}", self.sys_root.join("bus/usb/devices").display())),
        }
        self.last_scan = Instant::now();
        self.generation += 1;
    }

    fn tick(&mut self) {
        if self.last_scan.elapsed() >= RESCAN_EVERY {
            self.rescan();
        }
        if self.last_sample.elapsed() >= SAMPLE_EVERY {
            let live = self.monitor.state() == State::Running;
            self.rates.sample(&self.topo, self.monitor.snapshot());
            self.watcher.on_kernel_lines(self.klog.drain(), &self.topo);
            self.watcher.on_sample(&self.topo, &self.rates, &self.rates.latest, live);
            self.last_sample = Instant::now();
            self.generation += 1;
        }
    }
}

struct App {
    engine: Arc<Mutex<Engine>>,
    seen_generation: u64,
    // Copies of the engine's latest results, for drawing.
    topo: Topology,
    error: Option<String>,
    rates: Rates,
    watcher: Watcher,
    monitor: Monitor,
    klog: KernelLog,
    save_error: Option<String>,
    names: Names,
    /// sysfs name of the selected device, or `pci:<addr>` for a controller.
    selected: Option<String>,
    rename_text: String,
    setup_result: Arc<Mutex<Option<String>>>,
}

impl App {
    fn new(sys_root: PathBuf, ids: IdDb, demo: bool, ctx: egui::Context) -> Self {
        let topo = sysfs::scan(&sys_root, &ids).unwrap_or_default();
        let monitor = if demo { demo_monitor(&topo) } else { Monitor::start() };
        let klog = if demo { KernelLog::default() } else { KernelLog::start() };
        let mut engine = Engine {
            sys_root,
            ids,
            topo,
            error: None,
            last_scan: Instant::now(),
            last_sample: Instant::now(),
            monitor: monitor.clone(),
            rates: Rates::default(),
            watcher: Watcher::new(),
            klog: klog.clone(),
            generation: 0,
        };
        engine.rescan();
        let engine = Arc::new(Mutex::new(engine));
        let bg = engine.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(TICK);
            let generation = {
                let mut e = bg.lock().unwrap();
                let before = e.generation;
                e.tick();
                (e.generation != before).then_some(e.generation)
            };
            if generation.is_some() {
                ctx.request_repaint();
            }
        });
        let mut app = App {
            engine,
            seen_generation: u64::MAX,
            topo: Topology::default(),
            error: None,
            rates: Rates::default(),
            watcher: Watcher::with_log(None),
            monitor,
            klog,
            save_error: None,
            names: Names::load(),
            selected: None,
            rename_text: String::new(),
            setup_result: Arc::default(),
        };
        app.sync();
        app
    }

    /// Copies the engine's results if they changed since the last frame.
    fn sync(&mut self) {
        let e = self.engine.lock().unwrap();
        if e.generation != self.seen_generation {
            self.topo = e.topo.clone();
            self.error = e.error.clone();
            self.rates = e.rates.clone();
            self.watcher = e.watcher.clone();
            self.seen_generation = e.generation;
        }
    }

    fn rescan(&mut self) {
        self.engine.lock().unwrap().rescan();
        self.sync();
    }

    fn controller_title(&self, c: &Controller) -> String {
        match self.names.get(&c.pci_addr) {
            Some(n) => format!("{n}  ·  {}", c.name),
            None => format!("{}  ·  {}", c.kind.label(), c.name),
        }
    }

    /// Runs the setup script with a graphical password prompt.
    fn run_setup(&self) {
        let result = self.setup_result.clone();
        *result.lock().unwrap() = Some("Waiting for your password…".into());
        std::thread::spawn(move || {
            let out = std::process::Command::new("pkexec").args(["sh", "-c", SETUP_SCRIPT]).output();
            *result.lock().unwrap() = Some(match out {
                Ok(o) if o.status.success() => "Done. Live traffic will appear in a few seconds.".into(),
                Ok(o) => format!("Setup didn't finish: {}", String::from_utf8_lossy(&o.stderr).trim()),
                Err(e) => format!("Couldn't start pkexec ({e}). Run setup-usbmon.sh from a terminal instead."),
            });
        });
    }
}

/// Plausible SDR-ish rates for every device, for screenshots.
fn demo_monitor(topo: &Topology) -> Monitor {
    let mut all = topo.all_devices();
    all.sort_by_key(|d| d.sdr.is_none());
    let devs = all
        .iter()
        .filter(|d| !d.sysname.starts_with("usb"))
        .map(|d| {
            let rate = match d.sdr {
                Some("RTL-SDR") => 4_800_000,
                Some("Airspy") => 20_000_000,
                Some(_) => 40_000_000,
                None => 2_000,
            };
            (d.busnum as u16, d.devnum as u8, rate)
        })
        .collect();
    Monitor::demo(devs)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.sync();
        let state = self.monitor.state();
        // The engine asks for a repaint when it has new data; this is a backstop.
        ui.ctx().request_repaint_after(SAMPLE_EVERY);

        egui::Panel::top("summary").show(ui, |ui| {
            ui.horizontal(|ui| {
                let devices = self.topo.all_devices().iter().filter(|d| !d.sysname.starts_with("usb")).count();
                ui.heading("USB Bus Monitor");
                ui.separator();
                if !self.topo.board.is_empty() {
                    ui.label(&self.topo.board);
                    ui.separator();
                }
                ui.label(format!("{} controllers", self.topo.controllers.len()));
                ui.label(format!("{devices} devices"));
                ui.label(RichText::new(format!("{} SDRs", self.topo.sdr_count())).color(PURPLE).strong());
                ui.separator();
                match &state {
                    State::Running => {
                        let total: f64 = self.rates.controllers.values().map(Series::now).sum();
                        ui.label(RichText::new(format!("Live traffic: {}", fmt_rate(total))).color(GREEN));
                    }
                    State::Starting => {
                        ui.weak("Starting traffic monitor…");
                    }
                    _ => {
                        ui.label(RichText::new("Live traffic off").color(AMBER));
                    }
                }
                ui.separator();
                let problems = self.watcher.problem_count();
                let warnings = self.watcher.active.len() - problems;
                if problems > 0 {
                    ui.label(RichText::new(format!("⚠ {problems} problem{}", if problems == 1 { "" } else { "s" })).color(RED).strong());
                }
                if warnings > 0 {
                    ui.label(RichText::new(format!("⚠ {warnings} warning{}", if warnings == 1 { "" } else { "s" })).color(AMBER));
                }
                if problems + warnings == 0 {
                    ui.label(RichText::new("✓ No issues").color(GREEN));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Rescan").clicked() {
                        self.rescan();
                    }
                });
            });
            if !matches!(state, State::Running | State::Starting) {
                self.setup_banner(ui, &state);
            }
            if self.rates.latest.lost_events > 0 {
                ui.colored_label(AMBER, format!("⚠ The kernel dropped {} traffic events because the system was busy, so rates may read a little low.", self.rates.latest.lost_events));
            }
        });

        egui::Panel::bottom("issues").resizable(true).default_size(210.0).show(ui, |ui| self.issues_panel(ui));

        egui::Panel::right("details").default_size(380.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.details_panel(ui));
        });

        egui::CentralPanel::default().show(ui, |ui| {
            for e in [&self.error, &self.save_error].into_iter().flatten() {
                ui.colored_label(RED, e);
            }
            let live = state == State::Running;
            egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                for c in self.topo.controllers.clone() {
                    self.controller_tree(ui, &c, live);
                }
            });
        });
    }
}

impl App {
    fn issues_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.columns(2, |cols| {
            cols[0].label(RichText::new("Right now").strong());
            egui::ScrollArea::vertical().id_salt("active").auto_shrink([false; 2]).show(&mut cols[0], |ui| {
                if self.watcher.active.is_empty() {
                    ui.colored_label(GREEN, "✓ No issues right now.");
                }
                for i in &self.watcher.active {
                    ui.colored_label(severity_color(i.severity), format!("⚠ {}", i.text));
                }
                if !*self.klog.running.lock().unwrap() && self.monitor.state() == State::Running {
                    ui.add_space(6.0);
                    ui.weak("System log messages aren't being read (journalctl isn't available to this account), so USB resets, errors and SDR software crashes won't appear in the history.");
                }
            });

            cols[1].horizontal(|ui| {
                ui.label(RichText::new("History").strong());
                if let Some(p) = self.watcher.log_path.clone() {
                    if ui.small_button("Open log folder").clicked() {
                        if let Some(dir) = p.parent() {
                            let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
                        }
                    }
                    ui.weak(p.display().to_string());
                }
            });
            egui::ScrollArea::vertical().id_salt("history").auto_shrink([false; 2]).show(&mut cols[1], |ui| {
                for e in &self.watcher.events {
                    ui.horizontal_wrapped(|ui| {
                        ui.weak(&e.time[11..]);
                        ui.colored_label(severity_color(e.severity), &e.text);
                    });
                }
            });
        });
    }

    fn setup_banner(&mut self, ui: &mut egui::Ui, state: &State) {
        let why = match state {
            State::NotLoaded => "Linux's USB traffic monitor (usbmon) isn't switched on yet.".to_string(),
            State::NoPermission => "usbmon is on, but this account isn't allowed to read it yet.".to_string(),
            State::Failed(e) => e.clone(),
            _ => String::new(),
        };
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(AMBER, format!("⚠ {why} A one-time setup fixes this."));
            if ui.button("Set up live traffic…").clicked() {
                self.run_setup();
            }
        });
        if let Some(msg) = self.setup_result.lock().unwrap().as_ref() {
            ui.weak(msg);
        }
    }

    fn controller_tree(&mut self, ui: &mut egui::Ui, c: &Controller, live: bool) {
        let sdrs = c.sdrs().len();
        let mut title = self.controller_title(c);
        if sdrs > 0 {
            title += &format!("  ·  {} SDR{}", sdrs, if sdrs == 1 { "" } else { "s" });
        }
        if live {
            let r = self.rates.controllers.get(&c.pci_addr).map_or(0.0, Series::now);
            title += &format!("  ·  {}", fmt_rate(r));
        }
        let id = format!("pci:{}", c.pci_addr);
        let color = match self.watcher.issue_for(&id) {
            Some(i) => severity_color(i.severity),
            None => kind_color(&c.kind),
        };
        if self.watcher.issue_for(&id).is_some() {
            title = format!("⚠ {title}");
        }
        egui::CollapsingHeader::new(RichText::new(title).strong().color(color)).id_salt(&id).default_open(true).show(ui, |ui| {
            if ui.selectable_label(self.selected.as_deref() == Some(id.as_str()), "Controller details").clicked() {
                self.select(id.clone());
            }
            for b in &c.buses {
                let mut label = b.label();
                let used = self.rates.buses.get(&b.busnum).map_or(0.0, Series::now);
                let frac = used / usable_bytes_per_sec(b.speed_mbps());
                if live {
                    label += &format!("  ·  {}", fmt_rate(used));
                }
                egui::CollapsingHeader::new(RichText::new(label).color(speed_color(b.speed_mbps()))).id_salt(&b.root.sysname).default_open(true).show(ui, |ui| {
                    if live && !b.root.children.is_empty() {
                        usage_bar(ui, used, b.speed_mbps(), frac);
                    }
                    if b.root.children.is_empty() {
                        ui.weak("nothing plugged in");
                    }
                    for d in &b.root.children {
                        self.device_tree(ui, d, live);
                    }
                });
            }
        });
    }

    fn select(&mut self, id: String) {
        self.rename_text = id.strip_prefix("pci:").and_then(|a| self.names.get(a)).unwrap_or_default().to_string();
        self.selected = Some(id);
    }

    fn device_line(&self, d: &Device, live: bool) -> RichText {
        let port = d.port_path().unwrap_or("?");
        let mut text = format!("Port {port}  ·  {}  ·  {}", d.display_name(), sysfs::speed_short(d.speed_mbps));
        if let Some(kind) = d.sdr {
            text = format!("📻 {text}  ·  {kind}");
        }
        if live {
            text += &format!("  ·  {}", fmt_rate(self.rates.device(d).map_or(0.0, Series::now)));
        }
        let errors = self.rates.errors(d).map_or(0, |c| c.errors);
        if errors > 0 {
            text += &format!("  ⚠ {errors} transfer errors");
        }
        if d.below_capable_speed() {
            text += "  ⚠ USB 3 device running at USB 2 speed";
        }
        if let Some(i) = self.watcher.issue_for(&d.sysname) {
            if !text.contains('⚠') {
                text += "  ⚠";
            }
            RichText::new(text).color(severity_color(i.severity)).strong()
        } else if errors > 0 || d.below_capable_speed() {
            RichText::new(text).color(AMBER)
        } else if d.sdr.is_some() {
            RichText::new(text).color(PURPLE).strong()
        } else {
            RichText::new(text)
        }
    }

    fn device_tree(&mut self, ui: &mut egui::Ui, d: &Device, live: bool) {
        let is_sel = self.selected.as_deref() == Some(d.sysname.as_str());
        let line = self.device_line(d, live);
        if d.children.is_empty() {
            if ui.selectable_label(is_sel, line).clicked() {
                self.select(d.sysname.clone());
            }
            return;
        }
        egui::CollapsingHeader::new(line).id_salt(&d.sysname).default_open(true).show(ui, |ui| {
            if ui.selectable_label(is_sel, "Hub details").clicked() {
                self.select(d.sysname.clone());
            }
            for c in &d.children {
                self.device_tree(ui, c, live);
            }
        });
    }

    fn details_panel(&mut self, ui: &mut egui::Ui) {
        let live = self.monitor.state() == State::Running;
        let Some(sel) = self.selected.clone() else {
            ui.heading("Details");
            ui.label("Click a controller or device to see its details and traffic graph.");
            ui.add_space(8.0);
            legend(ui);
            return;
        };

        if let Some(addr) = sel.strip_prefix("pci:") {
            let Some(c) = self.topo.controllers.iter().find(|c| c.pci_addr == addr).cloned() else { return };
            ui.heading(self.names.get(addr).unwrap_or(&c.name).to_string());
            egui::Grid::new("ctrl").num_columns(2).striped(true).show(ui, |ui| {
                row(ui, "Chip", c.name.clone());
                row(ui, "Location", c.kind.label());
                row(ui, "PCI address", c.pci_addr.clone());
                row(ui, "Driver", c.driver.clone());
                row(ui, "PCIe link", c.pcie_link.clone());
            });

            ui.add_space(8.0);
            ui.label(RichText::new("Your name for this controller").strong());
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.rename_text).hint_text("e.g. Renesas card, top slot").desired_width(220.0));
                if ui.button("Save").clicked() {
                    if let Err(e) = self.names.set(addr, &self.rename_text) {
                        self.save_error = Some(format!("Couldn't save the name: {e}"));
                    }
                }
            });
            ui.weak("Leave empty and Save to go back to the automatic label.");

            if live {
                for b in &c.buses {
                    ui.add_space(8.0);
                    let s = self.rates.buses.get(&b.busnum);
                    let cap = usable_bytes_per_sec(b.speed_mbps());
                    ui.label(RichText::new(format!("{}  ·  {}", b.label(), fmt_rate(s.map_or(0.0, Series::now)))).strong());
                    graph(ui, s, Some(cap));
                }
            }
            let sdrs = c.sdrs();
            if !sdrs.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new(format!("SDRs on this controller ({})", sdrs.len())).strong().color(PURPLE));
                for d in sdrs {
                    ui.label(format!("• {} on bus {} port {}", d.sdr.unwrap_or("SDR"), d.busnum, d.port_path().unwrap_or("?")));
                }
                ui.weak("All SDRs on one controller share its bandwidth.");
            }
            if c.kind == ControllerKind::Unknown {
                ui.add_space(8.0);
                ui.weak("This controller sits behind a PCIe bridge but your motherboard doesn't say which slot, so it could be a plug-in card or an extra USB chip on the board. Give it a name above if you know.");
            }
            return;
        }

        let devices = self.topo.all_devices();
        let Some(d) = devices.iter().find(|d| d.sysname == sel) else {
            ui.label("That device has been unplugged.");
            return;
        };
        ui.heading(d.display_name());
        egui::Grid::new("dev").num_columns(2).striped(true).show(ui, |ui| {
            row(ui, "SDR type", d.sdr.unwrap_or_default());
            row(ui, "Manufacturer", d.manufacturer.clone());
            row(ui, "Vendor:Product", format!("{:04x}:{:04x}", d.vendor_id, d.product_id));
            row(ui, "Serial", d.serial.clone());
            row(ui, "Bus / port", format!("bus {}, port {}", d.busnum, d.port_path().unwrap_or("root")));
            row(ui, "Device number", d.devnum.to_string());
            row(ui, "Link speed", sysfs::speed_name(d.speed_mbps));
            row(ui, "Supports", if d.usb_version.is_empty() { String::new() } else { format!("USB {}", d.usb_version) });
            row(ui, "Max power", d.max_power.clone());
            row(ui, "Drivers", d.drivers.join(", "));
            row(ui, "sysfs name", d.sysname.clone());
        });
        if live {
            let s = self.rates.device(d);
            ui.add_space(8.0);
            ui.label(RichText::new(format!("Traffic  ·  now {}  ·  peak {}", fmt_rate(s.map_or(0.0, Series::now)), fmt_rate(s.map_or(0.0, Series::peak)))).strong());
            graph(ui, s, None);
        }
        if let Some(c) = self.rates.errors(d) {
            ui.add_space(8.0);
            ui.colored_label(AMBER, format!("⚠ {} transfer errors since the monitor started", c.errors));
            let mut codes: Vec<_> = c.error_codes.iter().collect();
            codes.sort_by(|a, b| b.1.cmp(a.1));
            for (code, n) in codes {
                ui.label(format!("• {n} × {}", traffic::error_name(*code)));
            }
        }
        if d.below_capable_speed() {
            ui.add_space(8.0);
            ui.colored_label(AMBER, format!("⚠ This device supports USB {} but is running at {}. Check the port, cable or hub it goes through.", d.usb_version, sysfs::speed_name(d.speed_mbps)));
        }
    }
}

fn usage_color(frac: f64) -> Color32 {
    if frac >= 0.8 {
        RED
    } else if frac >= 0.5 {
        AMBER
    } else {
        GREEN
    }
}

fn usage_bar(ui: &mut egui::Ui, used: f64, link_mbps: f64, frac: f64) {
    let text = format!("{:.0}% busy  ·  {} of about {} usable", frac * 100.0, fmt_rate(used), fmt_rate(usable_bytes_per_sec(link_mbps)));
    ui.horizontal(|ui| {
        ui.add(egui::ProgressBar::new(frac.min(1.0) as f32).desired_width(160.0).desired_height(10.0).fill(usage_color(frac)));
        ui.label(RichText::new(text).color(usage_color(frac)));
    });
}

/// A 60-second line graph. With `cap`, the scale is fixed to that capacity
/// and the line is coloured by how full the bus is.
fn graph(ui: &mut egui::Ui, s: Option<&Series>, cap: Option<f64>) {
    let (resp, painter) = ui.allocate_painter(egui::vec2(ui.available_width().min(360.0), 70.0), egui::Sense::hover());
    let rect = resp.rect;
    painter.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
    let Some(s) = s.filter(|s| !s.history.is_empty()) else {
        painter.text(rect.center(), egui::Align2::CENTER_CENTER, "collecting…", egui::FontId::proportional(12.0), Color32::GRAY);
        return;
    };
    // Leave headroom above a device's own peak so small wobbles look small.
    let top = cap.unwrap_or_else(|| s.peak() * 1.25).max(1.0);
    let color = cap.map_or(BLUE, |c| usage_color(s.peak() / c));
    let step = rect.width() / (HISTORY_SECS - 1) as f32;
    let offset = HISTORY_SECS - s.history.len();
    let points: Vec<egui::Pos2> = s
        .history
        .iter()
        .enumerate()
        .map(|(i, v)| egui::pos2(rect.left() + (i + offset) as f32 * step, rect.bottom() - (v / top).min(1.0) as f32 * (rect.height() - 4.0)))
        .collect();
    painter.add(egui::Shape::line(points, Stroke::new(1.5, color)));
    let scale = if cap.is_some() { format!("top = {} usable", fmt_rate(top)) } else { format!("top = {}", fmt_rate(top)) };
    painter.text(rect.left_top() + egui::vec2(4.0, 2.0), egui::Align2::LEFT_TOP, scale, egui::FontId::proportional(10.0), Color32::GRAY);
    painter.text(rect.left_bottom() + egui::vec2(4.0, -2.0), egui::Align2::LEFT_BOTTOM, "60 s ago", egui::FontId::proportional(10.0), Color32::GRAY);
    painter.text(rect.right_bottom() + egui::vec2(-4.0, -2.0), egui::Align2::RIGHT_BOTTOM, "now", egui::FontId::proportional(10.0), Color32::GRAY);
}

fn severity_color(s: Severity) -> Color32 {
    match s {
        Severity::Problem => RED,
        Severity::Warning => AMBER,
        Severity::Info => Color32::GRAY,
    }
}

fn kind_color(kind: &ControllerKind) -> Color32 {
    match kind {
        ControllerKind::Motherboard => BLUE,
        ControllerKind::AddOnCard { .. } | ControllerKind::SharedCard { .. } => GREEN,
        ControllerKind::Unknown => Color32::GRAY,
    }
}

fn speed_color(mbps: f64) -> Color32 {
    if mbps > 480.0 {
        BLUE
    } else if mbps >= 480.0 {
        GREEN
    } else {
        Color32::GRAY
    }
}

fn row(ui: &mut egui::Ui, k: &str, v: impl Into<String>) {
    let v = v.into();
    if v.is_empty() {
        return;
    }
    ui.label(RichText::new(k).weak());
    ui.label(v);
    ui.end_row();
}

fn legend(ui: &mut egui::Ui) {
    ui.label(RichText::new("Colours").strong());
    ui.colored_label(BLUE, "Motherboard controller / USB 3 bus");
    ui.colored_label(GREEN, "Add-on card / USB 2 bus / bus under 50% busy");
    ui.colored_label(AMBER, "Bus 50–80% busy, or a device with a problem");
    ui.colored_label(RED, "Bus over 80% busy");
    ui.colored_label(PURPLE, "📻 SDR receiver");
}
