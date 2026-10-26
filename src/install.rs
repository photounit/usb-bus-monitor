//! `--install` / `--uninstall`: puts the program in the desktop's app menu for
//! the current user. Everything goes under ~/.local, so no sudo is needed.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

pub const APP_ID: &str = "usb-monitor";
const ICON: &str = include_str!("../assets/usb-monitor.svg");

struct Paths {
    bin: PathBuf,
    icon: PathBuf,
    desktop: PathBuf,
    apps_dir: PathBuf,
}

fn paths() -> io::Result<Paths> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| io::Error::other("HOME is not set"))?;
    let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"));
    Ok(Paths {
        bin: home.join(".local/bin").join(APP_ID),
        icon: data.join("icons/hicolor/scalable/apps").join(format!("{APP_ID}.svg")),
        desktop: data.join("applications").join(format!("{APP_ID}.desktop")),
        apps_dir: data.join("applications"),
    })
}

fn write_file(path: &PathBuf, contents: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, contents)
}

pub fn install() -> io::Result<()> {
    let p = paths()?;
    let me = std::env::current_exe()?;
    if let Some(dir) = p.bin.parent() {
        fs::create_dir_all(dir)?;
    }
    // Copy to a temporary name first so reinstalling over a running copy works.
    if me != p.bin {
        let tmp = p.bin.with_extension("new");
        fs::copy(&me, &tmp)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        fs::rename(&tmp, &p.bin)?;
    }
    write_file(&p.icon, ICON.as_bytes())?;
    let desktop = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=USB Bus Monitor\n\
         Comment=Watch USB buses, ports and SDR traffic\n\
         Exec={}\n\
         Icon={}\n\
         Terminal=false\n\
         Categories=System;Monitor;HardwareSettings;\n\
         Keywords=usb;sdr;bandwidth;bus;\n\
         StartupWMClass={APP_ID}\n",
        p.bin.display(),
        // A full path rather than a theme name: GNOME doesn't always notice a
        // new icon in the user's icon theme until the next login.
        p.icon.display()
    );
    write_file(&p.desktop, desktop.as_bytes())?;
    // Refreshes the menu right away on desktops that cache it. Harmless if missing.
    let _ = Command::new("update-desktop-database").arg(&p.apps_dir).status();

    println!("Installed USB Bus Monitor.");
    println!("  program:  {}", p.bin.display());
    println!("  launcher: {}", p.desktop.display());
    println!("Find it in the app menu as \"USB Bus Monitor\". Right-click its icon in the dock and pick \"Pin to Dash\" to keep it there.");
    println!("To update later, download the new file and run it with --install again.");
    Ok(())
}

pub fn uninstall() -> io::Result<()> {
    let p = paths()?;
    for f in [&p.bin, &p.icon, &p.desktop] {
        match fs::remove_file(f) {
            Ok(()) => println!("Removed {}", f.display()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    let _ = Command::new("update-desktop-database").arg(&p.apps_dir).status();
    println!("USB Bus Monitor is uninstalled. Your controller names and event log are kept in ~/.config/usb-monitor and ~/.local/state/usb-monitor.");
    Ok(())
}
