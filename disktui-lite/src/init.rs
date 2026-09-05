//! Init phase: initramfs 引导与安装介质定位。
//!
//! 设计原则：brute force + 逐一尝试，不对硬件建模。
//! 仅在 Linux initramfs 中运行（main.rs 的 is_pid1 检测门控）。

use std::fs;
use std::path::Path;

use anyhow::bail;
use nix::libc;
use nix::sys::stat::{mknod, Mode, SFlag};

use crate::utils::{BOOT_MEDIA_DIR, IMAGE_FILE};

// ── Constants ───────────────────────────────────────────────────────────

// 设备号 / ioctl 号为 Linux UAPI 稳定 ABI（include/uapi/linux/loop.h、
// Documentation/admin-guide/devices.txt），跨内核版本恒定，硬编码即规范
const LOOP_CTL_GET_FREE: libc::Ioctl = 0x4C82;
const LOOP_CLR_FD: libc::Ioctl = 0x4C01;
// LOOP_CONFIGURE 自内核 5.8 起提供；本项目内核下限为 Debian bullseye(5.10)，
// 无需 LOOP_SET_FD + LOOP_SET_STATUS64 旧式回退
const LOOP_CONFIGURE: libc::Ioctl = 0x4C0A;
const LO_FLAGS_READ_ONLY: u32 = 1;
const MISC_MAJOR: libc::dev_t = 10;
const LOOP_CTL_MINOR: libc::dev_t = 237;
const LOOP_MAJOR: libc::dev_t = 7;

const MAX_SCAN_TRIES: u32 = 15;
const IMAGE_DIR: &str = "/image";
const SQUASHFS_FILE: &str = "/media/cdrom/image.squashfs";
const DEVICE_PREFIXES: [&str; 6] = ["sr", "sd", "nvme", "vd", "hd", "mmcblk"];

// ── Logging：console + /dev/kmsg，panic 后 dmesg 仍可读 ────────────────

pub fn log(msg: &str) {
    eprintln!("{msg}");
    use std::io::Write as _;
    if let Ok(mut f) = fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = writeln!(f, "imgflash: {msg}");
    }
}

// ── Public API ──────────────────────────────────────────────────────────

pub fn run_init() -> anyhow::Result<()> {
    log("ImgFlash init starting...");

    setup_path()?;
    mount_virtual_fs()?;
    parse_cmdline()?;
    load_modules()?;
    scan_and_mount_boot_media()?;
    verify_image()?;

    log("Init complete. Starting installer...");
    Ok(())
}

pub fn emergency_halt(msg: &str) -> ! {
    eprintln!("ERROR: {}", msg);
    eprintln!("Powering off in 5 seconds...");
    std::thread::sleep(std::time::Duration::from_secs(5));
    #[cfg(target_os = "linux")]
    {
        nix::unistd::sync();
        let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_POWER_OFF);
    }
    // reboot syscall 成功时内核直接关机（永不返回）；仅失败才走到 exit(1) → kernel panic
    std::process::exit(1);
}

// ── Phase 1: Bootstrap ─────────────────────────────────────────────────

fn setup_path() -> anyhow::Result<()> {
    // SAFETY: init 阶段单线程执行（尚无其他线程），set_var 不存在与其他线程
    // 并发读写环境变量的竞态；且仅写入 PATH 一处，不读取可变环境项。
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
        .map_err(|e| anyhow::anyhow!("mount /proc: {e}"))?;

    mount::<str, str, str, str>(Some("sysfs"), "/sys", Some("sysfs"),
        MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID | MsFlags::MS_NODEV, None)
        .map_err(|e| anyhow::anyhow!("mount /sys: {e}"))?;

    if mount::<str, str, str, str>(Some("devtmpfs"), "/dev", Some("devtmpfs"),
        MsFlags::MS_NOSUID, Some("mode=0755,size=2M")).is_err()
    {
        mount::<str, str, str, str>(Some("tmpfs"), "/dev", Some("tmpfs"),
            MsFlags::MS_NOSUID, Some("mode=0755,size=2M"))
            .map_err(|e| anyhow::anyhow!("mount /dev: {e}"))?;
        // tmpfs 不会像 devtmpfs 那样自动生成设备节点，需从 sysfs 重建
        populate_dev_nodes_from_sysfs();
        let _ = mknod("/dev/loop-control", SFlag::S_IFCHR, Mode::from_bits_truncate(0o600),
            nix::sys::stat::makedev(MISC_MAJOR, LOOP_CTL_MINOR));
    }

    let _ = mount::<str, str, str, str>(Some("devpts"), "/dev/pts", Some("devpts"),
        MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID, Some("gid=5,mode=0620"));

    let _ = mount::<str, str, str, str>(Some("tmpfs"), "/dev/shm", Some("tmpfs"),
        MsFlags::MS_NODEV | MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC, None);

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

/// tmpfs /dev fallback：从 sysfs major:minor 重建 block 设备节点
fn populate_dev_nodes_from_sysfs() {
    let Ok(entries) = fs::read_dir("/sys/class/block") else { return };
    for entry in entries.flatten() {
        let Ok(content) = fs::read_to_string(entry.path().join("dev")) else { continue };
        let Some((maj, min)) = content.trim().split_once(':') else { continue };
        let (Ok(maj), Ok(min)) = (maj.parse::<libc::dev_t>(), min.parse::<libc::dev_t>()) else { continue };
        let name = entry.file_name();
        let path = Path::new("/dev").join(&name);
        let _ = fs::remove_file(&path);
        let _ = mknod(&path, SFlag::S_IFBLK, Mode::from_bits_truncate(0o600),
            nix::sys::stat::makedev(maj, min));
    }
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

    log("Loading kernel modules...");

    // 依赖表一次构建全程复用；构建失败 = 无模块可载，与此前 modprobe
    // 全部静默失败等价，只记日志不阻塞（介质扫描会自然失败）
    let loader = match crate::modload::ModuleLoader::new() {
        Ok(l) => l,
        Err(e) => {
            log(&format!("modload: {e}"));
            return Ok(());
        }
    };

    let content = fs::read_to_string(modules_path)?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Err(e) = loader.load(line) {
            log(&format!("module {line}: {e}"));
        }
    }

    load_vendor_specific_modules(&loader);

    Ok(())
}

fn load_vendor_specific_modules(loader: &crate::modload::ModuleLoader) {
    if let Ok(vendor) = fs::read_to_string("/sys/devices/virtual/dmi/id/sys_vendor")
        && vendor.trim().contains("VMware")
    {
        log("VMware detected, loading virtual SCSI drivers...");
        for mod_name in &["ata_piix", "mptspi", "sr_mod"] {
            if let Err(e) = loader.load(mod_name) {
                log(&format!("module {mod_name}: {e}"));
            }
        }
    }
}

// ── Phase 5: Boot Media Scan (brute force) ────────────────────────────

fn scan_and_mount_boot_media() -> anyhow::Result<()> {
    log("Scanning for boot media...");

    // 首次立即扫描；之后间隔从 250ms 起指数退避，1s 封顶。
    // 已失败的设备不永久排除：早期失败可能只是设备尚未稳定
    let mut interval_ms: u64 = 0;

    for attempt in 0..MAX_SCAN_TRIES {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(interval_ms));
            interval_ms = (interval_ms * 2).clamp(250, 1000);
        }

        for device in enumerate_candidate_devices() {
            if try_boot_device(&device) {
                log(&format!("Boot media found: {}", device));
                return Ok(());
            }
        }
    }

    bail!("Boot media not found after {} scan attempts", MAX_SCAN_TRIES);
}

/// 候选设备来自 /sys/class/block（内核权威视图，含 partitions 与
/// removable 等属性来源），DEVICE_PREFIXES 白名单排除 loop*/ram*/dm-*；
/// 挂载仍走 /dev/<name> 节点（devtmpfs 或 sysfs 重建均已就位）
fn enumerate_candidate_devices() -> Vec<String> {
    let mut devices: Vec<String> = Vec::new();

    if let Ok(entries) = fs::read_dir("/sys/class/block") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !DEVICE_PREFIXES.iter().any(|p| name.starts_with(p)) {
                continue;
            }
            let device_path = format!("/dev/{name}");
            if is_block_device(&device_path) {
                devices.push(device_path);
            }
        }
    }

    // 光驱优先（最常见的 boot 介质），其余按名字稳定排序
    devices.sort_by(|a, b| {
        let a_is_sr = a.starts_with("/dev/sr");
        let b_is_sr = b.starts_with("/dev/sr");
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
    // nosuid/nodev 加固；noexec 不可加：grow 工具住在此挂载点的 /grow/ 下，需要可执行
    let sec = MsFlags::MS_NOSUID | MsFlags::MS_NODEV;

    let _ = umount(BOOT_MEDIA_DIR);

    if mount::<str, str, str, str>(Some(device), BOOT_MEDIA_DIR, Some("iso9660"),
        MsFlags::MS_RDONLY | sec, None).is_err()
        && mount::<str, str, str, str>(Some(device), BOOT_MEDIA_DIR, Some("vfat"),
        MsFlags::MS_RDONLY | sec, None).is_err()
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

// loop_info64 / loop_config 的 C 布局镜像（include/uapi/linux/loop.h）
#[repr(C)]
struct LoopInfo64 {
    lo_device: u64, lo_inode: u64, lo_rdevice: u64,
    lo_offset: u64, lo_sizelimit: u64,
    lo_number: u32, lo_encrypt_type: u32, lo_encrypt_key_size: u32, lo_flags: u32,
    lo_file_name: [u8; 64], lo_crypt_name: [u8; 64], lo_encrypt_key: [u8; 32],
    lo_init: [u64; 2],
}

// [u8; 64] 超出 std Default 的数组实现上限（N≤32），手动补零
impl Default for LoopInfo64 {
    fn default() -> Self {
        LoopInfo64 {
            lo_device: 0, lo_inode: 0, lo_rdevice: 0, lo_offset: 0, lo_sizelimit: 0,
            lo_number: 0, lo_encrypt_type: 0, lo_encrypt_key_size: 0, lo_flags: 0,
            lo_file_name: [0; 64], lo_crypt_name: [0; 64], lo_encrypt_key: [0; 32],
            lo_init: [0; 2],
        }
    }
}

#[repr(C)]
struct LoopConfig {
    fd: u32,
    block_size: u32,
    info: LoopInfo64,
    reserved: [u64; 8],
}

/// squashfs 挂载：LOOP_CTL_GET_FREE 取空闲 loop 设备，LOOP_CONFIGURE 一次
/// ioctl 原子完成 attach + 只读标记（内核 ≥5.8）。initramfs 不携带 mount
/// 二进制（squashfs/loop 模块由 /etc/modules 显式加载），失败只能重试扫描
fn mount_squashfs() -> bool {
    use nix::fcntl::{open, OFlag};
    use nix::mount::{mount, umount, MsFlags};
    use nix::sys::stat::Mode;

    // 兜掉上次失败尝试可能的残留挂载
    let _ = umount(IMAGE_DIR);

    let attach_via_loop = (|| -> Option<String> {
        use std::os::fd::AsRawFd;
        let ctrl = open("/dev/loop-control", OFlag::O_RDWR, Mode::empty()).ok()?;
        let free = unsafe { libc::ioctl(ctrl.as_raw_fd(), LOOP_CTL_GET_FREE) };
        if free < 0 {
            return None;
        }
        let loop_path = format!("/dev/loop{free}");
        if !Path::new(&loop_path).exists() {
            let _ = mknod(loop_path.as_str(), SFlag::S_IFBLK, Mode::from_bits_truncate(0o600),
                nix::sys::stat::makedev(LOOP_MAJOR, free as libc::dev_t));
        }
        let backing = open(SQUASHFS_FILE, OFlag::O_RDONLY, Mode::empty()).ok()?;
        let cfg = LoopConfig {
            fd: backing.as_raw_fd() as u32,
            block_size: 0,
            info: LoopInfo64 { lo_flags: LO_FLAGS_READ_ONLY, ..Default::default() },
            reserved: [0; 8],
        };
        let loop_fd = open(loop_path.as_str(), OFlag::O_RDWR, Mode::empty()).ok()?;
        let ok = unsafe {
            libc::ioctl(loop_fd.as_raw_fd(), LOOP_CONFIGURE, &cfg as *const LoopConfig)
        } == 0;
        if !ok {
            unsafe { libc::ioctl(loop_fd.as_raw_fd(), LOOP_CLR_FD, 0) };
            return None;
        }
        Some(loop_path)
    })();

    let Some(loop_path) = attach_via_loop else {
        return false;
    };

    let result = mount::<str, str, str, str>(Some(&loop_path), IMAGE_DIR, Some("squashfs"),
        MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NODEV, None);
    if result.is_err() {
        // 失败时 detach loop，避免残留绑定阻塞下次尝试
        if let Ok(lfd) = open(loop_path.as_str(), OFlag::O_RDWR, Mode::empty()) {
            use std::os::fd::AsRawFd;
            unsafe { libc::ioctl(lfd.as_raw_fd(), LOOP_CLR_FD, 0) };
        }
        return false;
    }
    true
}

// ── Phase 6: Verify Image ───────────────────────────────────────────────

fn verify_image() -> anyhow::Result<()> {
    match fs::metadata(IMAGE_FILE) {
        Ok(m) if m.is_file() && m.len() > 0 => Ok(()),
        Ok(_) => bail!("image.img empty or not a regular file"),
        Err(e) => bail!("image.img not found in squashfs: {e}"),
    }
}