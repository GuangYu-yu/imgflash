//! Init phase: replaces scripts/init.sh
//!
//! Design philosophy: "brute force + trial-and-error" over "correct modeling".
//! Don't try to understand the hardware. Just try everything until something works.
//! This runs on Linux only (initramfs), gated behind `is_pid1` detection in main.rs.

use std::fs;
use std::path::Path;

use anyhow::bail;

// ── Constants ───────────────────────────────────────────────────────────

const MAX_SCAN_TRIES: u32 = 10;
const SCAN_INTERVAL_SECS: u64 = 1;
pub(crate) const BOOT_MEDIA_DIR: &str = "/media/cdrom";
const IMAGE_DIR: &str = "/image";
const SQUASHFS_FILE: &str = "/media/cdrom/image.squashfs";
const DEVICE_PREFIXES: [&str; 6] = ["sr", "sd", "nvme", "vd", "hd", "mmcblk"];

// ── Public API ──────────────────────────────────────────────────────────

pub fn run_init() -> anyhow::Result<()> {
    eprintln!("ImgFlash init starting...");

    setup_path()?;
    mount_virtual_fs()?;
    parse_cmdline()?;
    load_modules()?;
    scan_and_mount_boot_media()?;
    verify_image()?;

    eprintln!("Init complete. Starting installer...");
    Ok(())
}

pub fn emergency_halt(msg: &str) -> ! {
    eprintln!("ERROR: {}", msg);
    eprintln!("Powering off in 5 seconds...");
    std::thread::sleep(std::time::Duration::from_secs(5));
    let _ = std::process::Command::new("poweroff").arg("-f").status();
    std::process::exit(1);
}

// ── Phase 1: Bootstrap ─────────────────────────────────────────────────

fn setup_path() -> anyhow::Result<()> {
    unsafe {
        std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    }
    Ok(())
}

// ── Phase 2: Virtual Filesystems ────────────────────────────────────────

fn mount_virtual_fs() -> anyhow::Result<()> {
    let dirs = &["/proc", "/sys", "/dev", "/run", "/tmp", BOOT_MEDIA_DIR, IMAGE_DIR,
                 "/dev/pts", "/dev/shm", "/etc", "/root", "/var/log"];
    for dir in dirs {
        let _ = fs::create_dir_all(dir);
    }

    use nix::mount::{mount, MsFlags};

    mount::<str, str, str, str>(Some("proc"), "/proc", Some("proc"),
        MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID | MsFlags::MS_NODEV, None)
        .map_err(|_| anyhow::anyhow!("Failed to mount /proc"))?;

    mount::<str, str, str, str>(Some("sysfs"), "/sys", Some("sysfs"),
        MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID | MsFlags::MS_NODEV, None)
        .map_err(|_| anyhow::anyhow!("Failed to mount /sys"))?;

    if mount::<str, str, str, str>(Some("devtmpfs"), "/dev", Some("devtmpfs"),
        MsFlags::MS_NOSUID, Some("mode=0755,size=2M")).is_err()
    {
        eprintln!("devtmpfs unavailable, using tmpfs");
        mount::<str, str, str, str>(Some("tmpfs"), "/dev", Some("tmpfs"),
            MsFlags::MS_NOSUID, Some("mode=0755,size=2M"))
            .map_err(|_| anyhow::anyhow!("Failed to mount /dev"))?;
    }

    let _ = mount::<str, str, str, str>(Some("devpts"), "/dev/pts", Some("devpts"),
        MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID, Some("gid=5,mode=0620"));

    let _ = mount::<str, str, str, str>(Some("tmpfs"), "/dev/shm", Some("tmpfs"),
        MsFlags::MS_NODEV | MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC, None);

    use nix::sys::stat::{mknod, Mode, SFlag};

    if !Path::new("/dev/null").exists() {
        let _ = mknod("/dev/null", SFlag::S_IFCHR, Mode::from_bits(0o666).unwrap(),
            nix::sys::stat::makedev(1, 3));
    }
    if !Path::new("/dev/kmsg").exists() {
        let _ = mknod("/dev/kmsg", SFlag::S_IFCHR, Mode::from_bits(0o660).unwrap(),
            nix::sys::stat::makedev(1, 11));
    }
    if !Path::new("/dev/ptmx").exists() {
        let _ = mknod("/dev/ptmx", SFlag::S_IFCHR, Mode::from_bits(0o666).unwrap(),
            nix::sys::stat::makedev(5, 2));
    }

    let _ = std::os::unix::fs::symlink("/proc/mounts", "/etc/mtab");

    Ok(())
}

// ── Phase 3: Kernel Command Line ───────────────────────────────────────

fn parse_cmdline() -> anyhow::Result<()> {
    if let Ok(cmdline) = fs::read_to_string("/proc/cmdline") {
        for opt in cmdline.split_whitespace() {
            if opt == "quiet" {
                let _ = fs::write("/proc/sys/kernel/printk", "1\n");
            }
        }
    }
    Ok(())
}

// ── Phase 4: Load Kernel Modules ───────────────────────────────────────

fn load_modules() -> anyhow::Result<()> {
    let modules_path = "/etc/modules";
    if !Path::new(modules_path).exists() {
        return Ok(());
    }

    eprintln!("Loading kernel modules...");

    let content = fs::read_to_string(modules_path)?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let _ = std::process::Command::new("modprobe").arg(line).status();
    }

    load_vendor_specific_modules();

    Ok(())
}

fn load_vendor_specific_modules() {
    if let Ok(vendor) = fs::read_to_string("/sys/devices/virtual/dmi/id/sys_vendor")
        && vendor.trim().contains("VMware")
    {
        eprintln!("VMware detected, loading virtual SCSI drivers...");
        for mod_name in &["ata_piix", "mptspi", "sr_mod"] {
            let _ = std::process::Command::new("modprobe").arg(mod_name).status();
        }
    }
}

// ── Phase 5: Boot Media Scan (brute force) ────────────────────────────

fn scan_and_mount_boot_media() -> anyhow::Result<()> {
    eprintln!("Scanning for boot media...");

    std::thread::sleep(std::time::Duration::from_secs(1));

    for attempt in 0..MAX_SCAN_TRIES {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(SCAN_INTERVAL_SECS));
        }

        let devices = enumerate_dev_devices();

        for device in devices {
            if try_boot_device(&device) {
                eprintln!("Boot media found: {}", device);
                return Ok(());
            }
        }
    }

    bail!("Boot media not found after {} seconds", MAX_SCAN_TRIES);
}

fn enumerate_dev_devices() -> Vec<String> {
    let mut devices = Vec::new();

    if let Ok(entries) = fs::read_dir("/dev") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();

            for prefix in &DEVICE_PREFIXES {
                if name.starts_with(prefix) {
                    let device_path = format!("/dev/{}", name);
                    if is_block_device(&device_path) {
                        devices.push(device_path);
                    }
                    break;
                }
            }
        }
    }

    devices.sort_by(|a, b| {
        let a_is_sr = a.contains("sr");
        let b_is_sr = b.contains("sr");
        if a_is_sr != b_is_sr {
            b_is_sr.cmp(&a_is_sr)
        } else {
            a.cmp(b)
        }
    });

    devices
}

fn is_block_device(path: &str) -> bool {
    use nix::sys::stat::{stat, SFlag};
    if let Ok(st) = stat(path) {
        let mode = SFlag::from_bits_truncate(st.st_mode);
        mode.contains(SFlag::S_IFBLK)
    } else {
        false
    }
}

fn try_boot_device(device: &str) -> bool {
    use nix::mount::{mount, umount, MsFlags};

    let _ = umount(BOOT_MEDIA_DIR);

    if mount::<str, str, str, str>(Some(device), BOOT_MEDIA_DIR, Some("iso9660"),
        MsFlags::MS_RDONLY, None).is_err()
        && mount::<str, str, str, str>(Some(device), BOOT_MEDIA_DIR, Some("vfat"),
            MsFlags::MS_RDONLY, None).is_err()
    {
        return false;
    }

    if !Path::new(SQUASHFS_FILE).exists() {
        let _ = umount(BOOT_MEDIA_DIR);
        return false;
    }

    if mount_squashfs() {
        return true;
    }

    let _ = umount(BOOT_MEDIA_DIR);
    false
}

fn mount_squashfs() -> bool {
    std::process::Command::new("mount")
        .args(["-t", "squashfs", "-o", "ro,loop", SQUASHFS_FILE, IMAGE_DIR])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ── Phase 6: Verify Image ───────────────────────────────────────────────

fn verify_image() -> anyhow::Result<()> {
    if !Path::new(crate::app::App::IMAGE_FILE).exists() {
        bail!("image.img not found in squashfs");
    }
    Ok(())
}