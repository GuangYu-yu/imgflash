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

    pub transport: String,      // NVMe / VirtIO / eMMC / IDE / USB / SATA / SAS / ATA / Unknown

    pub disk_type: String,      // SSD / HDD；无法证实时为空（不猜）

    pub is_removable: bool,     // true = removable (USB), false = fixed
    pub is_mounted: bool,
    pub mount_point: Option<String>,

    pub model: Option<String>,  // 优先 SAT IDENTIFY（USB 桥后真实盘型号），回退 sysfs
    pub serial: Option<String>, // SAT IDENTIFY（sd）/ NVMe sysfs
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

        // NVMe 控制器属性含 serial；sd 的 sysfs 无 serial，由 SAT 补齐
        let serial = fs::read_to_string(base.join("device/serial"))
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

        // SAT IDENTIFY：sd 盘逐盘探测（SATA 直连与 USB 桥都覆盖），
        // 成功则获得真实 model/serial 与 word217 介质转速，失败回退 sysfs 值
        let sat = if name.starts_with("sd") {
            sat_probe(Path::new(&format!("/dev/{}", name)))
        } else {
            None
        };

        // SAT 成功表明设备支持 ATA 命令集，Unknown 可升级为 ATA
        let transport = if sat.is_some() && transport == "Unknown" {
            "ATA".to_string()
        } else {
            transport
        };

        let model = sat.as_ref().map(|(m, _, _)| m.clone()).or(model);
        let serial = sat
            .as_ref()
            .map(|(_, s, _)| s.clone())
            .filter(|s| !s.is_empty())
            .or(serial);
        let sat_type = sat
            .as_ref()
            .and_then(|(_, _, r)| match r {
                1 => Some("SSD"),
                n if *n > 1 => Some("HDD"),
                _ => None,
            });
        let disk_type = sat_type
            .map(String::from)
            .unwrap_or_else(|| Self::detect_disk_type(name, &base));

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
            serial,
        })
    }

    /// 介质类型只输出可证实值：rotational=1 是内核事实（HDD）；
    /// NVMe 协议即非易失固态；rotational=0 的其他设备（U 盘、virtio 等）
    /// 既非 SSD 亦非 HDD，留空不猜。
    fn detect_disk_type(name: &str, base: &Path) -> String {
        if name.starts_with("nvme") {
            return "SSD".to_string();
        }
        let rotational: u8 = fs::read_to_string(base.join("queue/rotational"))
            .unwrap_or_else(|_| "0".to_string())
            .trim()
            .parse()
            .unwrap_or(0);
        if rotational == 1 { "HDD".to_string() } else { String::new() }
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

    /// sd 盘 transport 只依据内核事实：SAS 设备的 sas_device 对象，以及
    /// 设备路径中的内核命名约定目录（usbN / ataX / virtioN）。
    /// 识别不了返回 Unknown，由 SAT 探测成功后升级为 ATA。
    fn detect_sd_transport(name: &str) -> String {
        let base = Path::new("/sys/block").join(name);

        if base.join("device/sas_device").is_dir() {
            return "SAS".to_string();
        }

        if let Ok(link) = fs::read_link(base.join("device")) {
            for comp in link.to_string_lossy().split('/') {
                if Self::is_numbered_kernel_dev(comp, "usb") { return "USB".to_string(); }
                if Self::is_numbered_kernel_dev(comp, "ata") { return "SATA".to_string(); }
                if Self::is_numbered_kernel_dev(comp, "virtio") { return "VirtIO".to_string(); }
            }
        }

        "Unknown".to_string()
    }

    /// 匹配 usb1 / ata3 / virtio0 这类内核命名约定的路径组件
    fn is_numbered_kernel_dev(comp: &str, prefix: &str) -> bool {
        comp.strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
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
                            suffix.chars().next().is_some_and(|c| c.is_ascii_digit() || c == 'p')
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

/// SAT 探测封装，返回 (model, serial, rotation_rate)。
/// 非 Linux 目标恒为 None，使上层逻辑无需 cfg 分支。
#[cfg(target_os = "linux")]
fn sat_probe(dev_path: &Path) -> Option<(String, String, u16)> {
    crate::sat::probe(dev_path).map(|i| (i.model, i.serial, i.rotation_rate))
}

#[cfg(not(target_os = "linux"))]
fn sat_probe(_dev_path: &Path) -> Option<(String, String, u16)> {
    None
}