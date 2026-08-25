use std::fs;
use std::path::Path;

use crate::utils::{format_bytes, SECTOR};

/// Information about a single block device, obtained via /sys/block.
#[derive(Debug, Clone)]
pub struct DiskInfo {
    pub name: String,           // e.g. "sda"
    pub dev_path: String,       // e.g. "/dev/sda"
    pub size_bytes: u64,        // total size in bytes
    pub size_str: String,       // human-readable size

    pub transport: String,      // nvme / usb / scsi / virtio / mmc (from /sys/block directly or best guess)

    pub disk_type: String,      // SSD / HDD (heuristic via queue/rotational)

    pub is_removable: bool,     // true = removable (USB), false = fixed
    pub is_mounted: bool,
    pub mount_point: Option<String>,

    pub model: Option<String>,  // device-reported, may be inaccurate for USB bridges
}

impl DiskInfo {
    /// Enumerate writable disks via /sys/block (same logic as installer.sh).
    pub fn enumerate() -> anyhow::Result<Vec<Self>> {
        let mut disks = Vec::new();
        let sys_block = Path::new("/sys/block");

        if !sys_block.is_dir() {
            return Ok(disks);
        }

        let prefixes = ["sd", "nvme", "vd", "hd", "mmcblk"];

        for entry in fs::read_dir(sys_block)? {
            let entry = entry?;
            let name_os = entry.file_name();
            let name = name_os.to_string_lossy().to_string();

            if !prefixes.iter().any(|p| name.starts_with(p)) {
                continue;
            }

            if let Some(disk) = Self::from_sys_block(&name) {
                disks.push(disk);
            }
        }

        disks.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(disks)
    }

    fn from_sys_block(name: &str) -> Option<Self> {
        let base = Path::new("/sys/block").join(name);
        if !base.is_dir() {
            return None;
        }

        let ro = fs::read_to_string(base.join("ro"))
            .unwrap_or_else(|_| "1".to_string())
            .trim()
            .to_string();
        if ro != "0" {
            return None;
        }

        let sectors: u64 = fs::read_to_string(base.join("size"))
            .unwrap_or_else(|_| "0".to_string())
            .trim()
            .parse()
            .unwrap_or(0);
        let size_bytes = sectors * SECTOR;

        let model = fs::read_to_string(base.join("device/model"))
            .or_else(|_| fs::read_to_string(base.join("device/device/model")))
            .or_else(|_| fs::read_to_string(base.join("device/name")))
            .or_else(|_| fs::read_to_string(base.join("device/device/name")))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let removable: u8 = fs::read_to_string(base.join("removable"))
            .unwrap_or_else(|_| "0".to_string())
            .trim()
            .parse()
            .unwrap_or(0);
        let is_removable = removable == 1;

        let (is_mounted, mount_point) = Self::check_mounted(name);

        let transport = Self::detect_transport(name);
        let disk_type = Self::detect_disk_type(&base);

        Some(Self {
            name: name.to_string(),
            dev_path: format!("/dev/{}", name),
            size_bytes,
            size_str: format_bytes(size_bytes),
            transport,
            disk_type,
            is_removable,
            is_mounted,
            mount_point,
            model,
        })
    }

    fn detect_disk_type(base: &Path) -> String {
        let rotational: u8 = fs::read_to_string(base.join("queue/rotational"))
            .unwrap_or_else(|_| "0".to_string())
            .trim()
            .parse()
            .unwrap_or(0);
        if rotational == 0 { "SSD".to_string() } else { "HDD".to_string() }
    }

    fn detect_transport(name: &str) -> String {
        if name.starts_with("nvme") {
            "NVMe".to_string()
        } else if name.starts_with("vd") {
            "VirtIO".to_string()
        } else if name.starts_with("mmcblk") {
            "eMMC".to_string()
        } else if name.starts_with("hd") {
            "IDE".to_string()
        } else if name.starts_with("sd") {
            Self::detect_sd_transport(name)
        } else {
            "Unknown".to_string()
        }
    }

    fn detect_sd_transport(name: &str) -> String {
        let base = Path::new("/sys/block").join(name);

        // Try device/subsystem first (most direct way)
        if let Ok(link) = fs::read_link(base.join("device/subsystem"))
            && let Some(subsystem) = link.file_name() {
                let s = subsystem.to_string_lossy().to_string();
                if s == "usb" { return "USB".to_string(); }
                if s == "virtio" { return "VirtIO".to_string(); }
                if s != "scsi" { return s; } // e.g. "ata"
        }

        // Fallback: read device symlink path for transport hints
        if let Ok(link) = fs::read_link(base.join("device")) {
            let path = link.to_string_lossy();
            if path.contains("/usb")  { return "USB".to_string(); }
            if path.contains("/virtio") { return "VirtIO".to_string(); }
            if path.contains("/ata")  { return "SATA".to_string(); }
        }

        // Last resort: sysfs markers
        if base.join("device/ata_device").is_dir() { return "SATA".to_string(); }
        if base.join("device/sas_device").is_dir() { return "SAS".to_string(); }

        "SCSI".to_string()
    }

    /// Check if the device or any of its partitions are mounted (via /proc/mounts).
    fn check_mounted(name: &str) -> (bool, Option<String>) {
        if name.trim().is_empty() {
            return (false, None);
        }

        if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
            for line in mounts.lines() {
                let mut parts = line.split_whitespace();
                if let (Some(dev_path), Some(mount_point)) = (parts.next(), parts.next())
                    && let Some(dev_name) = dev_path.strip_prefix("/dev/")
                {
                    let is_match = dev_name == name
                        || dev_name.strip_prefix(name).is_some_and(|suffix| {
                            let first_char = suffix.chars().next().unwrap();
                            first_char.is_ascii_digit() || first_char == 'p'
                        });

                    if is_match {
                        return (true, Some(mount_point.to_string()));
                    }
                }
            }
        }
        (false, None)
    }
}