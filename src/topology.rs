//! Finds the drives in the machine and the controller ("lane") each one hangs off.
//!
//! Drives on the same lane share its bandwidth, so Corsia plans by lane, not by drive.
//! Everything here comes from sysfs and /proc, so it needs no root.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub enum LaneKind {
    Sata,
    Usb2,
    Usb3,
    Nvme,
    Sd,
}

impl LaneKind {
    pub fn label(&self) -> &'static str {
        match self {
            LaneKind::Sata => "SATA",
            LaneKind::Usb2 => "USB 2.0",
            LaneKind::Usb3 => "USB 3",
            LaneKind::Nvme => "NVMe",
            LaneKind::Sd => "SD card",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Lane {
    /// PCI address of the controller, for example 0000:00:1f.2.
    pub id: String,
    pub kind: LaneKind,
    pub vendor: String,
    pub device: String,
    /// Bandwidth every drive on this lane shares, when the lane has such a limit.
    pub shared_mb_s: Option<u32>,
    /// PCIe link from the controller to the CPU, for example "5.0 GT/s x1".
    pub uplink: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Bus {
    Sata { link: Option<String> },
    Usb { port: String, speed_mbps: u32, version: String, uas: bool },
    Nvme { link: Option<String> },
    Sd,
    Other,
}

#[derive(Clone, Debug)]
pub struct Mount {
    pub point: PathBuf,
    pub fstype: String,
    pub read_only: bool,
}

#[derive(Clone, Debug)]
pub struct Drive {
    pub name: String,
    pub model: String,
    pub size_bytes: u64,
    pub rotational: bool,
    pub removable: bool,
    pub bus: Bus,
    pub lane: Option<String>,
    pub mounts: Vec<Mount>,
}

impl Drive {
    pub fn kind_word(&self) -> &'static str {
        // Cheap USB sticks often claim to spin, so "removable" wins over "rotational" on USB.
        match (&self.bus, self.rotational) {
            (Bus::Sd, _) => "memory card",
            (Bus::Nvme { .. }, _) => "NVMe SSD",
            (Bus::Usb { .. }, _) if self.removable => "flash drive",
            (_, true) => "hard disk",
            _ => "SSD",
        }
    }

    /// The fastest this drive's own link can carry, before any sharing.
    pub fn link_mb_s(&self) -> Option<u32> {
        match &self.bus {
            Bus::Sata { link: Some(l) } => Some(sata_mb_s(l)),
            Bus::Usb { speed_mbps, uas, .. } => Some(usb_mb_s(*speed_mbps, *uas)),
            Bus::Nvme { link: Some(l) } => pcie_mb_s(l),
            _ => None,
        }
    }
}

pub struct Topology {
    pub lanes: Vec<Lane>,
    pub drives: Vec<Drive>,
}

impl Topology {
    pub fn discover() -> Topology {
        let names = PciNames::load();
        let lanes = discover_lanes(&names);
        let mounts = read_mounts();
        let drives = discover_drives(&mounts);
        Topology { lanes, drives }
    }

    pub fn lane(&self, id: &str) -> Option<&Lane> {
        self.lanes.iter().find(|l| l.id == id)
    }

    pub fn drives_on(&self, lane: &str) -> Vec<&Drive> {
        self.drives.iter().filter(|d| d.lane.as_deref() == Some(lane)).collect()
    }

    pub fn drive(&self, name: &str) -> Option<&Drive> {
        self.drives.iter().find(|d| d.name == name)
    }

    /// The drive a directory lives on, through the longest mount point that contains it.
    pub fn drive_for_path(&self, path: &Path) -> Option<&Drive> {
        let path = fs::canonicalize(path).ok()?;
        self.drives
            .iter()
            .flat_map(|d| d.mounts.iter().map(move |m| (d, m)))
            .filter(|(_, m)| path.starts_with(&m.point))
            .max_by_key(|(_, m)| m.point.as_os_str().len())
            .map(|(d, _)| d)
    }

    /// Plain-language tips: wrong ports, shared lanes, lanes left empty.
    pub fn advice(&self) -> Vec<String> {
        let mut tips = Vec::new();
        let usb3 = self.lanes.iter().find(|l| l.kind == LaneKind::Usb3);
        for d in &self.drives {
            if let Bus::Usb { speed_mbps, version, uas, .. } = &d.bus {
                let ver: f32 = version.trim().parse().unwrap_or(0.0);
                if *speed_mbps <= 480 {
                    match (ver >= 2.1, usb3) {
                        (true, Some(l)) => tips.push(format!(
                            "{} is running at USB 2.0 speed (480 Mb/s, about 40 MB/s). It looks USB 3 capable: move it to a USB 3 port ({} controller, usually the blue ports) for up to about 10 times the speed.",
                            d.name, l.vendor
                        )),
                        (false, Some(_)) => tips.push(format!(
                            "{} is a USB 2.0 device, capped near 40 MB/s. A USB 3 enclosure or stick would lift that to about 400 MB/s on this machine's USB 3 port.",
                            d.name
                        )),
                        _ => {}
                    }
                }
                if *speed_mbps >= 5000 && !uas {
                    tips.push(format!(
                        "{} uses the older Bulk-Only protocol, which sends one request at a time. An enclosure with UAS would handle random reads much better.",
                        d.name
                    ));
                }
            }
        }
        for lane in &self.lanes {
            let on = self.drives_on(&lane.id);
            if on.len() > 1 && lane.shared_mb_s.is_some() {
                let names: Vec<&str> = on.iter().map(|d| d.name.as_str()).collect();
                tips.push(format!(
                    "{} share one {} lane (about {} MB/s between them). Spread them over different lanes to add their speed together.",
                    names.join(" and "),
                    lane.kind.label(),
                    lane.shared_mb_s.unwrap_or(0)
                ));
            }
        }
        let free: Vec<String> = self
            .lanes
            .iter()
            .filter(|l| self.drives_on(&l.id).is_empty())
            .map(|l| match l.shared_mb_s {
                Some(mb) => format!("{} (about {} MB/s)", l.kind.label(), mb),
                None => l.kind.label().to_string(),
            })
            .collect();
        if !free.is_empty() {
            tips.push(format!(
                "{} free lane{}: {}. A drive on each adds its full speed to the total.",
                free.len(),
                if free.len() == 1 { "" } else { "s" },
                free.join(", ")
            ));
        }
        tips
    }
}

// ---------------------------------------------------------------------------
// Lanes

fn discover_lanes(names: &PciNames) -> Vec<Lane> {
    let mut lanes = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") else { return lanes };
    for e in entries.flatten() {
        let dir = e.path();
        let id = e.file_name().to_string_lossy().to_string();
        let class = read_hex(&dir.join("class")).unwrap_or(0);
        let kind = match class {
            c if c >> 8 == 0x0106 || c >> 8 == 0x0101 => LaneKind::Sata,
            c if c >> 8 == 0x0108 => LaneKind::Nvme,
            0x0c0320 => LaneKind::Usb2,
            0x0c0330 | 0x0c0340 => LaneKind::Usb3,
            // SD readers: only the function that actually carries an mmc host.
            _ if dir.join("mmc_host").is_dir() => LaneKind::Sd,
            _ => continue,
        };
        let vendor_id = read_hex(&dir.join("vendor")).unwrap_or(0) as u16;
        let device_id = read_hex(&dir.join("device")).unwrap_or(0) as u16;
        let (vendor, device) = names.lookup(vendor_id, device_id);
        let uplink = read_trim(&dir.join("current_link_speed")).and_then(|s| {
            let w = read_trim(&dir.join("current_link_width"))?;
            let speed = s.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
            Some(format!("{speed} x{w}"))
        });
        let mut kind = kind;
        let shared_mb_s = match kind {
            LaneKind::Usb2 => Some(40),
            LaneKind::Usb3 => {
                let top = usb_root_speeds(&dir).into_iter().max().unwrap_or(5000);
                if top <= 480 {
                    kind = LaneKind::Usb2;
                }
                let bus = usb_mb_s(top, true);
                let up = uplink.as_deref().and_then(pcie_mb_s);
                Some(up.map_or(bus, |u| u.min(bus)))
            }
            _ => None,
        };
        lanes.push(Lane { id, kind, vendor, device, shared_mb_s, uplink });
    }
    lanes.sort_by_key(|l| (lane_rank(&l.kind), l.id.clone()));
    lanes
}

fn lane_rank(k: &LaneKind) -> u8 {
    match k {
        LaneKind::Nvme => 0,
        LaneKind::Sata => 1,
        LaneKind::Usb3 => 2,
        LaneKind::Usb2 => 3,
        LaneKind::Sd => 4,
    }
}

fn usb_root_speeds(pci: &Path) -> Vec<u32> {
    let Ok(entries) = fs::read_dir(pci) else { return vec![] };
    entries
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.starts_with("usb") && n[3..].chars().all(|c| c.is_ascii_digit()) && n.len() > 3
        })
        .filter_map(|e| read_trim(&e.path().join("speed"))?.parse().ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Drives

fn discover_drives(mounts: &BTreeMap<String, Vec<Mount>>) -> Vec<Drive> {
    let mut drives = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/block") else { return drives };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let skip = ["loop", "ram", "zram", "dm-", "md", "sr", "nbd", "fd", "zd"];
        if skip.iter().any(|p| name.starts_with(p)) || name.contains("boot") || name.contains("rpmb") {
            continue;
        }
        let sys = PathBuf::from("/sys/block").join(&name);
        let Ok(real) = fs::canonicalize(&sys) else { continue };
        let real_s = real.to_string_lossy().to_string();
        if real_s.contains("/virtual/") {
            continue;
        }
        let parts: Vec<&str> = real_s.split('/').collect();
        let lane = parts.iter().rev().find(|p| is_pci_addr(p)).map(|s| s.to_string());
        let bus = bus_of(&parts);
        let size_bytes = read_trim(&sys.join("size")).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0) * 512;
        if size_bytes == 0 {
            continue; // an empty card reader slot
        }
        let vendor = read_trim(&sys.join("device/vendor")).unwrap_or_default();
        let model = read_trim(&sys.join("device/model"))
            .or_else(|| read_trim(&sys.join("device/name")))
            .unwrap_or_else(|| "unknown model".into());
        // USB bridges put the first 8 characters of the ATA model in the SCSI vendor field,
        // so a full 8-character vendor joins the model without a space.
        let model = if vendor.is_empty() || vendor == "ATA" || model.starts_with(&vendor) {
            model
        } else if vendor.chars().count() == 8 {
            format!("{vendor}{model}")
        } else {
            format!("{vendor} {model}")
        };
        drives.push(Drive {
            rotational: read_trim(&sys.join("queue/rotational")).as_deref() == Some("1"),
            removable: read_trim(&sys.join("removable")).as_deref() == Some("1"),
            mounts: mounts.get(&name).cloned().unwrap_or_default(),
            name,
            model,
            size_bytes,
            bus,
            lane,
        });
    }
    drives.sort_by(|a, b| a.name.cmp(&b.name));
    drives
}

fn bus_of(parts: &[&str]) -> Bus {
    let prefix = |i: usize| -> PathBuf { PathBuf::from(parts[..=i].join("/")) };
    if let Some(i) = parts.iter().position(|p| p.starts_with("ata") && p[3..].chars().all(|c| c.is_ascii_digit()) && p.len() > 3) {
        let n = &parts[i][3..];
        let link = read_trim(&prefix(i).join(format!("link{n}/ata_link/link{n}/sata_spd")));
        return Bus::Sata { link };
    }
    if parts.iter().any(|p| p.starts_with("usb") && p.len() > 3 && p[3..].chars().all(|c| c.is_ascii_digit())) {
        if let Some(i) = parts.iter().rposition(|p| is_usb_port(p)) {
            let dir = prefix(i);
            let port = parts[i].to_string();
            let speed_mbps = read_trim(&dir.join("speed")).and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.0) as u32;
            let version = read_trim(&dir.join("version")).unwrap_or_default();
            let uas = fs::read_dir(&dir)
                .map(|it| {
                    it.flatten().any(|e| {
                        e.file_name().to_string_lossy().starts_with(&format!("{port}:"))
                            && fs::read_link(e.path().join("driver"))
                                .map(|l| l.file_name().map(|f| f == "uas").unwrap_or(false))
                                .unwrap_or(false)
                    })
                })
                .unwrap_or(false);
            return Bus::Usb { port, speed_mbps, version, uas };
        }
    }
    if let Some(i) = parts.iter().position(|p| *p == "nvme") {
        let pci = parts[..i].iter().rev().find(|p| is_pci_addr(p));
        let link = pci.and_then(|addr| {
            let dir = PathBuf::from("/sys/bus/pci/devices").join(addr);
            let s = read_trim(&dir.join("current_link_speed"))?;
            let w = read_trim(&dir.join("current_link_width"))?;
            Some(format!("{} x{w}", s.split_whitespace().take(2).collect::<Vec<_>>().join(" ")))
        });
        return Bus::Nvme { link };
    }
    if parts.contains(&"mmc_host") {
        return Bus::Sd;
    }
    Bus::Other
}

fn is_pci_addr(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 12 && b[4] == b':' && b[7] == b':' && b[10] == b'.' && s.chars().filter(|c| *c != ':' && *c != '.').all(|c| c.is_ascii_hexdigit())
}

fn is_usb_port(s: &str) -> bool {
    let Some((bus, rest)) = s.split_once('-') else { return false };
    !bus.is_empty() && bus.chars().all(|c| c.is_ascii_digit()) && !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == '.')
}

// ---------------------------------------------------------------------------
// Mounts

/// Whole-disk name (sda, nvme0n1) to the filesystems mounted from it.
fn read_mounts() -> BTreeMap<String, Vec<Mount>> {
    let mut map: BTreeMap<String, Vec<Mount>> = BTreeMap::new();
    let Ok(text) = fs::read_to_string("/proc/self/mountinfo") else { return map };
    for line in text.lines() {
        let Some((left, right)) = line.split_once(" - ") else { continue };
        let l: Vec<&str> = left.split(' ').collect();
        let r: Vec<&str> = right.split(' ').collect();
        if l.len() < 6 || r.len() < 2 || !r[1].starts_with("/dev/") {
            continue;
        }
        let point = PathBuf::from(unescape(l[4]));
        let read_only = l[5].split(',').any(|o| o == "ro");
        let fstype = r[0].to_string();
        for disk in disks_of(Path::new(r[1])) {
            let list = map.entry(disk).or_default();
            if !list.iter().any(|m| m.point == point) {
                list.push(Mount { point: point.clone(), fstype: fstype.clone(), read_only });
            }
        }
    }
    map
}

/// A device node to the whole disks under it, through partitions and device-mapper.
fn disks_of(dev: &Path) -> Vec<String> {
    let Ok(real) = fs::canonicalize(dev) else { return vec![] };
    let Some(name) = real.file_name().map(|n| n.to_string_lossy().to_string()) else { return vec![] };
    disks_of_name(&name, 0)
}

fn disks_of_name(name: &str, depth: u8) -> Vec<String> {
    if depth > 8 {
        return vec![];
    }
    let class = PathBuf::from("/sys/class/block").join(name);
    if let Ok(slaves) = fs::read_dir(class.join("slaves")) {
        let found: Vec<String> = slaves
            .flatten()
            .flat_map(|s| disks_of_name(&s.file_name().to_string_lossy(), depth + 1))
            .collect();
        if !found.is_empty() {
            return found;
        }
    }
    if class.join("partition").exists() {
        if let Ok(real) = fs::canonicalize(&class) {
            if let Some(parent) = real.parent().and_then(|p| p.file_name()) {
                return disks_of_name(&parent.to_string_lossy(), depth + 1);
            }
        }
    }
    vec![name.to_string()]
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Free bytes available to this user at a path.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let c = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some(s.f_bavail as u64 * s.f_frsize as u64)
}

pub fn writable(path: &Path) -> bool {
    let Ok(c) = CString::new(path.as_os_str().as_encoded_bytes()) else { return false };
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

// ---------------------------------------------------------------------------
// Speeds, all rough and on the cautious side

pub fn sata_mb_s(link: &str) -> u32 {
    let g: f32 = link.split_whitespace().next().and_then(|s| s.parse().ok()).unwrap_or(1.5);
    match g {
        g if g >= 6.0 => 550,
        g if g >= 3.0 => 280,
        _ => 140,
    }
}

pub fn usb_mb_s(speed_mbps: u32, uas: bool) -> u32 {
    match speed_mbps {
        0..=12 => 1,
        13..=480 => 40,
        481..=5000 => if uas { 430 } else { 380 },
        5001..=10000 => 900,
        10001..=20000 => 1800,
        _ => 3500,
    }
}

/// "5.0 GT/s x1" or "5.0 GT/s PCIe" style text to usable MB/s.
pub fn pcie_mb_s(link: &str) -> Option<u32> {
    let gt: f32 = link.split_whitespace().next()?.parse().ok()?;
    let width: u32 = link.split_whitespace().find_map(|w| w.strip_prefix('x')?.parse().ok()).unwrap_or(1);
    let per_lane = match gt {
        g if g >= 64.0 => 7877.0,
        g if g >= 32.0 => 3938.0,
        g if g >= 16.0 => 1969.0,
        g if g >= 8.0 => 985.0,
        g if g >= 5.0 => 500.0,
        _ => 250.0,
    };
    Some((per_lane * width as f32 * 0.85) as u32)
}

// ---------------------------------------------------------------------------
// Helpers

fn read_trim(p: &Path) -> Option<String> {
    let s = fs::read_to_string(p).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn read_hex(p: &Path) -> Option<u32> {
    let s = read_trim(p)?;
    u32::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

pub fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", cut.trim_end())
    }
}

/// Vendor and device names from the system's pci.ids, when it has one.
struct PciNames {
    text: String,
}

impl PciNames {
    fn load() -> PciNames {
        let text = ["/usr/share/misc/pci.ids", "/usr/share/hwdata/pci.ids", "/usr/share/pci.ids"]
            .iter()
            .find_map(|p| fs::read_to_string(p).ok())
            .unwrap_or_default();
        PciNames { text }
    }

    fn lookup(&self, vendor: u16, device: u16) -> (String, String) {
        let v = format!("{vendor:04x}  ");
        let d = format!("\t{device:04x}  ");
        let mut vendor_name = None;
        let mut device_name = None;
        let mut inside = false;
        for line in self.text.lines() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            if !line.starts_with('\t') {
                if inside {
                    break;
                }
                if let Some(rest) = line.strip_prefix(&v) {
                    vendor_name = Some(rest.to_string());
                    inside = true;
                }
            } else if inside {
                if let Some(rest) = line.strip_prefix(&d) {
                    device_name = Some(rest.to_string());
                    break;
                }
            }
        }
        let vendor_name = vendor_name.unwrap_or_else(|| format!("vendor {vendor:04x}"));
        let vendor_short = vendor_name.split_whitespace().next().unwrap_or("").trim_end_matches(',').to_string();
        // pci.ids names are long; drop the words that say nothing about speed.
        let mut device_name = device_name.unwrap_or_else(|| format!("device {device:04x}"));
        for noise in [" Chipset Family", " Host Controller", " Controller"] {
            device_name = device_name.replace(noise, "");
        }
        (vendor_short, device_name)
    }
}
