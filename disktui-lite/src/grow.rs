//! grow.rs — dd 后末分区自动扩容：策略加载 → 只读分析 → sfdisk 执行 → fs 扩展
//!
//! 分层契约：
//! - 分析层只读（分区表/GPT header/fs 魔数），空间算术只在分析层发生一次，
//!   产出 immutable expected 值；执行层只比对消费、禁止重新推导布局。
//!   例外（文档明载）：swap 手术的精确扇区算术依赖 relocate 后重读的
//!   last_usable_lba（dd 后盘上该字段是镜像旧尺寸的过期值）。
//! - sfdisk 是唯一分区表写入者；swap 是唯一允许删除重建的分区（易失可重建）。
//! - 五态模型 Disabled/Skipped/Expanded/Partial/Failed，划界原则 =
//!   是否发生持久分区表变更（变更一旦发生，后续失败不得降级为 Skipped）。
//! - IPC：status/result 原子写（tmp+rename）；exit 0 恒成立，
//!   结果只由 /run/grow.result 表达。

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

#[cfg(target_os = "linux")]
use crate::init::BOOT_MEDIA_DIR;
#[cfg(not(target_os = "linux"))]
const BOOT_MEDIA_DIR: &str = "/media/cdrom";

// ── 常量 ────────────────────────────────────────────────────────────────
pub const STATUS_FILE: &str = "/run/grow.status";
pub const RESULT_FILE: &str = "/run/grow.result";
pub const LOG_FILE: &str = "/run/grow.log";
const GROW_MNT: &str = "/tmp/.growmnt";
const SECTOR: u64 = 512;
/// NoUsefulSpace 阈值：尾部空闲 < 1MiB 视为无可扩空间
const MIN_FREE_SECTORS: u64 = 2048;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const POLL_TRIES: u32 = 100;
/// hang 超时：grow.status mtime 停滞且子进程仍在运行 → Failed。
/// 依据 = max(600s, Gate 4 实测最慢工具耗时 × 3) 的下界
pub const HANG_TIMEOUT: Duration = Duration::from_secs(600);

// ── 策略层 ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum PartSpec {
    Auto,
    Number(u32),
}

#[derive(Debug, Clone)]
pub struct GrowPolicy {
    pub enabled: bool,
    pub part: PartSpec,
}

impl Default for GrowPolicy {
    fn default() -> Self {
        Self { enabled: false, part: PartSpec::Auto }
    }
}

/// grow.conf 与工具同住 ISO 根 /grow/，随 fast path 每次构建注入；
/// 路径复用 init.rs 既有安装介质挂载点（与 image.squashfs 同源），
/// 不为 grow 自建介质发现逻辑；介质缺席 → disabled
pub fn load_policy() -> GrowPolicy {
    load_policy_from(&Path::new(BOOT_MEDIA_DIR).join("grow/grow.conf"))
}

/// 解析契约：未知键忽略；已知键缺失用默认；值非法回退 auto（fail-open 到安全默认）
pub fn load_policy_from(path: &Path) -> GrowPolicy {
    let mut policy = GrowPolicy::default();
    let Ok(content) = fs::read_to_string(path) else {
        return policy;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "enabled" => policy.enabled = v.trim() == "1",
            "part" => match v.trim() {
                "auto" | "" => policy.part = PartSpec::Auto,
                n => match n.parse::<u32>() {
                    Ok(num) => policy.part = PartSpec::Number(num),
                    Err(_) => log_line(&format!("grow.conf: invalid part '{n}', fallback auto")),
                },
            },
            unknown => log_line(&format!("grow.conf: unknown key '{unknown}' ignored")),
        }
    }
    // grow=off 内核参数逃生门
    if let Ok(cmdline) = fs::read_to_string("/proc/cmdline")
        && cmdline.split_whitespace().any(|t| t == "grow=off")
    {
        policy.enabled = false;
    }
    policy
}

/// TUI 快速判定：是否需要 spawn --grow 子进程
pub fn quick_enabled() -> bool {
    load_policy().enabled
}

// ── 分析层（只读，纯 Rust，零依赖） ─────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Label {
    Gpt,
    Mbr,
    /// 无分区表（superfloppy）
    None,
}

#[derive(Debug, Clone)]
pub struct PartEntry {
    pub num: u32,
    pub first_lba: u64,
    pub last_lba: u64,
    pub is_container: bool,
    /// MBR: 十六进制 type 字节（"82"）；GPT: type GUID 规范形式。
    /// sfdisk 重建时原样回填（hex/GUID 是第一形式，免疫别名解析）
    pub ptype: String,
    /// GPT unique GUID（PARTUUID）；MBR 为 None
    pub partuuid: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PartTable {
    pub label: Label,
    pub entries: Vec<PartEntry>,
    /// GPT: (条目数, 条目字节数)——推导 backup 结构保留区，不硬编码 33
    pub gpt_meta: Option<(u32, u32)>,
    /// GPT header.last_usable_lba。注意：dd 后盘上这是镜像旧尺寸的过期值，
    /// 仅手术精确算术使用且须 relocate 后重读
    pub gpt_last_usable_lba: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FsKind {
    Ext,
    Xfs,
    Ntfs,
    Btrfs,
    Swap,
    Luks,
    Lvm,
    Fat,
    Exfat,
    Iso9660,
    Unknown,
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}
/// GUID → 规范字符串（前 3 组小端，后 2 组大端）
fn guid_str(b: &[u8]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6],
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

fn read_at(dev: &mut dyn ReadSeek, offset: u64, buf: &mut [u8]) -> usize {
    if dev.seek(SeekFrom::Start(offset)).is_err() {
        return 0;
    }
    let mut filled = 0;
    while filled < buf.len() {
        match dev.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => break,
        }
    }
    filled
}

/// 解析分区表：GPT（LBA1 "EFI PART" + 条目区）/ MBR（446 起 4 条目）/ superfloppy。
/// 0x55AA 缺失或 4 条目全空 → Label::None
pub fn parse_table(dev: &mut dyn ReadSeek) -> Option<PartTable> {
    let mut lba0 = [0u8; 512];
    if read_at(dev, 0, &mut lba0) < 512 {
        return None;
    }
    if &lba0[510..512] != b"\x55\xaa" {
        return Some(PartTable { label: Label::None, entries: vec![], gpt_meta: None, gpt_last_usable_lba: None });
    }

    let mut entries = Vec::new();
    for i in 0..4u32 {
        let e = &lba0[446 + i as usize * 16..462 + i as usize * 16];
        let ptype = e[4];
        if ptype == 0 {
            continue;
        }
        let start = le32(&e[8..12]) as u64;
        let sectors = le32(&e[12..16]) as u64;
        if sectors == 0 {
            continue;
        }
        entries.push(PartEntry {
            num: i + 1,
            first_lba: start,
            last_lba: start + sectors - 1,
            is_container: ptype == 0x05 || ptype == 0x0f,
            ptype: format!("{:02x}", ptype),
            partuuid: None,
        });
    }

    // GPT 判定：protective MBR（type 0xEE）
    if entries.iter().any(|e| e.ptype == "ee") {
        let mut lba1 = [0u8; 512];
        if read_at(dev, SECTOR, &mut lba1) == 512 && &lba1[0..8] == b"EFI PART" {
            let entry_lba = le64(&lba1[72..80]);
            let num_entries = le32(&lba1[80..84]);
            let entry_size = le32(&lba1[84..88]).max(128);
            let last_usable = le64(&lba1[64..72]);
            let mut gpt_entries = Vec::new();
            let mut buf = vec![0u8; entry_size as usize];
            for i in 0..num_entries {
                let off = entry_lba * SECTOR + i as u64 * entry_size as u64;
                if read_at(dev, off, &mut buf) < entry_size as usize {
                    break;
                }
                if buf[0..16].iter().all(|&b| b == 0) {
                    continue; // 未使用条目
                }
                let first = le64(&buf[32..40]);
                let last = le64(&buf[40..48]);
                if last < first {
                    continue;
                }
                gpt_entries.push(PartEntry {
                    num: i + 1,
                    first_lba: first,
                    last_lba: last,
                    is_container: false,
                    ptype: guid_str(&buf[0..16]),
                    partuuid: Some(guid_str(&buf[16..32])),
                });
            }
            return Some(PartTable {
                label: Label::Gpt,
                entries: gpt_entries,
                gpt_meta: Some((num_entries, entry_size)),
                gpt_last_usable_lba: Some(last_usable),
            });
        }
    }

    if entries.is_empty() {
        return Some(PartTable { label: Label::None, entries: vec![], gpt_meta: None, gpt_last_usable_lba: None });
    }
    Some(PartTable { label: Label::Mbr, entries, gpt_meta: None, gpt_last_usable_lba: None })
}

fn magic(buf: &[u8], off: usize, m: &[u8]) -> bool {
    buf.len() >= off + m.len() && &buf[off..off + m.len()] == m
}

/// Read + Seek 的组合 trait（trait object 不允许多个非 auto trait）
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// fs 魔数初筛。API 约束： sniff 结果不是"可执行 resize"的充分条件——
/// 最终判定由执行层的工具级验证完成（e2fsck / mount -t xfs / ntfsresize -n）
pub fn sniff_fs(dev: &mut dyn ReadSeek, part_offset: u64) -> FsKind {
    // 0x10048 覆盖 btrfs 魔数 @0x10040（superblock 副本 0x10000 + 0x40）
    let mut buf = vec![0u8; 0x10048];
    let n = read_at(dev, part_offset, &mut buf);
    let buf = &buf[..n];

    if magic(buf, 0, b"XFSB") {
        return FsKind::Xfs;
    }
    if magic(buf, 0x438, &[0x53, 0xef]) {
        return FsKind::Ext;
    }
    if magic(buf, 3, b"NTFS    ") {
        return FsKind::Ntfs;
    }
    if magic(buf, 0x10040, b"_BHRfS_M") {
        return FsKind::Btrfs;
    }
    if magic(buf, 0, b"LUKS\xBA\xBE") {
        return FsKind::Luks;
    }
    if magic(buf, 512, b"LABELONE") {
        return FsKind::Lvm;
    }
    if magic(buf, 3, b"EXFAT") {
        return FsKind::Exfat;
    }
    if magic(buf, 0x36, b"FAT") || magic(buf, 0x52, b"FAT") {
        return FsKind::Fat;
    }
    if magic(buf, 4086, b"SWAPSPACE2") || magic(buf, 8186, b"SWAPSPACE2") {
        return FsKind::Swap;
    }
    if magic(buf, 0x8001, b"CD001") {
        return FsKind::Iso9660;
    }
    FsKind::Unknown
}

/// btrfs 多设备判定：超级块 @0x10000 的 num_devices 字段（相对偏移 0x88，u64 LE，
/// 官方 On-disk Format 文档核实）。>1 表示多设备/RAID——`resize max` 语义按 devid
/// 分配，不能一次到位，跳过（分区扩容本身安全，fs 层留给用户手动处理）
fn btrfs_multi_device(dev: &mut dyn ReadSeek, part_offset: u64) -> bool {
    let mut sb = [0u8; 0x90];
    read_at(dev, part_offset + 0x10000, &mut sb) == 0x90 && le64(&sb[0x88..0x90]) > 1
}

/// swap 头（v1）：UUID @1036、label @1052，魔数在页尾（4086/8186 按页大小）
pub struct SwapInfo {
    pub uuid: String,
    pub label: String,
}

pub fn read_swap_info(dev: &mut dyn ReadSeek, part_offset: u64) -> Option<SwapInfo> {
    let mut page = [0u8; 8192];
    let n = read_at(dev, part_offset, &mut page);
    if !(magic(&page[..n], 4086, b"SWAPSPACE2") || magic(&page[..n], 8186, b"SWAPSPACE2")) {
        return None;
    }
    let uuid = page[1036..1052].iter().map(|b| format!("{b:02x}")).collect::<String>();
    let label_end = page[1052..1068].iter().position(|&b| b == 0).unwrap_or(16);
    let label = String::from_utf8_lossy(&page[1052..1052 + label_end]).trim().to_string();
    Some(SwapInfo { uuid, label })
}

/// superfloppy 的 fs 末尾估算（NoUsefulSpace 判定用；不可得 → None → 直接执行，工具幂等）
fn fs_end_bytes(dev: &mut dyn ReadSeek, offset: u64) -> Option<u64> {
    let mut sb = [0u8; 1024];
    let n = read_at(dev, offset, &mut sb);
    let sb = &sb[..n];
    if sb.len() < 0x40 {
        return None;
    }
    // btrfs：超级块 @+0x10000（不在开头 1KB 内），total_bytes u64@0x70（官方 On-disk Format）
    let mut bsb = [0u8; 0x78];
    if read_at(dev, offset + 0x10000, &mut bsb) == 0x78 && magic(&bsb, 0x40, b"_BHRfS_M") {
        return Some(offset + le64(&bsb[0x70..0x78]));
    }
    if magic(sb, 3, b"NTFS    ") {
        // NTFS BPB：bytes/sector @0x0B, sectors/cluster @0x0D, total sectors @0x28
        let bps = u16::from_le_bytes([sb[0x0b], sb[0x0c]]) as u64;
        let spc = sb[0x0d] as u64;
        let total = le64(&sb[0x28..0x30]);
        let total = if total == 0 { le32(&sb[0x13..0x17]) as u64 } else { total };
        return (bps > 0 && spc > 0).then(|| bps * spc * total);
    }
    if magic(sb, 0, b"XFSB") {
        // XFS：blocksize u32@4, dblocks u64@8
        let bs = le32(&sb[4..8]) as u64;
        let dblocks = le64(&sb[8..16]);
        return (bs > 0).then(|| bs * dblocks);
    }
    // ext：superblock @fs+1024，魔数 @+0x438 已由 sniff 确认
    let mut esb = [0u8; 1024];
    let n = read_at(dev, offset + 1024, &mut esb);
    let esb = &esb[..n];
    if esb.len() >= 0x160 && magic(esb, 0x38, &[0x53, 0xef]) {
        let log_bs = le32(&esb[0x18..0x1c]) as u64;
        let blocks_lo = le32(&esb[0x4..0x8]) as u64;
        let incompat = le32(&esb[0x60..0x64]);
        let blocks = if incompat & 0x80 != 0 {
            blocks_lo | ((le32(&esb[0x150..0x154]) as u64) << 32)
        } else {
            blocks_lo
        };
        return Some(blocks * (1024u64 << log_bs));
    }
    None
}

// ── 分析：GrowPlan ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SurgeryPlan {
    pub swap_num: u32,
    pub root_num: u32,
    pub root_first_lba: u64,
    pub root_ptype: String,
    pub swap_first_lba: u64,
    pub swap_sectors: u64,
    pub swap_ptype: String,
    pub swap_uuid: String,
    pub swap_label: String,
    /// GPT PARTUUID（MBR 为 None）
    pub swap_partuuid: Option<String>,
}

#[derive(Debug, Clone)]
pub enum GrowAction {
    /// 常规路径：sfdisk -N ", +" 原地扩末分区
    PartitionGrow {
        part_num: u32,
        part_dev: String,
        fs: FsKind,
        surgery: Option<SurgeryPlan>,
        /// 非手术：分析层一次性算出的期望新尺寸（扇区，max 值；
        /// /sys 比对时扣除 `, +` 的对齐容差 2048）。手术路径为 None（精确值
        /// 依赖 relocate 后重读 header，属文档明载的分析层例外）
        expected_new_sectors: Option<u64>,
        old_sectors: u64,
        is_gpt: bool,
    },
    /// superfloppy：无分区表——只扩 fs，不碰 sfdisk，/sys 分区判据不适用
    FilesystemOnly {
        fs: FsKind,
        fs_dev: String,
    },
}

#[derive(Debug, Clone)]
pub struct GrowPlan {
    pub action: Option<GrowAction>,
    pub skip_reason: Option<String>,
}

fn unsupported_reason(fs: FsKind) -> String {
    match fs {
        FsKind::Luks => "LUKS encryption not supported".into(),
        FsKind::Fat | FsKind::Exfat => "FAT/exFAT cannot be resized in place".into(),
        FsKind::Iso9660 => "ISO9660 filesystem".into(),
        FsKind::Swap => "swap is the only partition".into(),
        FsKind::Unknown => "unknown filesystem".into(),
        _ => "unsupported filesystem".into(),
    }
}

/// 分区设备名：nvme/mmcblk 带 p 前缀（nvme0n1p3），其余直接拼接（sda3）
pub fn part_dev_name(disk_name: &str, num: u32) -> String {
    if disk_name.starts_with("nvme") || disk_name.starts_with("mmcblk") {
        format!("{disk_name}p{num}")
    } else {
        format!("{disk_name}{num}")
    }
}

fn disk_name_of(disk_dev: &str) -> String {
    disk_dev.rsplit('/').next().unwrap_or(disk_dev).to_string()
}

/// analyze 入口：读 /sys 设备尺寸后委托 analyze_with
pub fn analyze(disk_dev: &str, policy: &GrowPolicy) -> GrowPlan {
    let name = disk_name_of(disk_dev);
    let device_sectors = read_sys_block_size(&name).unwrap_or(0);
    if device_sectors == 0 {
        return GrowPlan { action: None, skip_reason: Some("cannot read device size".into()) };
    }
    analyze_with(Path::new(disk_dev), &name, device_sectors, policy)
}

pub fn analyze_with(dev: &Path, disk_name: &str, device_sectors: u64, policy: &GrowPolicy) -> GrowPlan {
    let skip = |r: &str| GrowPlan { action: None, skip_reason: Some(r.to_string()) };
    let Ok(mut f) = File::open(dev) else {
        return skip("cannot open device");
    };
    let Some(table) = parse_table(&mut f) else {
        return skip("cannot parse partition table");
    };

    // superfloppy：直达 fs 扩容，无分区步骤
    if table.label == Label::None {
        let fs = sniff_fs(&mut f, 0);
        return match fs {
            FsKind::Ext | FsKind::Xfs | FsKind::Ntfs | FsKind::Btrfs | FsKind::Lvm => {
                if fs == FsKind::Btrfs && btrfs_multi_device(&mut f, 0) {
                    return skip("btrfs multi-device filesystem");
                }
                if let Some(fs_end) = fs_end_bytes(&mut f, 0)
                    && fs_end + MIN_FREE_SECTORS * SECTOR >= device_sectors * SECTOR
                {
                    return skip("no free space after filesystem");
                }
                GrowPlan {
                    action: Some(GrowAction::FilesystemOnly { fs, fs_dev: dev.display().to_string() }),
                    skip_reason: None,
                }
            }
            other => skip(&unsupported_reason(other)),
        };
    }

    if table.entries.is_empty() {
        return skip("no partitions");
    }

    let mut sorted = table.entries.clone();
    sorted.sort_by_key(|e| e.last_lba);
    let last = sorted.last().unwrap().clone();

    // NoUsefulSpace（所有空间算术只在此发生一次）：
    // usable_end = 设备物理尾界 −（GPT：relocate 后 backup 结构保留区；MBR：32-bit LBA 上限）
    let usable_end = match table.label {
        Label::Gpt => {
            let (n, esz) = table.gpt_meta.unwrap_or((128, 128));
            let reserved = 1 + (n as u64 * esz as u64).div_ceil(SECTOR);
            device_sectors.saturating_sub(reserved)
        }
        _ => device_sectors.min(1u64 << 32),
    };
    let free = usable_end.saturating_sub(last.last_lba + 1);
    if free < MIN_FREE_SECTORS {
        return skip("no free space after last partition");
    }

    // 候选选择（最高级不变量"绝不移动有持久数据的分区"的直接推论）：
    // 可扩集合 = {末分区} ∪ {末分区=swap 时的倒数第二分区}
    let last_fs = sniff_fs(&mut f, last.first_lba * SECTOR);
    if last_fs == FsKind::Btrfs && btrfs_multi_device(&mut f, last.first_lba * SECTOR) {
        return skip("btrfs multi-device filesystem");
    }
    let (candidate, surgery) = if last_fs == FsKind::Swap {
        let Some(prev) = (sorted.len() >= 2).then(|| sorted[sorted.len() - 2].clone()) else {
            return skip("swap is the only partition");
        };
        if prev.is_container {
            return skip("MBR logical/extended not supported in v1");
        }
        let prev_fs = sniff_fs(&mut f, prev.first_lba * SECTOR);
        if prev_fs == FsKind::Btrfs && btrfs_multi_device(&mut f, prev.first_lba * SECTOR) {
            return skip("btrfs multi-device filesystem");
        }
        if !matches!(prev_fs, FsKind::Ext | FsKind::Xfs | FsKind::Ntfs | FsKind::Btrfs | FsKind::Lvm) {
            return skip("swap last, no growable partition before it");
        }
        let Some(si) = read_swap_info(&mut f, last.first_lba * SECTOR) else {
            return skip("cannot read swap header");
        };
        let plan = SurgeryPlan {
            swap_num: last.num,
            root_num: prev.num,
            root_first_lba: prev.first_lba,
            root_ptype: prev.ptype.clone(),
            swap_first_lba: last.first_lba,
            swap_sectors: last.last_lba - last.first_lba + 1,
            swap_ptype: last.ptype.clone(),
            swap_uuid: si.uuid,
            swap_label: si.label,
            swap_partuuid: last.partuuid.clone(),
        };
        (prev, Some(plan))
    } else if last.is_container {
        return skip("MBR logical/extended not supported in v1");
    } else if matches!(last_fs, FsKind::Ext | FsKind::Xfs | FsKind::Ntfs | FsKind::Btrfs | FsKind::Lvm) {
        (last.clone(), None)
    } else {
        return skip(&unsupported_reason(last_fs));
    };

    // 声明式指定 = "候选指定"：必须命中自动候选，否则服从安全判定
    if let PartSpec::Number(n) = policy.part
        && n != candidate.num
    {
        return skip(&format!("partition {n} is not the growth candidate (candidate: partition {})", candidate.num));
    }

    let old_sectors = candidate.last_lba - candidate.first_lba + 1;
    let expected_new_sectors = surgery.is_none().then(|| usable_end - candidate.first_lba);

    GrowPlan {
        action: Some(GrowAction::PartitionGrow {
            part_num: candidate.num,
            part_dev: format!("/dev/{}", part_dev_name(disk_name, candidate.num)),
            fs: match surgery {
                Some(_) => sniff_fs(&mut f, candidate.first_lba * SECTOR), // 手术目标 fs（倒数第二分区）
                None => last_fs,
            },
            surgery,
            expected_new_sectors,
            old_sectors,
            is_gpt: table.label == Label::Gpt,
        }),
        skip_reason: None,
    }
}

// ── 执行层（--grow 子进程） ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    Disabled,
    Expanded,
    Skipped,
    Partial,
    Failed,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Disabled => "disabled",
            Status::Expanded => "expanded",
            Status::Skipped => "skipped",
            Status::Partial => "partial",
            Status::Failed => "failed",
        }
    }
}

// 工具路径（存在性守卫与 spawn 用同一来源）。工具不进 initramfs，随 fast path
// 注入 ISO 根 /grow/；挂载仅 MS_RDONLY 不含 noexec，静态 musl 二进制可直接执行
const SFDISK: &str = "/media/cdrom/grow/sfdisk";
const MKSWAP: &str = "/media/cdrom/grow/mkswap";
const PARTX: &str = "/media/cdrom/grow/partx";
const E2FSCK: &str = "/media/cdrom/grow/e2fsck";
const RESIZE2FS: &str = "/media/cdrom/grow/resize2fs";
const XFS_GROWFS: &str = "/media/cdrom/grow/xfs_growfs";
const NTFSRESIZE: &str = "/media/cdrom/grow/ntfsresize";
const BTRFS: &str = "/media/cdrom/grow/btrfs";
/// LVM2 多调用二进制：统一以 `lvm <子命令>` 形式调用（pvresize/lvs/vgchange/lvextend）
const LVM: &str = "/media/cdrom/grow/lvm";

/// 原子写（write-to-tmp + rename，同 /run tmpfs 内 rename 原子），
/// 杜绝 TUI 读到 truncate 后的空帧/半帧
fn atomic_write(path: &str, content: &str) {
    let tmp = format!("{path}.tmp");
    if let Ok(mut f) = File::create(&tmp)
        && f.write_all(content.as_bytes()).is_ok()
    {
        let _ = f.sync_all();
        let _ = fs::rename(&tmp, path);
        return;
    }
    let _ = fs::write(path, content); // /run 不可写时的最后尝试（结果由 TUI crash 判定兜底）
}

/// grow.status 限定固定 phase 枚举：analyze / partition / kernel-reread / filesystem / done
fn write_phase(phase: &str) {
    atomic_write(STATUS_FILE, &format!("phase={phase}\n"));
}

fn log_line(msg: &str) {
    use std::io::Write as _;
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(LOG_FILE) {
        let _ = writeln!(f, "[{}] {msg}", SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
    }
}

struct GrowCtx {
    disk: String,
    disk_name: String,
    start: Instant,
}

impl GrowCtx {
    fn new(disk: &str) -> Self {
        Self { disk: disk.to_string(), disk_name: disk_name_of(disk), start: Instant::now() }
    }
    fn log(&self, msg: &str) {
        log_line(msg);
    }
    fn elapsed(&self) -> u64 {
        self.start.elapsed().as_secs()
    }

    /// 终态：写 result 后 exit 0（exit status 只表达进程健康，结果只看 result 文件）
    fn finish(&self, status: Status, reason: &str, manual_cmd: &str, old_bytes: u64, new_bytes: u64) -> ! {
        write_phase("done");
        let mut lines = vec![format!("status={}", status.as_str())];
        lines.push(format!("device={}", self.disk));
        if !reason.is_empty() {
            lines.push(format!("reason={reason}"));
        }
        if !manual_cmd.is_empty() {
            lines.push(format!("manual_cmd={manual_cmd}"));
        }
        if old_bytes > 0 {
            lines.push(format!("old_bytes={old_bytes}"));
        }
        if new_bytes > 0 {
            lines.push(format!("new_bytes={new_bytes}"));
        }
        atomic_write(RESULT_FILE, &format!("{}\n", lines.join("\n")));
        self.log(&format!("finish: status={} reason={reason}", status.as_str()));
        std::process::exit(0);
    }

    /// 工具执行：stdout/stderr 记入 grow.log。None = spawn 失败（基础设施故障）
    fn run(&self, tool: &str, args: &[&str], stdin_data: Option<&str>) -> Option<i32> {
        let mut cmd = Command::new(tool);
        cmd.args(args);
        if stdin_data.is_some() {
            cmd.stdin(Stdio::piped());
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.log(&format!("spawn {tool} failed: {e}"));
                return None;
            }
        };
        if let Some(data) = stdin_data
            && let Some(mut si) = child.stdin.take()
        {
            let _ = si.write_all(data.as_bytes());
        }
        let out = child.wait_with_output().ok()?;
        let code = out.status.code().unwrap_or(-1);
        self.log(&format!("{tool} exit={code} (t={}s)", self.elapsed()));
        let so = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !so.is_empty() {
            self.log(&format!("{tool} stdout: {so}"));
        }
        if !se.is_empty() {
            self.log(&format!("{tool} stderr: {se}"));
        }
        Some(code)
    }

    /// 工具执行并返回 stdout（LVM 的 VG/LV 元数据发现）。None = spawn 失败
    fn run_capture(&self, tool: &str, args: &[&str]) -> Option<(i32, String)> {
        let out = Command::new(tool).args(args).output().ok()?;
        let code = out.status.code().unwrap_or(-1);
        self.log(&format!("{tool} exit={code} (t={}s)", self.elapsed()));
        let so = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !so.is_empty() {
            self.log(&format!("{tool} stdout: {so}"));
        }
        if !se.is_empty() {
            self.log(&format!("{tool} stderr: {se}"));
        }
        Some((code, so))
    }
}

fn read_sys_block_size(disk_name: &str) -> Option<u64> {
    fs::read_to_string(format!("/sys/block/{disk_name}/size")).ok()?.trim().parse().ok()
}

fn sysfs_part_size(disk_name: &str, part_num: u32) -> Option<u64> {
    let part = part_dev_name(disk_name, part_num);
    fs::read_to_string(format!("/sys/block/{disk_name}/{part}/size")).ok()?.trim().parse().ok()
}

/// 分区变更最终判据 = /sys 实际尺寸（sfdisk 成功 ≠ 内核已暴露新尺寸）。
/// 轮询 100ms×100 → 未达 → partx 兜底（节点缺失 -a / 尺寸不符 -u）→ 再轮询 → 仍未达 → false
fn wait_partition_visible(ctx: &GrowCtx, part_num: u32, min_size: u64) -> bool {
    let check = |min: u64| sysfs_part_size(&ctx.disk_name, part_num).is_some_and(|s| s >= min);

    for _ in 0..POLL_TRIES {
        if check(min_size) {
            return true;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    // 兜底：节点缺失 → partx -a（读盘添加缺失分区）；尺寸不符 → partx -u（更新已存在分区）
    let node_missing = sysfs_part_size(&ctx.disk_name, part_num).is_none();
    let mode = if node_missing { "-a" } else { "-u" };
    ctx.log(&format!("kernel reread incomplete, trying partx {mode}"));
    let _ = ctx.run(PARTX, &[mode, &ctx.disk], None);
    for _ in 0..POLL_TRIES {
        if check(min_size) {
            return true;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    false
}

/// 读 GPT header（LBA1）字段：Some((last_usable_lba, (条目数, 条目尺寸)))
fn read_gpt_header(disk: &str) -> Option<(u64, (u32, u32))> {
    let mut f = File::open(disk).ok()?;
    let mut lba1 = [0u8; 512];
    if read_at(&mut f, SECTOR, &mut lba1) < 512 || &lba1[0..8] != b"EFI PART" {
        return None;
    }
    Some((le64(&lba1[64..72]), (le32(&lba1[80..84]), le32(&lba1[84..88]))))
}

/// backup header 是否已在设备末端标准位（relocate 成功/无需迁移的判据）
/// 签名 + my_lba 双重校验（GPT 规范：my_lba @header+24 指向 header 自身）：
/// 排除设备尾部残留旧 GPT 签名（my_lba 指向别处）造成的误判
fn backup_header_at_end(disk: &str, device_sectors: u64) -> bool {
    let Ok(mut f) = File::open(disk) else { return false };
    let mut tail = [0u8; 512];
    let off = device_sectors.saturating_sub(1) * SECTOR;
    read_at(&mut f, off, &mut tail) == 512
        && &tail[0..8] == b"EFI PART"
        && le64(&tail[24..32]) == device_sectors.saturating_sub(1)
}

/// 工具存在性守卫（模板 initramfs 与 grow.conf 不匹配时的安全网）。
/// 返回 None = 齐备；Some(reason) = Skipped 原因
fn tools_missing(fs: FsKind, need_surgery: bool) -> Option<String> {
    let mut missing: Vec<&str> = vec![];
    if need_surgery {
        for t in [SFDISK, MKSWAP, PARTX] {
            if !Path::new(t).exists() {
                missing.push(t);
            }
        }
    }
    let fs_tools: &[&str] = match fs {
        FsKind::Ext => &[E2FSCK, RESIZE2FS],
        FsKind::Xfs => &[XFS_GROWFS],
        FsKind::Ntfs => &[NTFSRESIZE],
        FsKind::Btrfs => &[BTRFS],
        FsKind::Lvm => &[LVM],
        _ => &[],
    };
    for t in fs_tools {
        if !Path::new(t).exists() {
            missing.push(t);
        }
    }
    (!missing.is_empty()).then(|| format!("tools not bundled: {}", missing.join(", ")))
}

/// fs 扩容分发。Err(reason) = 工具拒绝或执行失败（归因由调用方按 mutation 状态定级）
fn resize_fs(ctx: &GrowCtx, fs: FsKind, target: &str) -> Result<(), String> {
    match fs {
        FsKind::Ext => {
            let Some(code) = ctx.run(E2FSCK, &["-fp", target], None) else {
                return Err("e2fsck spawn failed".into());
            };
            // 显式白名单 0..=2，禁止 4+——防未来退出码扩展自动放行
            if !matches!(code, 0..=2) {
                return Err(format!("e2fsck rejected (exit {code}, filesystem inconsistent)"));
            }
            match ctx.run(RESIZE2FS, &[target], None) {
                Some(0) => Ok(()),
                Some(c) => Err(format!("resize2fs failed (exit {c})")),
                None => Err("resize2fs spawn failed".into()),
            }
        }
        FsKind::Xfs => {
            // 防御纵深：boot 期模块列表通常已加载 xfs，此处不依赖该隐式前提；
            // 不检查返回值，mount 失败自然兜底归因
            let _ = Command::new("modprobe").arg("xfs").status();
            let _ = fs::create_dir_all(GROW_MNT);
            #[cfg(target_os = "linux")]
            {
                use nix::mount::{MsFlags, mount, umount};
                // 显式 turbofish：data=None 单独无法推断类型（与 init.rs 同模式）
                if mount::<str, str, str, str>(Some(target), GROW_MNT, Some("xfs"), MsFlags::empty(), None).is_err() {
                    return Err("XFS kernel support unavailable".into());
                }
                let grown = matches!(ctx.run(XFS_GROWFS, &["-d", GROW_MNT], None), Some(0));
                let _ = umount(GROW_MNT);
                if grown {
                    return Ok(());
                }
                return Err("xfs_growfs failed".into());
            }
            #[cfg(not(target_os = "linux"))]
            Err("XFS grow requires Linux".into())
        }
        FsKind::Ntfs => {
            // 干跑守卫：休眠/Fast Startup/BitLocker 脏卷在此拒绝
            let dry_t = ctx.elapsed();
            match ctx.run(NTFSRESIZE, &["-n", "-P", target], None) {
                Some(0) => {}
                Some(c) => return Err(format!("ntfsresize dry-run rejected (exit {c}, volume dirty or hibernated)")),
                None => return Err("ntfsresize spawn failed".into()),
            }
            ctx.log(&format!("ntfsresize dry-run ok at t={}s", dry_t));
            // 实跑 -ff：纯 flag 零交互（Clonezilla batch 实证用法），安全性由前置干跑保证
            match ctx.run(NTFSRESIZE, &["-ff", "-P", target], None) {
                Some(0) => {
                    ctx.log(&format!("ntfsresize real-run done at t={}s", ctx.elapsed()));
                    Ok(())
                }
                Some(c) => Err(format!("ntfsresize failed (exit {c})")),
                None => Err("ntfsresize spawn failed".into()),
            }
        }
        FsKind::Btrfs => {
            // 在线扩容：btrfs filesystem resize max <挂载点>（官方 man 核实；
            // 单设备由分析层 num_devices==1 保证，多设备已提前 skip）
            let _ = Command::new("modprobe").arg("btrfs").status();
            let _ = fs::create_dir_all(GROW_MNT);
            #[cfg(target_os = "linux")]
            {
                use nix::mount::{MsFlags, mount, umount};
                // 显式 turbofish：data=None 单独无法推断类型（与 init.rs 同模式）
                if mount::<str, str, str, str>(Some(target), GROW_MNT, Some("btrfs"), MsFlags::empty(), None).is_err() {
                    return Err("Btrfs kernel support unavailable".into());
                }
                let grown = matches!(ctx.run(BTRFS, &["filesystem", "resize", "max", GROW_MNT], None), Some(0));
                let _ = umount(GROW_MNT);
                if grown {
                    return Ok(());
                }
                return Err("btrfs filesystem resize failed".into());
            }
            #[cfg(not(target_os = "linux"))]
            Err("Btrfs grow requires Linux".into())
        }
        FsKind::Lvm => resize_lvm(ctx, target),
        _ => Err("unsupported filesystem".into()),
    }
}

/// LVM 扩容链：pvresize（PV 吃下分区新增空间）→ VG/LV 发现（唯一 LV 策略，
/// 避免启发式猜错目标）→ vgchange -ay（initramfs 无 udev 规则，dm 节点必须
/// 显式激活才会出现）→ lvextend -l +100%FREE → 对 dm 设备 sniff 后递归 fs 扩容。
/// 递归深度有界：LV 上只可能是 ext/xfs/btrfs（再嵌套 LVM 的病态布局不支持）
fn resize_lvm(ctx: &GrowCtx, target: &str) -> Result<(), String> {
    let _ = Command::new("modprobe").arg("dm-mod").status();
    // 静态 lvm 的运行时目录（锁文件/扫描缓存；无 udev 环境不自建则命令失败）
    let _ = fs::create_dir_all("/run/lvm");
    let _ = fs::create_dir_all("/run/lock/lvm");

    // 1) PV 扩容（分区已由调用方扩完）
    match ctx.run(LVM, &["pvresize", target], None) {
        Some(0) => {}
        Some(c) => return Err(format!("pvresize failed (exit {c})")),
        None => return Err("lvm spawn failed".into()),
    }

    // 2) PV 所属 VG
    let Some((code, vg_out)) = ctx.run_capture(LVM, &["pvs", "--noheadings", "-o", "vg_name", target]) else {
        return Err("lvm spawn failed".into());
    };
    if code != 0 {
        return Err(format!("pvs failed (exit {code})"));
    }
    let vg = vg_out.trim().to_string();
    if vg.is_empty() {
        return Err("PV not in any volume group".into());
    }

    // 3) VG 内 LV 清单：仅唯一 LV 自动扩（多 LV 目标选择是用户决策，不猜）
    let Some((code, lvs_out)) = ctx.run_capture(LVM, &["lvs", "--noheadings", "-o", "lv_name", &vg]) else {
        return Err("lvm spawn failed".into());
    };
    if code != 0 {
        return Err(format!("lvs failed (exit {code})"));
    }
    let lv_names: Vec<String> =
        lvs_out.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    if lv_names.len() != 1 {
        return Err(format!("volume group {vg} has {} logical volumes (auto-grow requires exactly 1)", lv_names.len()));
    }
    let lv = &lv_names[0];
    let lv_path = format!("/dev/{vg}/{lv}");

    // 4) 激活 VG：dm 设备节点由激活创建（无 udev 的 initramfs 必需步骤）
    match ctx.run(LVM, &["vgchange", "-ay", &vg], None) {
        Some(0) => {}
        Some(c) => return Err(format!("vgchange -ay failed (exit {c})")),
        None => return Err("lvm spawn failed".into()),
    }

    // 5) LV 吃掉 VG 全部空闲 extent
    match ctx.run(LVM, &["lvextend", "-l", "+100%FREE", &lv_path], None) {
        Some(0) => {}
        Some(c) => return Err(format!("lvextend failed (exit {c})")),
        None => return Err("lvm spawn failed".into()),
    }

    // 6) 解析 dm 设备节点（/dev/<vg>/<lv> 是 udev 符号链接，此处不存在；
    //    dm_path 给出真实节点 /dev/mapper/<vg>-<lv>，含转义规则处理）
    let Some((code, dm_out)) = ctx.run_capture(LVM, &["lvs", "--noheadings", "-o", "dm_path", &lv_path]) else {
        return Err("lvm spawn failed".into());
    };
    if code != 0 {
        return Err(format!("lvs dm_path failed (exit {code})"));
    }
    let dm = dm_out.trim().to_string();
    if dm.is_empty() {
        return Err("cannot resolve LV device path".into());
    }

    // 7) LV 内容 fs 识别 → 递归扩容（工具守卫在此层补检）
    let Ok(mut dev) = File::open(&dm) else {
        return Err(format!("cannot open LV device {dm}"));
    };
    let lv_fs = sniff_fs(&mut dev, 0);
    if !matches!(lv_fs, FsKind::Ext | FsKind::Xfs | FsKind::Btrfs) {
        return Err(format!("LV filesystem not growable ({lv_fs:?})"));
    }
    if let Some(reason) = tools_missing(lv_fs, false) {
        return Err(format!("{reason} (LV {dm})"));
    }
    resize_fs(ctx, lv_fs, &dm)
}

/// --grow 子进程入口：每步失败即写 result 退出，永不 panic，exit 0 恒成立
pub fn run_grow(disk: &str) -> ! {
    let ctx = GrowCtx::new(disk);
    ctx.log(&format!("grow start: {disk}"));

    write_phase("analyze");
    let policy = load_policy();
    if !policy.enabled {
        // Disabled：内部态，UI 不显示任何 grow 行
        ctx.finish(Status::Disabled, "", "", 0, 0);
    }

    let Some(device_sectors) = read_sys_block_size(&ctx.disk_name) else {
        ctx.finish(Status::Failed, "cannot read device size", "", 0, 0);
    };

    let plan = analyze_with(Path::new(disk), &ctx.disk_name, device_sectors, &policy);
    let Some(action) = plan.action else {
        let reason = plan.skip_reason.unwrap_or_else(|| "not growable".into());
        ctx.finish(Status::Skipped, &reason, "", 0, 0);
    };

    match action {
        GrowAction::FilesystemOnly { fs, fs_dev } => {
            if let Some(reason) = tools_missing(fs, false) {
                ctx.finish(Status::Skipped, &reason, "", 0, 0);
            }
            write_phase("filesystem");
            // 无分区表变更：fs 失败归 Skipped
            match resize_fs(&ctx, fs, &fs_dev) {
                Ok(()) => {
                    let new_bytes = device_sectors * SECTOR;
                    ctx.finish(Status::Expanded, "", "", 0, new_bytes);
                }
                Err(reason) => ctx.finish(Status::Skipped, &reason, "", 0, 0),
            }
        }
        GrowAction::PartitionGrow { part_num, part_dev, fs, surgery, expected_new_sectors, old_sectors, is_gpt } => {
            if let Some(reason) = tools_missing(fs, true) {
                ctx.finish(Status::Skipped, &reason, "", 0, 0);
            }
            write_phase("partition");

            // GPT：relocate backup header 到设备末端（仅 GPT；MBR 跳过）
            if is_gpt {
                match ctx.run(SFDISK, &["--relocate", "gpt-bak-std", &ctx.disk], None) {
                    Some(0) => {}
                    _ => {
                        // relocate 失败归因（它是 GPT metadata 写操作，纳入"持久变更"原则）：
                        // 证实 backup 仍在原位（未持久变更）→ Skipped；状态无法证实 → Failed
                        if backup_header_at_end(&ctx.disk, device_sectors) {
                            // 已在标准位（本就无需迁移）→ 继续
                        } else if read_gpt_header(&ctx.disk).is_some() {
                            // backup 可能被半迁移（relocate 中途失败）→ 附幂等修复命令
                            let reason = format!(
                                "gpt backup header relocate failed (repair: sfdisk --relocate gpt-bak-std {})",
                                ctx.disk
                            );
                            ctx.finish(Status::Skipped, &reason, "", 0, 0);
                        } else {
                            ctx.finish(Status::Failed, "gpt relocate failed (state unknown)", "", 0, 0);
                        }
                    }
                }
            }

            if let Some(s) = surgery {
                grow_with_surgery(&ctx, device_sectors, is_gpt, &s, &part_dev, fs, old_sectors);
            } else {
                // 非手术：`, +`（start/type/UUID 全保留）。前置不变量：分析层已证明
                // target 是 end-LBA 最大可扩分区且其后无障碍——安全边界全在分析层
                let stdin = ", +\n";
                match ctx.run(SFDISK, &["-N", &part_num.to_string(), &ctx.disk], Some(stdin)) {
                    Some(0) => {}
                    Some(c) => {
                        // GPT relocate 已提交 mutation → 不得降级 Skipped；MBR 未变更 → Skipped
                        let reason = format!("sfdisk partition grow failed (exit {c})");
                        if is_gpt {
                            ctx.finish(Status::Partial, &reason, "", old_sectors * SECTOR, 0);
                        }
                        ctx.finish(Status::Skipped, &reason, "", 0, 0);
                    }
                    None => ctx.finish(Status::Failed, "sfdisk spawn failed", "", 0, 0),
                }

                write_phase("kernel-reread");
                // /sys 比对消费分析层的 expected 值；扣 `, +` 对齐容差（<1MiB，man 已核实）
                let expected = expected_new_sectors.unwrap_or(0);
                let min_size = expected.saturating_sub(MIN_FREE_SECTORS);
                if old_sectors >= min_size {
                    // 期望值未超过旧尺寸（对齐后无增长空间）→ 已是目标态
                } else if !wait_partition_visible(&ctx, part_num, min_size) {
                    // 与手术路径同档归档：分区表已持久扩容（sfdisk exit 0 已过），
                    // 卡点在内核同步 → Failed + fs 手动命令（重启后内核重读即扩，补 fs 即完成）
                    let manual = manual_cmd_for_fs(fs, &part_dev);
                    ctx.finish(Status::Failed, "kernel partition reread failed", &manual, old_sectors * SECTOR, 0);
                }

                write_phase("filesystem");
                let new_sectors = sysfs_part_size(&ctx.disk_name, part_num).unwrap_or(old_sectors);
                match resize_fs(&ctx, fs, &part_dev) {
                    Ok(()) => ctx.finish(Status::Expanded, "", "", old_sectors * SECTOR, new_sectors * SECTOR),
                    // 持久分区表变更已发生 → 不得降级 Skipped
                    Err(reason) => {
                        let manual = manual_cmd_for_fs(fs, &part_dev);
                        ctx.finish(Status::Partial, &format!("partition expanded; filesystem resize failed ({reason})"), &manual, old_sectors * SECTOR, new_sectors * SECTOR);
                    }
                }
            }
        }
    }
}

fn manual_cmd_for_fs(fs: FsKind, dev: &str) -> String {
    match fs {
        FsKind::Ext => format!("e2fsck -fp {dev} && resize2fs {dev}"),
        FsKind::Xfs => format!("mount -t xfs {dev} /mnt && xfs_growfs -d /mnt && umount /mnt"),
        FsKind::Ntfs => format!("ntfsresize {dev}"),
        FsKind::Btrfs => format!("mount -t btrfs {dev} /mnt && btrfs filesystem resize max /mnt && umount /mnt"),
        // VG/LV 名运行期才可知，占位符形式给全链路命令（顺序与 resize_lvm 实际执行链一致）
        FsKind::Lvm => format!(
            "pvresize {dev}; pvs; vgchange -ay <vg>; lvextend -l +100%FREE <vg>/<lv>; resize2fs /dev/mapper/<vg>-<lv> (xfs: mount+xfs_growfs -d, btrfs: mount+btrfs filesystem resize max)"
        ),
        _ => String::new(),
    }
}

/// swap 手术状态机：S0 原始 → S1 swap 已删 → S2 target 已扩 → S3 swap 已重建
/// → S4 fs 已扩。失败归因以状态达成为界；持久变更一旦发生不得降级 Skipped
fn grow_with_surgery(
    ctx: &GrowCtx,
    device_sectors: u64,
    is_gpt: bool,
    s: &SurgeryPlan,
    root_dev: &str,
    fs: FsKind,
    old_root_sectors: u64,
) -> ! {
    let swap_dev = format!("/dev/{}", part_dev_name(&ctx.disk_name, s.swap_num));

    // 手术精确算术：relocate 后重读 header 拿真实 last_usable_lba
    // （dd 后盘上该字段是镜像旧尺寸的过期值——顺序依赖，文档第 5 步）
    let usable_last = if is_gpt {
        match read_gpt_header(&ctx.disk) {
            Some((last_usable, _)) => last_usable,
            None => ctx.finish(Status::Failed, "cannot re-read GPT header after relocate", "", 0, 0),
        }
    } else {
        device_sectors.min(1u64 << 32) - 1
    };
    let new_swap_start = usable_last - s.swap_sectors + 1;
    let root_new_size = new_swap_start - s.root_first_lba;

    // 手动恢复命令（按失败档位生成；分析层已持有全部原值）
    let mkswap_cmd = |dev: &str| {
        let label = if s.swap_label.is_empty() {
            String::new()
        } else {
            format!(" -L {}", s.swap_label)
        };
        format!("mkswap -U {}{label} {dev}", s.swap_uuid)
    };
    let restore_partuuid = |cmd: &mut String| {
        if let Some(pu) = &s.swap_partuuid {
            cmd.push_str(&format!("; sfdisk --part-uuid {} {} {}", ctx.disk, s.swap_num, pu));
        }
    };
    // S1→S2 失败：swap 已删未重建 → sfdisk 原位重建 + PARTUUID + mkswap 组合
    let manual_s2 = {
        let mut c = format!(
            "printf '{}, {}, type={}' | sfdisk -N {} {}",
            s.swap_first_lba, s.swap_sectors, s.swap_ptype, s.swap_num, ctx.disk
        );
        restore_partuuid(&mut c);
        c.push_str("; ");
        c.push_str(&mkswap_cmd(&swap_dev));
        c
    };
    // S2→S3 及以后：分区已重建 → mkswap 全参（+ GPT PARTUUID）
    let manual_s3 = {
        let mut c = mkswap_cmd(&swap_dev);
        restore_partuuid(&mut c);
        c
    };
    let manual_s4 = manual_cmd_for_fs(fs, root_dev);

    // S0 → S1：删除 swap（失败 = 未动盘 → Skipped）
    match ctx.run(SFDISK, &["--delete", &ctx.disk, &s.swap_num.to_string()], None) {
        Some(0) => {}
        Some(c) => ctx.finish(Status::Skipped, &format!("swap delete failed (exit {c})"), "", 0, 0),
        None => ctx.finish(Status::Failed, "sfdisk spawn failed", "", 0, 0),
    }
    ctx.log("surgery S1: swap deleted");

    // S1 → S2：root 精确扇区扩容（start/type/UUID 保留，size 精确无对齐优化）
    let stdin = format!(", {root_new_size}, type={}\n", s.root_ptype);
    match ctx.run(SFDISK, &["-N", &s.root_num.to_string(), &ctx.disk], Some(&stdin)) {
        Some(0) => {}
        Some(c) => ctx.finish(
            Status::Partial,
            &format!("swap deleted; target unchanged (root expand exit {c})"),
            &manual_s2,
            0,
            0,
        ),
        None => ctx.finish(Status::Failed, "sfdisk spawn failed", &manual_s2, 0, 0),
    }
    ctx.log("surgery S2: target expanded");

    // S2 → S3：swap 尾部重建（复用原槽位 → 分区号/fstab 引用保真）
    let stdin = format!("{}, {}, type={}\n", new_swap_start, s.swap_sectors, s.swap_ptype);
    match ctx.run(SFDISK, &["-N", &s.swap_num.to_string(), &ctx.disk], Some(&stdin)) {
        Some(0) => {}
        Some(c) => ctx.finish(
            Status::Partial,
            &format!("target expanded; swap missing (recreate exit {c})"),
            &manual_s3,
            old_root_sectors * SECTOR,
            0,
        ),
        None => ctx.finish(Status::Failed, "sfdisk spawn failed", &manual_s3, old_root_sectors * SECTOR, 0),
    }
    ctx.log("surgery S3: swap partition rebuilt");

    // PARTUUID 恢复（仅 GPT；两命名空间独立，不抽象为单一 restore）
    if let Some(pu) = &s.swap_partuuid {
        match ctx.run(SFDISK, &["--part-uuid", &ctx.disk, &s.swap_num.to_string(), pu], None) {
            Some(0) => {}
            _ => ctx.finish(
                Status::Partial,
                "target expanded; swap recreation incomplete (part-uuid restore failed)",
                &manual_s3,
                old_root_sectors * SECTOR,
                0,
            ),
        }
    }

    // 内核同步最终判据：/sys 实际尺寸（root 精确值 + swap 精确值）
    write_phase("kernel-reread");
    if !wait_partition_visible(ctx, s.root_num, root_new_size)
        || !wait_partition_visible(ctx, s.swap_num, s.swap_sectors)
    {
        ctx.finish(Status::Failed, "kernel partition reread failed", &manual_s3, old_root_sectors * SECTOR, 0);
    }

    // fs UUID 恢复（失败归 S3→S4 档）
    let label_arg: Vec<String> = if s.swap_label.is_empty() {
        vec![]
    } else {
        vec!["-L".to_string(), s.swap_label.clone()]
    };
    let mut mkswap_args: Vec<String> = vec!["-U".into(), s.swap_uuid.clone()];
    mkswap_args.extend(label_arg);
    mkswap_args.push(swap_dev.clone());
    let arg_refs: Vec<&str> = mkswap_args.iter().map(|s| s.as_str()).collect();
    match ctx.run(MKSWAP, &arg_refs, None) {
        Some(0) => {}
        Some(c) => ctx.finish(
            Status::Partial,
            &format!("target expanded; swap recreation incomplete (mkswap exit {c})"),
            &manual_s3,
            old_root_sectors * SECTOR,
            root_new_size * SECTOR,
        ),
        None => ctx.finish(Status::Failed, "mkswap spawn failed", &manual_s3, old_root_sectors * SECTOR, 0),
    }
    ctx.log("surgery S3 complete: swap UUID/label restored");

    // S3 → S4：fs 扩容
    write_phase("filesystem");
    match resize_fs(ctx, fs, root_dev) {
        Ok(()) => ctx.finish(Status::Expanded, "", "", old_root_sectors * SECTOR, root_new_size * SECTOR),
        Err(reason) => ctx.finish(
            Status::Partial,
            &format!("swap rebuilt; fs resize failed ({reason})"),
            &manual_s4,
            old_root_sectors * SECTOR,
            root_new_size * SECTOR,
        ),
    }
}

// ── TUI 消费接口 ───────────────────────────────────────────────────────

/// /run/grow.result 解析结果（严格 key-value，UI 只做 presentation）
#[derive(Debug, Clone, Default)]
pub struct GrowOutcome {
    pub status: String,
    pub device: String,
    pub reason: String,
    pub manual_cmd: String,
    pub old_bytes: u64,
    pub new_bytes: u64,
}

pub fn read_result() -> Option<GrowOutcome> {
    let content = fs::read_to_string(RESULT_FILE).ok()?;
    let mut out = GrowOutcome::default();
    let mut has_status = false;
    for line in content.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "status" => {
                out.status = v.trim().to_string();
                has_status = true;
            }
            "device" => out.device = v.trim().to_string(),
            "reason" => out.reason = v.trim().to_string(),
            "manual_cmd" => out.manual_cmd = v.trim().to_string(),
            "old_bytes" => out.old_bytes = v.trim().parse().unwrap_or(0),
            "new_bytes" => out.new_bytes = v.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    has_status.then_some(out)
}

/// TUI 读取 grow.status：容忍 NotFound 与 UTF-8 错误（视为无更新）
pub fn read_status_line() -> Option<String> {
    let content = fs::read_to_string(STATUS_FILE).ok()?;
    let phase = content.lines().next()?.strip_prefix("phase=")?.trim().to_string();
    let text = match phase.as_str() {
        "analyze" => "Analyzing disk layout",
        "partition" => "Resizing partition table",
        "kernel-reread" => "Syncing kernel partition table",
        "filesystem" => "Expanding filesystem",
        "done" => "Finishing",
        _ => "Working",
    };
    Some(text.to_string())
}

/// status 文件 mtime（hang 判定：停滞超时且子进程仍在运行）
pub fn status_mtime() -> Option<SystemTime> {
    fs::metadata(STATUS_FILE).ok().and_then(|m| m.modified().ok())
}