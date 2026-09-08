//! grow.rs — dd 后自动扩容：策略加载 → 只读分析 → sfdisk 执行 → fs 扩展
//!
//! 分层契约：
//! - 分析层只读（分区表/GPT header/fs 魔数），空间算术只在分析层发生一次，
//!   产出 immutable expected 值；执行层只比对消费、禁止重新推导布局。
//!   例外：swap 手术的精确扇区算术依赖 relocate 后重读的
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

use crate::utils::{SECTOR, BOOT_MEDIA_DIR};

// ── 常量 ────────────────────────────────────────────────────────────────
pub const STATUS_FILE: &str = "/run/grow.status";
pub const RESULT_FILE: &str = "/run/grow.result";
pub const LOG_FILE: &str = "/run/grow.log";
const GROW_MNT: &str = "/tmp/.growmnt";
/// NoUsefulSpace 阈值：尾部空闲 < 1MiB 视为无可扩空间
const MIN_FREE_SECTORS: u64 = 2048;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const POLL_TRIES: u32 = 100;
/// hang 超时：grow.status mtime 停滞且子进程仍在运行 → Failed。
/// 下界 = max(600s, 实测最慢工具耗时 × 3)
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
    /// 多 LV 时唯一受益 LV 的声明（grow.conf `lv=`）。单 LV 布局无需声明：
    /// 与 part= 同构的"声明式指定"——把"空闲给谁"从猜测变为填空。
    /// thin pool 名也可声明（此时扩的是池数据区，lvm 语义即如此）
    pub lv: Option<String>,
}

impl Default for GrowPolicy {
    fn default() -> Self {
        Self { enabled: false, part: PartSpec::Auto, lv: None }
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
            "lv" => match v.trim() {
                "" | "auto" => policy.lv = None,
                name => policy.lv = Some(name.to_string()),
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
    /// GPT header.last_usable_lba。注意：dd 后盘上这是镜像旧尺寸的过期值；
    /// 运行时不消费（手术路径 relocate 后经 read_gpt_header 重读），仅供测试断言
    pub gpt_last_usable_lba: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FsKind {
    Ext,
    Xfs,
    Ntfs,
    Btrfs,
    F2fs,
    Swap,
    Luks,
    Lvm,
    Fat,
    Exfat,
    Iso9660,
    /// 只读根 squashfs（OpenWrt combined 镜像：同分区尾部 RW overlay）
    Squashfs,
    /// 只读根 EROFS overlay（fstools 支持的另一种底层；此处不扩容）
    Erofs,
    Unknown,
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}
/// XFS 磁盘结构全大端（内核 xfs_dsb 为 __be32/__be64）
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
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
/// 0x55AA 缺失或 4 条目全空 → Label::None。
/// lba_bytes = 分区表 LBA 单位（= 逻辑块大小；4Kn 盘上 LBA1 偏移 4096 而非 512）
pub fn parse_table(dev: &mut dyn ReadSeek, lba_bytes: u64) -> Option<PartTable> {
    let mut lba0 = [0u8; 512];
    if read_at(dev, 0, &mut lba0) < 512 {
        return None;
    }
    if &lba0[510..512] != b"\x55\xaa" {
        return Some(PartTable { label: Label::None, entries: vec![], gpt_meta: None, gpt_last_usable_lba: None });
    }

    let mut entries = Vec::new();
    for i in 0..4u32 {
        let base = 446 + i as usize * 16;
        let e = &lba0[base..base + 16];
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
            is_container: ptype == 0x05 || ptype == 0x0f || ptype == 0x85,
            ptype: format!("{:02x}", ptype),
            partuuid: None,
        });
    }

    // GPT 判定：protective MBR（type 0xEE）
    if entries.iter().any(|e| e.ptype == "ee") {
        let mut lba1 = [0u8; 512];
        if read_at(dev, lba_bytes, &mut lba1) == 512 && &lba1[0..8] == b"EFI PART" {
            let entry_lba = le64(&lba1[72..80]);
            let num_entries = le32(&lba1[80..84]);
            // UEFI 规范：SizeOfPartitionEntry = 128×2ⁿ；条目数无规范上界，
            // 损坏值会使逐条读取扫到设备尾——防御性上界 65536
            let entry_size = le32(&lba1[84..88]);
            if entry_size < 128 || !entry_size.is_power_of_two() || num_entries > 65536 {
                return None;
            }
            let last_usable = le64(&lba1[48..56]);
            let mut gpt_entries = Vec::new();
            // 条目内容字段全在前 128B 内（type/GUID/first/last @0..64）——
            // 无论 entry_size 多大只读固定 128B，杜绝按盘上值分配内存
            let mut buf = [0u8; 128];
            for i in 0..num_entries {
                // entry_lba/entry_size 都是盘上可控值——saturating 防溢出
                let off = entry_lba
                    .saturating_mul(lba_bytes)
                    .saturating_add((i as u64).saturating_mul(entry_size as u64));
                if read_at(dev, off, &mut buf) < 128 {
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
    // F2FS 魔数 0xF2F52010，superblock 位于分区偏移 0x400，魔数在 superblock 首 4 字节（小端）
    if magic(buf, 0x400, &[0x10, 0x20, 0xF5, 0xF2]) {
        return FsKind::F2fs;
    }
    if magic(buf, 0, b"LUKS\xBA\xBE") {
        return FsKind::Luks;
    }
    // LVM 标签位于前 4 扇区之一的扇区对齐槽位（LVM2 label 恒扇区对齐，pvcreate 默认第 2 扇区）
    if (0..4).any(|s| magic(buf, s * 512, b"LABELONE")) {
        return FsKind::Lvm;
    }
    if magic(buf, 3, b"EXFAT") {
        return FsKind::Exfat;
    }
    if magic(buf, 0x36, b"FAT") || magic(buf, 0x52, b"FAT") {
        return FsKind::Fat;
    }
    if is_swap_magic(buf) {
        return FsKind::Swap;
    }
    if magic(buf, 0x8001, b"CD001") {
        return FsKind::Iso9660;
    }
    // squashfs 4.0 魔数 "hsqs"（SQUASHFS_MAGIC 小端）@分区偏移 0；
    // EROFS 魔数 __le32 0xE0F5E1E2，超级块位于偏移 1024（与 fstools rootdisk.c 一致）
    if magic(buf, 0, b"hsqs") {
        return FsKind::Squashfs;
    }
    if magic(buf, 1024, &[0xE2, 0xE1, 0xF5, 0xE0]) {
        return FsKind::Erofs;
    }
    FsKind::Unknown
}

// ── overlay 布局（OpenWrt combined 镜像：分区头只读根 + 同分区 RW 层）────

/// overlay 起点对齐：fstools libfstools/rootdisk.c 的 ROOTDEV_OVERLAY_ALIGN。
/// 公式必须与 fstools 逐字节一致——mount_root 建立 loop 时用的 lo_offset
/// 即此对齐值，任何偏差都会使我们 resize 的区域与运行时 overlay 错位
const OVERLAY_ALIGN: u64 = 64 * 1024;

/// overlay 层起点（字节，相对分区头）：
/// squashfs 4.0 → bytes_used __le64 @0x28（内核 squashfs_fs.h 结构序：
/// 5×u32 + 4×u16 + u64 root_inode = 0x28）；
/// EROFS → blocks u32 @超级块+0x24 左移 blkszbits u8 @超级块+0x0C
/// （超级块 @1024，字段序同 fstools rootdisk.c 的 struct erofs_super_block）。
/// 两者统一 64K 上对齐；解析失败或 offset 为 0 → None
fn overlay_offset_at(dev: &mut dyn ReadSeek, part_offset: u64) -> Option<u64> {
    let mut sb = [0u8; 2048];
    let n = read_at(dev, part_offset, &mut sb);
    let sb = &sb[..n];
    let raw = if magic(sb, 0, b"hsqs") {
        if sb.len() < 0x30 {
            return None;
        }
        le64(&sb[0x28..0x30])
    } else if sb.len() >= 1024 + 0x28 && magic(sb, 1024, &[0xE2, 0xE1, 0xF5, 0xE0]) {
        let blkszbits = sb[1024 + 0x0C] as u32;
        let blocks = le32(&sb[1024 + 0x24..1024 + 0x28]) as u64;
        // blkszbits 是盘上可控值，checked_shl 挡位数溢出
        blocks.checked_shl(blkszbits)?
    } else {
        return None;
    };
    if raw == 0 {
        return None;
    }
    raw.checked_add(OVERLAY_ALIGN - 1).map(|v| v & !(OVERLAY_ALIGN - 1))
}

/// btrfs 多设备判定：超级块 @0x10000 的 num_devices 字段（相对偏移 0x88，u64 LE）。
/// 大于 1 表示多设备/RAID——`resize max` 语义按 devid
/// 分配，不能一次到位，跳过（分区扩容本身安全，fs 层留给用户手动处理）
fn btrfs_multi_device(dev: &mut dyn ReadSeek, part_offset: u64) -> bool {
    let mut sb = [0u8; 0x90];
    read_at(dev, part_offset + 0x10000, &mut sb) == 0x90 && le64(&sb[0x88..0x90]) > 1
}

/// swap 头（v1）：UUID @1036、label @1052，魔数在页尾（4086/8186/65526 按页大小）
pub struct SwapInfo {
    pub uuid: String,
    pub label: String,
}

pub fn read_swap_info(dev: &mut dyn ReadSeek, part_offset: u64) -> Option<SwapInfo> {
    let mut page = [0u8; 65536];
    let n = read_at(dev, part_offset, &mut page);
    let page = &page[..n];
    if !is_swap_magic(page) {
        return None;
    }
    // swap 头 UUID @1036 为 RFC 大端序；Uuid::from_bytes → hyphenated（8-4-4-4-12）
    let raw: [u8; 16] = page[1036..1052].try_into().map_err(|_| ()).ok()?;
    let uuid = uuid::Uuid::from_bytes(raw).hyphenated().to_string();
    let label_end = page[1052..1068].iter().position(|&b| b == 0).unwrap_or(16);
    let label = String::from_utf8_lossy(&page[1052..1052 + label_end]).trim().to_string();
    Some(SwapInfo { uuid, label })
}

/// swap 魔数在页尾，按页大小三档（4K/8K/64K page）
const SWAP_MAGIC_OFFSETS: [usize; 3] = [4086, 8186, 65526];

fn is_swap_magic(buf: &[u8]) -> bool {
    SWAP_MAGIC_OFFSETS.iter().any(|&off| magic(buf, off, b"SWAPSPACE2"))
}

/// superfloppy 的 fs 末尾估算（NoUsefulSpace 判定用；不可得 → None → 直接执行，工具幂等）
fn superfloppy_fs_end_bytes(dev: &mut dyn ReadSeek, offset: u64) -> Option<u64> {
    let mut sb = [0u8; 1024];
    let n = read_at(dev, offset, &mut sb);
    let sb = &sb[..n];
    if sb.len() < 0x40 {
        return None;
    }
    // btrfs：超级块 @+0x10000（不在开头 1KB 内），total_bytes u64@0x70（官方 On-disk Format）
    let mut bsb = [0u8; 0x78];
    if read_at(dev, offset + 0x10000, &mut bsb) == 0x78 && magic(&bsb, 0x40, b"_BHRfS_M") {
        return Some(offset.saturating_add(le64(&bsb[0x70..0x78])));
    }
    if magic(sb, 3, b"NTFS    ") {
        // NTFS BPB：bytes/sector @0x0B, sectors/cluster @0x0D, total sectors @0x28
        let bps = u16::from_le_bytes([sb[0x0b], sb[0x0c]]) as u64;
        let spc = sb[0x0d] as u64;
        let total = le64(&sb[0x28..0x30]);
        let total = if total == 0 { le32(&sb[0x13..0x17]) as u64 } else { total };
        if bps == 0 || spc == 0 {
            return None;
        }
        return Some(bps.saturating_mul(spc).saturating_mul(total));
    }
    if magic(sb, 0, b"XFSB") {
        // XFS：blocksize u32@4, dblocks u64@8（大端，xfs_dsb __be32/__be64）
        let bs = be32(&sb[4..8]) as u64;
        let dblocks = be64(&sb[8..16]);
        if bs == 0 {
            return None;
        }
        return Some(bs.saturating_mul(dblocks));
    }
    // ext：superblock @fs+1024，魔数 @+0x438 已由 sniff 确认
    let mut esb = [0u8; 1024];
    let n = read_at(dev, offset + 1024, &mut esb);
    let esb = &esb[..n];
    if esb.len() >= 0x160 && magic(esb, 0x38, &[0x53, 0xef]) {
        let log_bs = le32(&esb[0x18..0x1c]);
        let blocks_lo = le32(&esb[0x4..0x8]) as u64;
        let incompat = le32(&esb[0x60..0x64]);
        let blocks = if incompat & 0x80 != 0 {
            blocks_lo | ((le32(&esb[0x150..0x154]) as u64) << 32)
        } else {
            blocks_lo
        };
        // log_bs 来自盘上 u32——checked_shl 同时挡移位位数溢出与块大小值溢出
        let bs = 1024u64.checked_shl(log_bs)?;
        return blocks.checked_mul(bs);
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
    /// root 分区旧尺寸（sysfs 512B 扇区）——分析层算术产物，执行层只消费
    pub root_old_sectors: u64,
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
        /// /sys 比对时扣除 `, +` 的对齐容差）。手术路径不消费此值
        /// （精确值依赖 relocate 后重读 header）
        expected_new_sectors: u64,
        old_sectors: u64,
        is_gpt: bool,
        /// overlay 布局（OpenWrt combined 镜像）：RW 层起点（字节，相对分区头）。
        /// Some 时 fs 扩容经临时 loop（offset 映射）执行；fs == Unknown =
        /// 首启 rootfs_data 未格式化，分区级扩容后由 mount_root 初始化
        overlay: Option<u64>,
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
        FsKind::Squashfs => "squashfs rootfs outside partition overlay layout".into(),
        FsKind::Erofs => "EROFS rootfs not supported".into(),
        FsKind::Swap => "swap on superfloppy is not growable".into(),
        FsKind::Unknown => "unknown filesystem".into(),
        _ => "unsupported filesystem".into(),
    }
}

/// 可原地扩容的 fs 集合（单一来源：superfloppy / swap 手术 / 常规候选共用）
fn is_growable(fs: FsKind) -> bool {
    matches!(fs, FsKind::Ext | FsKind::Xfs | FsKind::Ntfs | FsKind::Btrfs | FsKind::Lvm | FsKind::F2fs)
}

/// 分区设备名：内核规则——设备名以数字结尾加 p（nvme0n1p3/loop0p1/md126p1），否则直接拼接（sda3）
pub fn part_dev_name(disk_name: &str, num: u32) -> String {
    let sep = if disk_name.ends_with(|c: char| c.is_ascii_digit()) { "p" } else { "" };
    format!("{disk_name}{sep}{num}")
}

fn disk_name_of(disk_dev: &str) -> String {
    disk_dev.rsplit('/').next().unwrap_or(disk_dev).to_string()
}

/// 单位约定见 [`DiskGeometry`]；容量字段（old/expected_sectors）统一用 sysfs 单位
pub fn analyze_with(dev: &Path, disk_name: &str, device_sectors: u64, lba_bytes: u64, policy: &GrowPolicy) -> GrowPlan {
    let skip = |r: &str| GrowPlan { action: None, skip_reason: Some(r.to_string()) };
    let Ok(mut f) = File::open(dev) else {
        return skip("cannot open device");
    };
    let Some(table) = parse_table(&mut f, lba_bytes) else {
        return skip("cannot parse partition table");
    };

    // superfloppy：直达 fs 扩容，无分区步骤。part= 声明在无分区盘上无法
    // 兑现，按契约拒绝而非静默忽略
    if table.label == Label::None {
        if let PartSpec::Number(n) = policy.part {
            return skip(&format!("partition {n} declared but disk has no partition table"));
        }
        let fs = sniff_fs(&mut f, 0);
        return if is_growable(fs) {
            if fs == FsKind::Btrfs && btrfs_multi_device(&mut f, 0) {
                return skip("btrfs multi-device filesystem");
            }
            // fs_end 来自盘上 superblock，损坏时可能为超大值——用减法 + saturating 防溢出
            if let Some(fs_end) = superfloppy_fs_end_bytes(&mut f, 0)
                && device_sectors.saturating_mul(SECTOR).saturating_sub(fs_end)
                    < MIN_FREE_SECTORS * SECTOR
            {
                return skip("no free space after filesystem");
            }
            GrowPlan {
                action: Some(GrowAction::FilesystemOnly { fs, fs_dev: dev.display().to_string() }),
                skip_reason: None,
            }
        } else {
            skip(&unsupported_reason(fs))
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
    // backup 保留区按 UEFI 条目数组下限推导（≥16384B），不硬编码 33 扇区
    let usable_end = match table.label {
        Label::Gpt => {
            let (n, esz) = table.gpt_meta.unwrap_or((128, 128));
            let reserved_lba = 1 + (n as u64 * esz as u64).div_ceil(lba_bytes);
            device_sectors.saturating_sub(lba_to_sysfs(reserved_lba, lba_bytes))
        }
        // MBR 32-bit LBA 上限（512 盘 = 2^32 扇区；4Kn 盘 = 2^35）
        _ => device_sectors.min(lba_to_sysfs(1u64 << 32, lba_bytes)),
    };
    // last_lba 来自盘上分区表，损坏时可达 u64::MAX——saturating 防溢出
    let free = usable_end.saturating_sub(lba_to_sysfs(last.last_lba.saturating_add(1), lba_bytes));
    if free < MIN_FREE_SECTORS {
        return skip("no free space after last partition");
    }

    // 候选选择（最高级不变量"绝不移动有持久数据的分区"的直接推论）：
    // 可扩集合 = {末分区} ∪ {末分区=swap 时的倒数第二分区}
    let last_fs = sniff_fs(&mut f, lba_to_bytes(last.first_lba, lba_bytes));
    if last_fs == FsKind::Btrfs && btrfs_multi_device(&mut f, lba_to_bytes(last.first_lba, lba_bytes)) {
        return skip("btrfs multi-device filesystem");
    }
    let (candidate, surgery, overlay) = if last_fs == FsKind::Swap {
        let Some(prev) = (sorted.len() >= 2).then(|| sorted[sorted.len() - 2].clone()) else {
            return skip("swap is the only partition");
        };
        if prev.is_container {
            return skip("MBR logical/extended not supported in v1");
        }
        let prev_fs = sniff_fs(&mut f, lba_to_bytes(prev.first_lba, lba_bytes));
        if prev_fs == FsKind::Btrfs && btrfs_multi_device(&mut f, lba_to_bytes(prev.first_lba, lba_bytes)) {
            return skip("btrfs multi-device filesystem");
        }
        // overlay 布局同样可作为手术目标：手术只操作分区表，分区起始 LBA
        // 不变 → overlay offset（相对分区头）手术前后不变；swap-last 是
        // OpenWrt 用户自定义布局时 fstools 的 rootfs_data 不受影响
        let prev_overlay = if prev_fs == FsKind::Erofs {
            return skip("EROFS rootfs overlay not supported (squashfs only)");
        } else if prev_fs == FsKind::Squashfs {
            let Some(off) = overlay_offset_at(&mut f, lba_to_bytes(prev.first_lba, lba_bytes)) else {
                return skip("cannot parse squashfs superblock for overlay offset");
            };
            let part_bytes = (prev.last_lba - prev.first_lba + 1).saturating_mul(lba_bytes);
            if off >= part_bytes {
                return skip("overlay offset exceeds partition bounds");
            }
            Some(off)
        } else {
            None
        };
        if !is_growable(prev_fs) && prev_overlay.is_none() {
            return skip("swap last, no growable partition before it");
        }
        let Some(si) = read_swap_info(&mut f, lba_to_bytes(last.first_lba, lba_bytes)) else {
            return skip("cannot read swap header");
        };
        let plan = SurgeryPlan {
            swap_num: last.num,
            root_num: prev.num,
            root_first_lba: prev.first_lba,
            root_ptype: prev.ptype.clone(),
            root_old_sectors: lba_to_sysfs(prev.last_lba - prev.first_lba + 1, lba_bytes),
            swap_first_lba: last.first_lba,
            swap_sectors: last.last_lba - last.first_lba + 1,
            swap_ptype: last.ptype.clone(),
            swap_uuid: si.uuid,
            swap_label: si.label,
            swap_partuuid: last.partuuid.clone(),
        };
        (prev, Some(plan), prev_overlay)
    } else if last.is_container {
        return skip("MBR logical/extended not supported in v1");
    } else if last_fs == FsKind::Erofs {
        // fstools 已支持 EROFS overlay；EROFS 底层不扩容，仅 squashfs
        return skip("EROFS rootfs overlay not supported (squashfs only)");
    } else if last_fs == FsKind::Squashfs {
        let Some(off) = overlay_offset_at(&mut f, lba_to_bytes(last.first_lba, lba_bytes)) else {
            return skip("cannot parse squashfs superblock for overlay offset");
        };
        // offset 越过分区界 = 镜像损坏防御（squashfs 声明的 bytes_used 不可信）
        let part_bytes = (last.last_lba - last.first_lba + 1).saturating_mul(lba_bytes);
        if off >= part_bytes {
            return skip("overlay offset exceeds partition bounds");
        }
        (last.clone(), None, Some(off))
    } else if is_growable(last_fs) {
        (last.clone(), None, None)
    } else {
        return skip(&unsupported_reason(last_fs));
    };
    // 候选 fs：overlay 布局在 offset 处识别 RW 层（f2fs/ext4 可扩容；
    // Unknown = 首启 rootfs_data 未格式化，分区级扩容后由 mount_root 以
    // 全分区初始化，同 fstools block_volume_format 语义）
    let plan_fs = if let Some(off) = overlay {
        match sniff_fs(&mut f, lba_to_bytes(candidate.first_lba, lba_bytes).saturating_add(off)) {
            k @ (FsKind::F2fs | FsKind::Ext) => k,
            _ => FsKind::Unknown,
        }
    } else {
        match surgery {
            Some(_) => sniff_fs(&mut f, lba_to_bytes(candidate.first_lba, lba_bytes)), // 手术目标 fs（倒数第二分区）
            None => last_fs,
        }
    };

    // 声明式指定 = "候选指定"：必须命中自动候选，否则服从安全判定
    if let PartSpec::Number(n) = policy.part
        && n != candidate.num
    {
        return skip(&format!("partition {n} is not the growth candidate (candidate: partition {})", candidate.num));
    }

    let old_sectors = lba_to_sysfs(candidate.last_lba - candidate.first_lba + 1, lba_bytes);
    let expected_new_sectors = match surgery {
        Some(_) => 0, // 手术路径不消费（精确值 relocate 后重读）
        None => usable_end - lba_to_sysfs(candidate.first_lba, lba_bytes),
    };

    GrowPlan {
        action: Some(GrowAction::PartitionGrow {
            part_num: candidate.num,
            part_dev: format!("/dev/{}", part_dev_name(disk_name, candidate.num)),
            fs: plan_fs,
            surgery,
            expected_new_sectors,
            old_sectors,
            is_gpt: table.label == Label::Gpt,
            overlay,
        }),
        skip_reason: None,
    }
}

// ── 执行层（--grow 子进程） ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Status {
    Disabled,
    Expanded,
    Skipped,
    Partial,
    #[default]
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

// 工具名（存在性守卫与 spawn 用同一来源）。工具不进 initramfs，随 fast path
// 注入 ISO 根 /grow/；挂载仅 MS_RDONLY 不含 noexec，静态 musl 二进制可直接执行。
// 绝对路径由 tool_path() 从 BOOT_MEDIA_DIR 拼接（避免介质路径前缀二次硬编码）
const SFDISK: &str = "sfdisk";
const MKSWAP: &str = "mkswap";
const PARTX: &str = "partx";
const E2FSCK: &str = "e2fsck";
const RESIZE2FS: &str = "resize2fs";
const XFS_GROWFS: &str = "xfs_growfs";
const NTFSRESIZE: &str = "ntfsresize";
const BTRFS: &str = "btrfs";
/// F2FS 双工具：fsck.f2fs 一致性预检 + resize.f2fs 扩容（同 f2fs-tools 包）
const FSCK_F2FS: &str = "fsck.f2fs";
const RESIZE_F2FS: &str = "resize.f2fs";
/// LVM2 多调用二进制：统一以 `lvm <子命令>` 形式调用（pvresize/lvs/vgchange/lvextend）
const LVM: &str = "lvm";
/// 工具子目录（相对安装介质根）
const GROW_TOOLS_DIR: &str = "grow";

/// 工具绝对路径（BOOT_MEDIA_DIR 拼接，与 grow.conf 同源）
fn tool_path(name: &str) -> String {
    format!("{BOOT_MEDIA_DIR}/{GROW_TOOLS_DIR}/{name}")
}

/// 按需加载单个内核模块（xfs/btrfs/dm-mod 场景的防御纵深）。
/// grow 阶段 BOOT_MEDIA_DIR 已挂载，追加 ISO /grow/modules/<ver> 作为兜底根：
/// boot 闭包模块仍在 initrd，grow 专用模块从 ISO 加载。
#[cfg(target_os = "linux")]
fn ensure_kernel_module(name: &str) -> Result<(), String> {
    crate::modload::ModuleLoader::new()
        .and_then(|mut l| { l.add_media_module_root(); l.load(name) })
}

/// 原子写（write-to-tmp + rename，同 /run tmpfs 内 rename 原子），
/// 杜绝 TUI 读到 truncate 后的空帧/半帧。tmp 写失败时放弃写帧——半帧
/// 会让 TUI 把非法 status 误读为 Failed，掩盖真实结果；缺失帧由 TUI 的
/// crash 判定兜底
fn atomic_write(path: &str, content: &str) {
    let tmp = format!("{path}.tmp");
    if let Ok(mut f) = File::create(&tmp)
        && f.write_all(content.as_bytes()).is_ok()
        && f.sync_all().is_ok()
        && fs::rename(&tmp, path).is_ok()
    {
        return;
    }
    log_line("atomic_write failed (frame dropped, TUI crash detection takes over)");
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

/// /sys/block/*/size 单位（内核 ABI 恒 512B 扇区）
type SysfsSectors = u64;
/// 分区表 LBA 单位（= 逻辑块大小，4Kn 盘为 4096）
type LbaSectors = u64;

/// LBA → sysfs 扇区（全文件唯一换算点）。LBA 输入含盘上可控数据（损坏的
/// GPT 条目可达任意值），saturating 防溢出
fn lba_to_sysfs(lba: LbaSectors, lba_bytes: u64) -> SysfsSectors {
    lba.saturating_mul(lba_bytes) / SECTOR
}

/// sysfs → LBA 扇区（手术路径 MBR 上限换算专用）
fn sysfs_to_lba(sysfs: SysfsSectors, lba_bytes: u64) -> LbaSectors {
    sysfs.saturating_mul(SECTOR) / lba_bytes
}

/// LBA → 字节偏移（sniff_fs/read_swap_info 的设备内偏移用）。LBA 输入
/// 含盘上可控数据（损坏分区表条目可达任意值），saturating 防溢出
fn lba_to_bytes(lba: LbaSectors, lba_bytes: u64) -> u64 {
    lba.saturating_mul(lba_bytes)
}

/// 目标盘几何（sysfs 一次读取的成对值）。
/// 全文件单位约定：device_sectors 是 sysfs 512B 扇区单位（/sys/block/*/size，
/// 内核 ABI 恒 512B）；lba_bytes 是分区表 LBA 单位（= 逻辑块大小，4Kn 盘为 4096）。
/// 两者换算统一走 lba_to_sysfs/sysfs_to_lba；容量字段（old/expected_sectors）统一用 sysfs 单位
struct DiskGeometry {
    device_sectors: u64,
    lba_bytes: u64,
}

impl DiskGeometry {
    /// 读取失败 → None（容量读不出在 run_grow 早已终结，此构造仅在几何齐备时调用）
    fn read(disk_name: &str) -> Option<Self> {
        Some(Self {
            device_sectors: read_sys_block_size(disk_name)?,
            lba_bytes: read_lba_size(disk_name)?,
        })
    }
}

struct GrowCtx {
    disk: String,
    disk_name: String,
    start: Instant,
    lv_declared: Option<String>,
}

impl GrowCtx {
    fn new(disk: &str, policy: &GrowPolicy) -> Self {
        Self {
            disk: disk.to_string(),
            disk_name: disk_name_of(disk),
            start: Instant::now(),
            lv_declared: policy.lv.clone(),
        }
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

    /// 工具执行结果统一记入 grow.log（exit 码 + 非空 stdout/stderr）
    fn log_output(&self, tool: &str, out: &std::process::Output) {
        let code = out.status.code().unwrap_or(-1);
        self.log(&format!("{tool} exit={code} (t={}s)", self.elapsed()));
        for (label, data) in [("stdout", &out.stdout), ("stderr", &out.stderr)] {
            let s = String::from_utf8_lossy(data).trim().to_string();
            if !s.is_empty() {
                self.log(&format!("{tool} {label}: {s}"));
            }
        }
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
        self.log_output(tool, &out);
        Some(out.status.code().unwrap_or(-1))
    }

    /// 工具执行并返回 stdout（LVM 的 VG/LV 元数据发现）。None = spawn 失败
    fn run_capture(&self, tool: &str, args: &[&str]) -> Option<(i32, String)> {
        let out = Command::new(tool).args(args).output().ok()?;
        self.log_output(tool, &out);
        Some((out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).trim().to_string()))
    }

    /// 工具执行并归因：exit 0 返回继续；失败/无法 spawn 由闭包归因并终结
    /// （闭包内调用 ctx.finish 发散，其后代码不可达）。返回 ()，成功继续。
    fn run_checked(
        &self,
        tool: &str,
        args: &[&str],
        stdin: Option<&str>,
        on_fail: impl FnOnce(i32),
        on_spawn: impl FnOnce(),
    ) {
        match self.run(tool, args, stdin) {
            Some(0) => {}
            Some(c) => {
                on_fail(c);
            }
            None => {
                on_spawn();
            }
        }
    }
}

fn read_sys_block_size(disk_name: &str) -> Option<u64> {
    fs::read_to_string(format!("/sys/block/{disk_name}/size")).ok()?.trim().parse().ok()
}

/// 分区表 LBA 单位 = 逻辑块大小。sfdisk(8)："sfdisk always internally uses
/// the device sector size provided by the kernel"——即本函数读取的同一 sysfs 值，
/// 故手术路径向 sfdisk 传 LBA 值时单位天然一致。注意：仅无名 field 输入
/// （`, +`/`start,size,type=`）如此；带 sector-size header 的 dump 式输入
/// 在 util-linux ≥ 2.39 会触发重算
fn read_lba_size(disk_name: &str) -> Option<u64> {
    fs::read_to_string(format!("/sys/block/{disk_name}/queue/logical_block_size"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&b| b >= SECTOR && b % SECTOR == 0)
        // 不做 512 兜底：4Kn 盘按 512 解析会使 GPT 头错位、分区表误读——
        // 识别失败直接无几何可用，由调用方安全终结
}

/// sfdisk `, +` 对齐容差：镜像 libfdisk alignment.c 的 grain 公式——
/// io_size = optimal（缺省回退 min_io），低于 1MiB 取 1MiB；libfdisk 侧的
/// 缩减修正（phy 对齐修正 / 小盘取 phy）只会让实际 grain 更小，容差取
/// 公式上限即恒 ≥ 实际对齐损失。单位：sysfs 512B 扇区
fn align_tolerance(disk_name: &str) -> u64 {
    let read = |f: &str| {
        fs::read_to_string(format!("/sys/block/{disk_name}/queue/{f}"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    let io_size = match read("optimal_io_size") {
        0 => read("minimum_io_size"),
        v => v,
    };
    io_size.max(1 << 20) / SECTOR
}

/// 读盘上分区表核对目标分区是否已变大——sfdisk 失败时 exit code 无法区分
/// "写入前失败"与"写入后失败"（内核重读失败也返回非 0），以盘上事实为准
fn disk_partition_grown(disk: &Path, part_num: u32, old_sectors: u64, lba_bytes: u64) -> bool {
    let Ok(mut f) = File::open(disk) else { return false };
    let Some(table) = parse_table(&mut f, lba_bytes) else { return false };
    table.entries.iter().any(|e| {
        e.num == part_num
            && lba_to_sysfs(e.last_lba.saturating_sub(e.first_lba) + 1, lba_bytes) > old_sectors
    })
}

fn sysfs_part_size(disk_name: &str, part_num: u32) -> Option<u64> {
    let part = part_dev_name(disk_name, part_num);
    fs::read_to_string(format!("/sys/block/{disk_name}/{part}/size")).ok()?.trim().parse().ok()
}

/// 分区变更最终判据 = /sys 实际尺寸（sfdisk 成功 ≠ 内核已暴露新尺寸）。
/// 轮询 100ms×100 → 未达 → partx 兜底（节点缺失 -a / 尺寸不符 -u）→ 再轮询 → 仍未达 → false
fn wait_partition_visible(ctx: &GrowCtx, part_num: u32, min_size: u64) -> bool {
    let check = |min: u64| sysfs_part_size(&ctx.disk_name, part_num).is_some_and(|s| s >= min);
    let poll = |min: u64| {
        for _ in 0..POLL_TRIES {
            if check(min) {
                return true;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        false
    };

    if poll(min_size) {
        return true;
    }
    // 兜底：节点缺失 → partx --add；尺寸不符 → partx --update。--nr 限精确分区，
    // 避免全盘分区刷新产生不必要 I/O / 无关分区 EBUSY（partx(8) 支持 --nr 单分区）
    let node_missing = sysfs_part_size(&ctx.disk_name, part_num).is_none();
    let mode = if node_missing { "-a" } else { "-u" };
    let nr = part_num.to_string();
    ctx.log(&format!("kernel reread incomplete, trying partx {mode} --nr {nr}"));
    let _ = ctx.run(&tool_path(PARTX), &[mode, "--nr", &nr, &ctx.disk], None);
    poll(min_size)
}

/// 设备末端 LBA（唯一推导点）：device_bytes / lba_bytes − 1
fn last_lba_of(device_bytes: u64, lba_bytes: u64) -> Option<u64> {
    device_bytes.checked_sub(lba_bytes).map(|b| b / lba_bytes)
}

/// 读 GPT header 字段：Some((last_usable_lba, (条目数, 条目尺寸)))。
/// 主头（LBA1）优先，损坏时按 UEFI 双副本语义回退 backup——backup 除魔数外
/// 还须验 MyLBA（@24）== 末端 LBA，排除 dd 前旧尺寸镜像尾部残留的过时
/// backup（其 last_usable_lba 是过期值）；字段偏移两副本相同
fn read_gpt_header(disk: &str, device_sectors: u64, lba_bytes: u64) -> Option<(u64, (u32, u32))> {
    let parse = |header: &[u8; 512], my_lba: u64| -> Option<(u64, (u32, u32))> {
        (&header[0..8] == b"EFI PART" && le64(&header[24..32]) == my_lba)
            .then(|| (le64(&header[48..56]), (le32(&header[80..84]), le32(&header[84..88]))))
    };
    let last_lba = last_lba_of(device_sectors.checked_mul(SECTOR)?, lba_bytes)?;
    let mut f = File::open(disk).ok()?;
    let mut header = [0u8; 512];
    if read_at(&mut f, lba_bytes, &mut header) == 512
        && let Some(v) = parse(&header, 1)
    {
        return Some(v);
    }
    (read_at(&mut f, last_lba * lba_bytes, &mut header) == 512).then(|| parse(&header, last_lba))?
}

/// backup header 是否已在设备末端标准位（relocate 成功/无需迁移的判据）。
/// UEFI 规范：backup header 位于最后 LBA，primary header 的 AlternateLBA（@+32）
/// 应指向它。此处同时校验 backup 的 MyLBA（@+24）与 primary 的 AlternateLBA，
/// 排除"设备尾部残留旧 GPT 备份、primary 仍指向旧位置"造成的误判已就位。
fn backup_header_at_end(disk: &str, device_sectors: u64, lba_bytes: u64) -> bool {
    let Ok(mut f) = File::open(disk) else { return false };
    let Some(device_bytes) = device_sectors.checked_mul(SECTOR) else { return false };
    let Some(last_lba) = last_lba_of(device_bytes, lba_bytes) else { return false };
    let Some(off) = device_bytes.checked_sub(lba_bytes) else { return false };
    let mut header = [0u8; 512];
    let backup_ok = read_at(&mut f, off, &mut header) == 512
        && &header[0..8] == b"EFI PART"
        && le64(&header[24..32]) == last_lba;
    if !backup_ok {
        return false;
    }
    // 校验 primary header（LBA1）的 AlternateLBA（@32..40）指向末尾备份 LBA
    let mut primary = [0u8; 512];
    read_at(&mut f, lba_bytes, &mut primary) == 512
        && &primary[0..8] == b"EFI PART"
        && le64(&primary[32..40]) == last_lba
}

/// fs 扩容工具存在性守卫（模板 initramfs 与 grow.conf 不匹配时的安全网）。
/// 返回 None = 齐备；Some(reason) = Skipped 原因。
/// 分区基础工具（sfdisk/partx/mkswap）不在此列——mkswap 仅手术需要，
/// 由调用方按布局单独检查
fn tools_missing(fs: FsKind) -> Option<String> {
    let fs_tools: &[&str] = match fs {
        FsKind::Ext => &[E2FSCK, RESIZE2FS],
        FsKind::Xfs => &[XFS_GROWFS],
        FsKind::Ntfs => &[NTFSRESIZE],
        FsKind::Btrfs => &[BTRFS],
        FsKind::F2fs => &[FSCK_F2FS, RESIZE_F2FS],
        FsKind::Lvm => &[LVM],
        _ => &[],
    };
    let missing: Vec<&str> = fs_tools
        .iter()
        .filter(|t| !Path::new(&tool_path(t)).exists())
        .copied()
        .collect();
    (!missing.is_empty()).then(|| format!("tools not bundled: {}", missing.join(", ")))
}

/// 在线扩容 fs 规格：挂载参数、工具命令与归因文案（单一数据，供 grow_mounted_fs 消费）。
/// 非 Linux 构建仅占位（Linux-only 项目），字段只被 linux 分支读取
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct MountedGrow {
    fstype: &'static str,
    module: &'static str,
    tool: &'static str,
    args: &'static [&'static str],
    mount_err: &'static str,
    grow_err: &'static str,
}

const XFS_GROW: MountedGrow = MountedGrow {
    fstype: "xfs",
    module: "xfs",
    tool: XFS_GROWFS,
    args: &["-d", GROW_MNT],
    mount_err: "XFS kernel support unavailable",
    grow_err: "xfs_growfs failed",
};

const BTRFS_GROW: MountedGrow = MountedGrow {
    fstype: "btrfs",
    module: "btrfs",
    tool: BTRFS,
    args: &["filesystem", "resize", "max", GROW_MNT],
    mount_err: "Btrfs kernel support unavailable",
    grow_err: "btrfs filesystem resize failed",
};

/// 在线扩容共享路径：模块加载防御 → 挂载 → 工具执行 → 卸载。
/// spec 携带 fstype/module/工具命令与归因文案；mount 失败即 Err（fs 支持缺失）。
fn grow_mounted_fs(ctx: &GrowCtx, target: &str, spec: &MountedGrow) -> Result<(), String> {
    // 防御纵深：boot 期模块列表通常已加载 fstype，此处不依赖该隐式前提；
    // 失败记入 grow.log，mount 失败自然兜底归因
    #[cfg(target_os = "linux")]
    if let Err(e) = ensure_kernel_module(spec.module) {
        ctx.log(&format!("module {}: {}", spec.module, e));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (ctx, spec);
    let _ = fs::create_dir_all(GROW_MNT);
    #[cfg(target_os = "linux")]
    {
        use nix::mount::{MsFlags, mount, umount, umount2, MntFlags};
        // 显式 turbofish：data=None 单独无法推断类型（与 init.rs 同模式）
        if mount::<str, str, str, str>(
            Some(target),
            GROW_MNT,
            Some(spec.fstype),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            None,
        )
        .is_err()
        {
            return Err(spec.mount_err.into());
        }
        let grown = matches!(ctx.run(&tool_path(spec.tool), spec.args, None), Some(0));
        // 残留挂载兜底：普通 umount 失败（如句柄未释放）→ lazy detach
        if let Err(e) = umount(GROW_MNT) {
            ctx.log(&format!("umount {GROW_MNT} failed: {e}, trying lazy detach"));
            let _ = umount2(GROW_MNT, MntFlags::MNT_DETACH);
        }
        if grown {
            return Ok(());
        }
        Err(spec.grow_err.into())
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Linux-only 项目：非 linux 路径仅为占位，显式丢弃其余参数
        let _ = (ctx, target);
        Err(format!("{} grow requires Linux", spec.fstype))
    }
}

/// fs 扩容分发。Err(reason) = 工具拒绝或执行失败（归因由调用方按 mutation 状态定级）
fn resize_fs(ctx: &GrowCtx, fs: FsKind, target: &str) -> Result<(), String> {
    match fs {
        FsKind::Ext => {
            let Some(code) = ctx.run(&tool_path(E2FSCK), &["-fp", target], None) else {
                return Err("e2fsck spawn failed".into());
            };
            // 显式白名单 0..=2，禁止 4+——防未来退出码扩展自动放行
            if !matches!(code, 0..=2) {
                return Err(format!("e2fsck rejected (exit {code}, filesystem inconsistent)"));
            }
            match ctx.run(&tool_path(RESIZE2FS), &[target], None) {
                Some(0) => Ok(()),
                Some(c) => Err(format!("resize2fs failed (exit {c})")),
                None => Err("resize2fs spawn failed".into()),
            }
        }
        FsKind::Xfs => grow_mounted_fs(ctx, target, &XFS_GROW),
        FsKind::Ntfs => {
            // 干跑守卫：休眠/Fast Startup/BitLocker 脏卷在此拒绝
            let dry_t = ctx.elapsed();
            match ctx.run(&tool_path(NTFSRESIZE), &["-n", "-P", target], None) {
                Some(0) => {}
                Some(c) => return Err(format!("ntfsresize dry-run rejected (exit {c}, volume dirty or hibernated)")),
                None => return Err("ntfsresize spawn failed".into()),
            }
            ctx.log(&format!("ntfsresize dry-run ok at t={}s", dry_t));
            // 实跑 -ff：纯 flag 零交互（Clonezilla batch 实证用法），安全性由前置干跑保证
            match ctx.run(&tool_path(NTFSRESIZE), &["-ff", "-P", target], None) {
                Some(0) => {
                    ctx.log(&format!("ntfsresize real-run done at t={}s", ctx.elapsed()));
                    Ok(())
                }
                Some(c) => Err(format!("ntfsresize failed (exit {c})")),
                None => Err("ntfsresize spawn failed".into()),
            }
        }
        FsKind::Btrfs => grow_mounted_fs(ctx, target, &BTRFS_GROW),
        // F2FS 裸设备扩容（同 ext4 模式），但 fsck 语义与 e2fsck 截然不同：
        // -a/-p 并非无条件完整检查——官方语义为"仅当内核 F2FS 模块报告 bug 时才执行检查"，
        //   默认禁用，因此不能当预检用。故此处用无参数 fsck.f2fs 做真正的一致性扫描。
        // -f 是 force fix（修复全部不一致），无人值守 grow 阶段不应自动修改异常 fs，不传。
        // fsck.f2fs 退出码：0 成功 / -1 失败（无 e2fsck 那套 0..=2 白名单，仅接受 0）。
        // resize.f2fs 选项为 -d/-f/-s/-t/-i/-o/-V：不传 -t 默认 target=设备大小，
        // target > 当前 fs 大小即走 grow 路径（-f 仅在 target≤当前时强走 grow，勿传）；
        // -o 不传自动取最优 overprovision；-s 仅用于收缩路径。走默认路径即可。
        FsKind::F2fs => {
            match ctx.run(&tool_path(FSCK_F2FS), &[target], None) {
                Some(0) => {}
                Some(c) => return Err(format!("fsck.f2fs failed (exit {c}, filesystem inconsistent)")),
                None => return Err("fsck.f2fs spawn failed".into()),
            }
            match ctx.run(&tool_path(RESIZE_F2FS), &[target], None) {
                Some(0) => Ok(()),
                Some(c) => Err(format!("resize.f2fs failed (exit {c})")),
                None => Err("resize.f2fs spawn failed".into()),
            }
        }
        FsKind::Lvm => resize_lvm(ctx, target),
        _ => Err("unsupported filesystem".into()),
    }
}

/// LVM 扩容链：pvresize（PV 吃下分区新增空间）→ VG/LV 发现（单 LV 自动，
/// 多 LV 需 grow.conf `lv=` 声明，避免启发式猜错目标）→ vgchange -ay（initramfs
/// 无 udev 规则，dm 节点必须显式激活才会出现）→ lvextend -l +100%FREE →
/// 对 dm 设备 sniff 后递归 fs 扩容。
/// 递归深度有界：LV 上只可能是 ext/xfs/btrfs（再嵌套 LVM 的病态布局不支持）
fn resize_lvm(ctx: &GrowCtx, target: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if let Err(e) = ensure_kernel_module("dm-mod") {
        ctx.log(&format!("module dm-mod: {e}"));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = ctx;
    // 静态 lvm 的运行时目录（锁文件/扫描缓存；无 udev 环境不自建则命令失败）
    let _ = fs::create_dir_all("/run/lvm");
    let _ = fs::create_dir_all("/run/lock/lvm");

    // 1) PV 扩容（分区已由调用方扩完）
    match ctx.run(&tool_path(LVM), &["pvresize", target], None) {
        Some(0) => {}
        Some(c) => return Err(format!("pvresize failed (exit {c})")),
        None => return Err("lvm spawn failed".into()),
    }

    // 2) PV 所属 VG
    let Some((code, vg_out)) = ctx.run_capture(&tool_path(LVM), &["pvs", "--noheadings", "-o", "vg_name", target]) else {
        return Err("lvm spawn failed".into());
    };
    if code != 0 {
        return Err(format!("pvs failed (exit {code})"));
    }
    let vg = vg_out.trim().to_string();
    if vg.is_empty() {
        return Err("PV not in any volume group".into());
    }

    // 3) VG 内 LV 清单与受益者选择（与 part= 同构的声明式策略，不猜）：
    //    单 LV → 自动；多 LV → 必须命中 grow.conf `lv=` 声明，否则拒绝。
    //    默认 lvs 不加 -a，hidden 子卷（[tdata]/[tmeta]/_pmspare）不出现在
    //    输出中（lvs(8)：internal LV 仅 -a 可见），等值匹配亦不会误选；
    //    thin pool 本体默认可见且 lv_attr 首字符为 't'（lvs(8) volume type）
    let Some((code, lvs_out)) = ctx.run_capture(&tool_path(LVM), &["lvs", "--noheadings", "-o", "lv_name,lv_attr", &vg]) else {
        return Err("lvm spawn failed".into());
    };
    if code != 0 {
        return Err(format!("lvs failed (exit {code})"));
    }
    let lv_list: Vec<(String, String)> = lvs_out
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?.to_string(), f.next().unwrap_or("").to_string()))
        })
        .collect();
    let lv = match lv_list.len() {
        1 => lv_list[0].0.clone(),
        _ => match &ctx.lv_declared {
            Some(declared) if lv_list.iter().any(|(n, _)| n == declared) => declared.clone(),
            _ => {
                return Err(format!(
                    "volume group {vg} has {} logical volumes; declare one via grow.conf 'lv=<name>' (found: {})",
                    lv_list.len(),
                    lv_list.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
                ))
            }
        },
    };
    let lv_path = format!("/dev/{vg}/{lv}");

    // 4) 激活 VG：dm 设备节点由激活创建（无 udev 的 initramfs 必需步骤）
    match ctx.run(&tool_path(LVM), &["vgchange", "-ay", &vg], None) {
        Some(0) => {}
        Some(c) => return Err(format!("vgchange -ay failed (exit {c})")),
        None => return Err("lvm spawn failed".into()),
    }

    // 5) LV 吃掉 VG 全部空闲 extent
    match ctx.run(&tool_path(LVM), &["lvextend", "-l", "+100%FREE", &lv_path], None) {
        Some(0) => {}
        Some(c) => return Err(format!("lvextend failed (exit {c})")),
        None => return Err("lvm spawn failed".into()),
    }

    // thin pool（lv_attr 首字符 't'）：lvextend 扩池数据区即达成目标，
    // pool dm 设备无文件系统可扩——跳过激活/dm 解析/fs 递归，避免恒 Partial 假失败
    if lv_list.iter().any(|(n, a)| n == &lv && a.starts_with('t')) {
        ctx.log("thin pool data area extended (no fs resize needed)");
        return Ok(());
    }

    // 6) 解析 dm 设备节点（/dev/<vg>/<lv> 是 udev 符号链接，此处不存在；
    //    dm_path 给出真实节点 /dev/mapper/<vg>-<lv>，含转义规则处理）
    let Some((code, dm_out)) = ctx.run_capture(&tool_path(LVM), &["lvs", "--noheadings", "-o", "dm_path", &lv_path]) else {
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
    if !matches!(lv_fs, FsKind::Ext | FsKind::Xfs | FsKind::Btrfs | FsKind::F2fs) {
        return Err(format!("LV filesystem not growable ({lv_fs:?})"));
    }
    if let Some(reason) = tools_missing(lv_fs) {
        return Err(format!("{reason} (LV {dm})"));
    }
    resize_fs(ctx, lv_fs, &dm)
}

// ── overlay 执行层：临时 loop（offset 映射 RW 层）──────────────────────

// loop ioctl 常量与结构：与 init.rs 的 squashfs 挂载共用同一组 UAPI
// （include/uapi/linux/loop.h，内核 ≥5.8 的 LOOP_CONFIGURE；硬编码即规范）
#[cfg(target_os = "linux")]
mod loopabi {
    use nix::libc;
    pub const LOOP_CTL_GET_FREE: libc::Ioctl = 0x4C82;
    pub const LOOP_CLR_FD: libc::Ioctl = 0x4C01;
    pub const LOOP_CONFIGURE: libc::Ioctl = 0x4C0A;
    pub const LOOP_MAJOR: libc::dev_t = 7;

    #[repr(C)]
    pub struct LoopInfo64 {
        pub lo_device: u64, pub lo_inode: u64, pub lo_rdevice: u64,
        pub lo_offset: u64, pub lo_sizelimit: u64,
        pub lo_number: u32, pub lo_encrypt_type: u32, pub lo_encrypt_key_size: u32, pub lo_flags: u32,
        pub lo_file_name: [u8; 64], pub lo_crypt_name: [u8; 64], pub lo_encrypt_key: [u8; 32],
        pub lo_init: [u64; 2],
    }

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
    pub struct LoopConfig {
        pub fd: u32,
        pub block_size: u32,
        pub info: LoopInfo64,
        pub reserved: [u64; 8],
    }
}

/// 以 offset 映射分区内的 overlay 区域为独立块设备。resize 工具经
/// BLKGETSIZE64 取的是 loop 可见大小（= 分区大小 − offset，lo_offset 生效
/// 后由内核保证），不会越过 offset 触及 squashfs 区域，也不会回读底层分区
#[cfg(target_os = "linux")]
fn attach_loop_with_offset(ctx: &GrowCtx, backing: &Path, offset: u64) -> Result<String, String> {
    use nix::fcntl::{open, OFlag};
    use nix::sys::stat::{makedev, mknod, Mode, SFlag};
    use std::os::fd::AsRawFd;

    let ctrl = open("/dev/loop-control", OFlag::O_RDWR, Mode::empty())
        .map_err(|e| format!("open /dev/loop-control: {e}"))?;
    let free = unsafe { nix::libc::ioctl(ctrl.as_raw_fd(), loopabi::LOOP_CTL_GET_FREE) };
    if free < 0 {
        return Err("LOOP_CTL_GET_FREE failed".into());
    }
    let loop_path = format!("/dev/loop{free}");
    if !Path::new(&loop_path).exists() {
        let _ = mknod(loop_path.as_str(), SFlag::S_IFBLK, Mode::from_bits_truncate(0o600),
            makedev(loopabi::LOOP_MAJOR, free as nix::libc::dev_t));
    }
    let backing_fd = open(backing, OFlag::O_RDWR, Mode::empty())
        .map_err(|e| format!("open {backing:?}: {e}"))?;
    let cfg = loopabi::LoopConfig {
        fd: backing_fd.as_raw_fd() as u32,
        block_size: 0,
        info: loopabi::LoopInfo64 { lo_offset: offset, ..Default::default() },
        reserved: [0; 8],
    };
    let loop_fd = open(loop_path.as_str(), OFlag::O_RDWR, Mode::empty())
        .map_err(|e| format!("open {loop_path}: {e}"))?;
    let ok = unsafe {
        nix::libc::ioctl(loop_fd.as_raw_fd(), loopabi::LOOP_CONFIGURE, &cfg as *const loopabi::LoopConfig)
    } == 0;
    if !ok {
        unsafe { nix::libc::ioctl(loop_fd.as_raw_fd(), loopabi::LOOP_CLR_FD, 0) };
        return Err(format!("LOOP_CONFIGURE with offset {offset} failed"));
    }
    ctx.log(&format!("loop attached: {loop_path} offset={offset}"));
    Ok(loop_path)
}

/// loop 解绑。失败仅记日志（autoclear 语义下挂载结束即释放，残留不阻塞流程）
#[cfg(target_os = "linux")]
fn detach_loop(ctx: &GrowCtx, loop_dev: &str) {
    use nix::fcntl::{open, OFlag};
    use nix::sys::stat::Mode;
    use std::os::fd::AsRawFd;
    if let Ok(fd) = open(loop_dev, OFlag::O_RDWR, Mode::empty()) {
        unsafe { nix::libc::ioctl(fd.as_raw_fd(), loopabi::LOOP_CLR_FD, 0) };
    }
    ctx.log(&format!("loop detached: {loop_dev}"));
}

/// overlay 布局的 fs 扩容：临时 loop 承载 RW 层 → 复用 resize_fs → 解绑。
/// 清理失败不掩盖 resize 结果（先保存结果再解绑）。Linux-only 项目，
/// 非 Linux 仅为宿主测试编译的占位
fn grow_overlay_fs(ctx: &GrowCtx, fs: FsKind, part_dev: &Path, offset: u64) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let loop_dev = attach_loop_with_offset(ctx, part_dev, offset)?;
        let result = resize_fs(ctx, fs, &loop_dev);
        detach_loop(ctx, &loop_dev);
        result
    }
    #[cfg(not(target_os = "linux"))]
    {
        // 占位分支：逐个具名绑定，签名变更时编译器强制同步，不用 allow 整体静音
        let _ = (ctx, fs, part_dev, offset);
        Err("overlay grow requires Linux".into())
    }
}

/// overlay 手动恢复命令：完整自洽（建 loop → resize → 解绑），失败档共用
fn manual_cmd_for_overlay(fs: FsKind, part_dev: &str, offset: u64) -> String {
    format!(
        "losetup -o {offset} /dev/loop0 {part_dev}; {} ; losetup -d /dev/loop0",
        manual_cmd_for_fs(fs, "/dev/loop0")
    )
}

/// --grow 子进程入口：每步失败即写 result 退出，永不 panic，exit 0 恒成立
pub fn run_grow(disk: &str) -> ! {
    let policy = load_policy();
    let ctx = GrowCtx::new(disk, &policy);
    ctx.log(&format!("grow start: {disk}"));

    write_phase("analyze");
    if !policy.enabled {
        // Disabled：内部态，UI 不显示任何 grow 行
        ctx.finish(Status::Disabled, "", "", 0, 0);
    }

    let Some(geom) = DiskGeometry::read(&ctx.disk_name) else {
        ctx.finish(Status::Failed, "cannot read device size", "", 0, 0);
    };

    let plan = analyze_with(Path::new(disk), &ctx.disk_name, geom.device_sectors, geom.lba_bytes, &policy);
    let Some(action) = plan.action else {
        let reason = plan.skip_reason.unwrap_or_else(|| "not growable".into());
        ctx.finish(Status::Skipped, &reason, "", 0, 0);
    };

    match action {
        GrowAction::FilesystemOnly { fs, fs_dev } => {
            if let Some(reason) = tools_missing(fs) {
                ctx.finish(Status::Skipped, &reason, "", 0, 0);
            }
            write_phase("filesystem");
            // 无分区表变更：fs 失败归 Skipped
            match resize_fs(&ctx, fs, &fs_dev) {
                Ok(()) => {
                    let new_bytes = geom.device_sectors * SECTOR;
                    ctx.finish(Status::Expanded, "", "", 0, new_bytes);
                }
                Err(reason) => ctx.finish(Status::Skipped, &reason, "", 0, 0),
            }
        }
        GrowAction::PartitionGrow { part_num, part_dev, fs, surgery, expected_new_sectors, old_sectors, is_gpt, overlay } => {
            // 工具守卫（未动盘 → Skipped）：fs 工具按布局/手术需要；
            // sfdisk/partx 恒需（表写入与内核重读兜底）；mkswap 仅手术
            // （非手术路径不重建 swap）
            if ((overlay.is_some() && is_growable(fs)) || surgery.is_some())
                && let Some(reason) = tools_missing(fs)
            {
                ctx.finish(Status::Skipped, &reason, "", 0, 0);
            }
            let missing: Vec<&str> = [SFDISK, PARTX]
                .into_iter()
                .chain(surgery.is_some().then_some(MKSWAP))
                .filter(|t| !Path::new(&tool_path(t)).exists())
                .collect();
            if !missing.is_empty() {
                ctx.finish(Status::Skipped, &format!("tools not bundled: {}", missing.join(", ")), "", 0, 0);
            }
            write_phase("partition");

            // GPT：relocate backup header 到设备末端（仅 GPT；MBR 跳过）
            if is_gpt {
                match ctx.run(&tool_path(SFDISK), &["--relocate", "gpt-bak-std", &ctx.disk], None) {
                    Some(0) => {}
                    _ => {
                        // relocate 失败按盘上事实归因（覆盖三种半迁移态）：
                        // primary 与 backup 一致就位 → 无需迁移，继续；任一副本
                        // 已改写而另一副本未就位 = 持久变更 → Partial；两副本
                        // 均未改写 → Skipped（附幂等修复命令）；均不可读 → Failed
                        let last_lba = geom
                            .device_sectors
                            .checked_mul(SECTOR)
                            .and_then(|b| last_lba_of(b, geom.lba_bytes));
                        let repair = format!("repair: sfdisk --relocate gpt-bak-std {}", ctx.disk);
                        if backup_header_at_end(&ctx.disk, geom.device_sectors, geom.lba_bytes) {
                            // 已在标准位（本就无需迁移）→ 继续
                        } else {
                            let read_hdr = |off: u64| -> Option<[u8; 512]> {
                                let mut f = File::open(&ctx.disk).ok()?;
                                let mut header = [0u8; 512];
                                (read_at(&mut f, off, &mut header) == 512).then_some(header)
                            };
                            // primary 已迁移 = AlternateLBA（@32）指向设备末端
                            let primary_moved = last_lba.is_some_and(|l| {
                                read_hdr(geom.lba_bytes).is_some_and(|h| {
                                    &h[0..8] == b"EFI PART" && le64(&h[32..40]) == l
                                })
                            });
                            // backup 已迁移 = 末端 header 的 MyLBA（@24）指向自身
                            let backup_moved = last_lba.is_some_and(|l| {
                                read_hdr(l * geom.lba_bytes).is_some_and(|h| {
                                    &h[0..8] == b"EFI PART" && le64(&h[24..32]) == l
                                })
                            });
                            if primary_moved || backup_moved {
                                let reason =
                                    format!("gpt backup header relocate incomplete ({repair})");
                                ctx.finish(Status::Partial, &reason, "", old_sectors * SECTOR, 0);
                            } else if read_gpt_header(&ctx.disk, geom.device_sectors, geom.lba_bytes).is_some() {
                                let reason =
                                    format!("gpt backup header relocate failed ({repair})");
                                ctx.finish(Status::Skipped, &reason, "", 0, 0);
                            } else {
                                ctx.finish(Status::Failed, "gpt relocate failed (state unknown)", "", 0, 0);
                            }
                        }
                    }
                }
            }

            let old_bytes = old_sectors * SECTOR;
            if let Some(s) = surgery {
                grow_with_surgery(&ctx, &geom, is_gpt, &s, &part_dev, fs, overlay);
            } else {
                // 非手术：`, +`（start/type/UUID 全保留）。前置不变量：分析层已证明
                // target 是 end-LBA 最大可扩分区且其后无障碍——安全边界全在分析层
                let stdin = ", +\n";
                ctx.run_checked(
                    &tool_path(SFDISK),
                    &["-N", &part_num.to_string(), &ctx.disk],
                    Some(stdin),
                    |c| {
                        let reason = format!("sfdisk partition grow failed (exit {c})");
                        // 归因按盘上事实：分区已变大 = 变更已发生（Partial）；
                        // 未变大时 GPT 仍归 Partial（relocate 已改写 backup 元数据），
                        // MBR 未动 → Skipped
                        if is_gpt || disk_partition_grown(Path::new(&ctx.disk), part_num, old_sectors, geom.lba_bytes) {
                            ctx.finish(Status::Partial, &reason, "", old_bytes, 0);
                        } else {
                            ctx.finish(Status::Skipped, &reason, "", 0, 0);
                        }
                    },
                    || ctx.finish(Status::Failed, "sfdisk spawn failed", "", 0, 0),
                );

                write_phase("kernel-reread");
                // /sys 比对消费分析层的 expected 值；扣 `, +` 对齐容差（sfdisk 对齐粒度，自适应）。
                // 仅非手术路径消费容差——superfloppy / 手术路径不读
                let tolerance = align_tolerance(&ctx.disk_name);
                let expected = expected_new_sectors;
                let min_size = expected.saturating_sub(tolerance);
                // 期望值未超过旧尺寸（对齐后无增长空间）→ 已是目标态，无需等待内核同步
                if old_sectors < min_size && !wait_partition_visible(&ctx, part_num, min_size) {
                    // 与手术路径同档归档：分区表已持久扩容（sfdisk exit 0 已过），
                    // 卡点在内核同步 → Failed + fs 手动命令（重启后内核重读即扩，补 fs 即完成）
                    let manual = manual_cmd_for_fs(fs, &part_dev);
                    ctx.finish(Status::Failed, "kernel partition reread failed", &manual, old_bytes, 0);
                }

                write_phase("filesystem");
                let new_sectors = sysfs_part_size(&ctx.disk_name, part_num).unwrap_or(old_sectors);
                let new_bytes = new_sectors * SECTOR;
                if let Some(off) = overlay {
                    if !is_growable(fs) {
                        // 首启：rootfs_data 未格式化，分区级扩容即完成——
                        // mount_root 首次挂载将以全分区初始化 RW 层
                        ctx.finish(
                            Status::Expanded,
                            "overlay not formatted yet; partition expanded (mount_root will initialize it)",
                            "", old_bytes, new_bytes,
                        );
                    }
                    let manual = manual_cmd_for_overlay(fs, &part_dev, off);
                    match grow_overlay_fs(&ctx, fs, Path::new(&part_dev), off) {
                        Ok(()) => ctx.finish(Status::Expanded, "", "", old_bytes, new_bytes),
                        // 持久分区表变更已发生 → Partial + 手动命令
                        Err(reason) => ctx.finish(
                            Status::Partial,
                            &format!("partition expanded; overlay filesystem resize failed ({reason})"),
                            &manual, old_bytes, new_bytes,
                        ),
                    };
                }
                match resize_fs(&ctx, fs, &part_dev) {
                    Ok(()) => ctx.finish(Status::Expanded, "", "", old_bytes, new_bytes),
                    // 持久分区表变更已发生 → 不得降级 Skipped
                    Err(reason) => {
                        let manual = manual_cmd_for_fs(fs, &part_dev);
                        ctx.finish(Status::Partial, &format!("partition expanded; filesystem resize failed ({reason})"), &manual, old_bytes, new_bytes);
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
        FsKind::F2fs => format!("fsck.f2fs {dev} && resize.f2fs {dev}"),
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
    geom: &DiskGeometry,
    is_gpt: bool,
    s: &SurgeryPlan,
    root_dev: &str,
    fs: FsKind,
    overlay: Option<u64>,
) -> ! {
    let old_root_bytes = s.root_old_sectors * SECTOR;
    let swap_dev = format!("/dev/{}", part_dev_name(&ctx.disk_name, s.swap_num));
    let lba = geom.lba_bytes;

    // 手术精确算术：last_usable_lba 在 dd 后是镜像旧尺寸的过期值，
    // 必须 relocate 后重读（顺序依赖）；重读值异常偏小时回绕会产生
    // 天文数字 LBA，checked 减法显式终结
    let usable_last = if is_gpt {
        match read_gpt_header(&ctx.disk, geom.device_sectors, lba) {
            Some((last_usable, _)) => last_usable,
            None => ctx.finish(Status::Failed, "cannot re-read GPT header after relocate", "", 0, 0),
        }
    } else {
        // MBR 32-bit LBA 上限；device_sectors 是 sysfs 512B 单位，先换算为 LBA。
        // saturating_sub(1) 不依赖分析层不变式：极小盘换算为 0 时 usable_last=0，
        // 由下方 checked 链统一终结为 Skipped
        sysfs_to_lba(geom.device_sectors, lba).min(1u64 << 32).saturating_sub(1)
    };
    // S0 未动盘：算术异常属分析层前提失效 → Skipped（非 Failed）
    let Some(new_swap_start) = s.swap_sectors.checked_sub(1).and_then(|n| usable_last.checked_sub(n))
    else {
        ctx.finish(Status::Skipped, "surgery plan arithmetic underflow (abnormal last usable lba)", "", 0, 0);
    };
    let Some(root_new_size) = new_swap_start.checked_sub(s.root_first_lba) else {
        ctx.finish(Status::Skipped, "surgery plan arithmetic underflow (root start)", "", 0, 0);
    };
    let new_root_bytes = root_new_size.saturating_mul(lba);

    // 手动恢复命令（按失败档位生成；分析层已持有全部原值）
    let mkswap_cmd = |dev: &str| {
        // label 是盘上可控字节，manual_cmd 面向用户复制粘贴：白名单过滤防注入
        let label = if s.swap_label.is_empty() {
            String::new()
        } else {
            let safe: String = s
                .swap_label
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') { c } else { '_' })
                .collect();
            format!(" -L {safe}")
        };
        format!("mkswap -U {}{label} {dev}", s.swap_uuid)
    };
    let restore_partuuid = |cmd: &mut String| {
        if let Some(pu) = &s.swap_partuuid {
            cmd.push_str(&format!("; sfdisk --part-uuid {} {} {}", ctx.disk, s.swap_num, pu));
        }
    };
    // swap 重建组合命令（两失败档共用，仅起始 LBA 不同：原位 / 新尾部位置）
    let mkswap_recreate = |start: u64| {
        let mut c = format!(
            "printf '{}, {}, type={}' | sfdisk -N {} {}",
            start, s.swap_sectors, s.swap_ptype, s.swap_num, ctx.disk
        );
        restore_partuuid(&mut c);
        c.push_str("; ");
        c.push_str(&mkswap_cmd(&swap_dev));
        c
    };
    // S1→S2 失败：swap 已删未重建 → sfdisk 原位重建 + PARTUUID + mkswap 组合
    let manual_s2 = mkswap_recreate(s.swap_first_lba);
    // S2→S3 及以后：分区已重建 → mkswap 全参（+ GPT PARTUUID）
    let manual_s3 = {
        let mut c = mkswap_cmd(&swap_dev);
        restore_partuuid(&mut c);
        c
    };
    // S2→S3 失败档：swap 分区未重建，mkswap 对不存在的节点必失败——
    // manual 必须含分区重建步骤（新位置 + 原有 type/UUID/PARTUUID 全值）
    let manual_s3_pre = mkswap_recreate(new_swap_start);
    let manual_s4 = match overlay {
        Some(off) => manual_cmd_for_overlay(fs, root_dev, off),
        None => manual_cmd_for_fs(fs, root_dev),
    };

    // S0 → S1：删除 swap（失败 = 未动盘 → Skipped）
    ctx.run_checked(
        &tool_path(SFDISK),
        &["--delete", &ctx.disk, &s.swap_num.to_string()],
        None,
        |c| ctx.finish(Status::Skipped, &format!("swap delete failed (exit {c})"), "", 0, 0),
        || ctx.finish(Status::Failed, "sfdisk spawn failed", "", 0, 0),
    );
    ctx.log("surgery S1: swap deleted");

    // S1 → S2：root 精确扇区扩容（start/type/UUID 保留，size 精确无对齐优化）
    let stdin = format!(", {root_new_size}, type={}\n", s.root_ptype);
    ctx.run_checked(
        &tool_path(SFDISK),
        &["-N", &s.root_num.to_string(), &ctx.disk],
        Some(&stdin),
        |c| ctx.finish(
            Status::Partial,
            &format!("swap deleted; target unchanged (root expand exit {c})"),
            &manual_s2,
            old_root_bytes,
            0,
        ),
        || ctx.finish(Status::Failed, "sfdisk spawn failed", &manual_s2, old_root_bytes, 0),
    );
    ctx.log("surgery S2: target expanded");

    // S2 → S3：swap 尾部重建（复用原槽位 → 分区号/fstab 引用保真）
    let stdin = format!("{}, {}, type={}\n", new_swap_start, s.swap_sectors, s.swap_ptype);
    ctx.run_checked(
        &tool_path(SFDISK),
        &["-N", &s.swap_num.to_string(), &ctx.disk],
        Some(&stdin),
        |c| ctx.finish(
            Status::Partial,
            &format!("target expanded; swap missing (recreate exit {c})"),
            &manual_s3_pre,
            old_root_bytes,
            0,
        ),
        || ctx.finish(Status::Failed, "sfdisk spawn failed", &manual_s3_pre, old_root_bytes, 0),
    );
    ctx.log("surgery S3: swap partition rebuilt");

    // PARTUUID 恢复（仅 GPT；两命名空间独立，不抽象为单一 restore）
    if let Some(pu) = &s.swap_partuuid {
        ctx.run_checked(
            &tool_path(SFDISK),
            &["--part-uuid", &ctx.disk, &s.swap_num.to_string(), pu],
            None,
            |c| ctx.finish(
                Status::Partial,
                &format!("target expanded; swap recreation incomplete (part-uuid restore exit {c})"),
                &manual_s3,
                old_root_bytes,
                0,
            ),
            // spawn 失败 = 基础设施故障（全文件惯例 → Failed），此时 swap 已
            // 重建但 PARTUUID 缺失，状态不完整
            || ctx.finish(Status::Failed, "sfdisk spawn failed (part-uuid restore)", &manual_s3, old_root_bytes, 0),
        );
    }

    // 内核同步最终判据：/sys 实际尺寸（root 精确值 + swap 精确值，LBA→sysfs 512B 扇区换算）
    write_phase("kernel-reread");
    if !wait_partition_visible(ctx, s.root_num, lba_to_sysfs(root_new_size, lba))
        || !wait_partition_visible(ctx, s.swap_num, lba_to_sysfs(s.swap_sectors, lba))
    {
        ctx.finish(Status::Failed, "kernel partition reread failed", &manual_s3, old_root_bytes, 0);
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
    ctx.run_checked(
        &tool_path(MKSWAP),
        &arg_refs,
        None,
        |c| ctx.finish(
            Status::Partial,
            &format!("target expanded; swap recreation incomplete (mkswap exit {c})"),
            &manual_s3,
            old_root_bytes,
            new_root_bytes,
        ),
        || ctx.finish(Status::Failed, "mkswap spawn failed", &manual_s3, old_root_bytes, 0),
    );
    ctx.log("surgery S3 complete: swap UUID/label restored");

    // S3 → S4：fs 扩容（overlay 布局经临时 loop 映射 RW 层）
    write_phase("filesystem");
    if let Some(off) = overlay {
        if !is_growable(fs) {
            // 首启：rootfs_data 未格式化，swap 重建 + 分区级扩容即完成
            ctx.finish(
                Status::Expanded,
                "overlay not formatted yet; partition expanded (mount_root will initialize it)",
                "", old_root_bytes, new_root_bytes,
            );
        }
        match grow_overlay_fs(ctx, fs, Path::new(root_dev), off) {
            Ok(()) => ctx.finish(Status::Expanded, "", "", old_root_bytes, new_root_bytes),
            Err(reason) => ctx.finish(
                Status::Partial,
                &format!("swap rebuilt; overlay filesystem resize failed ({reason})"),
                &manual_s4,
                old_root_bytes,
                new_root_bytes,
            ),
        }
    }
    match resize_fs(ctx, fs, root_dev) {
        Ok(()) => ctx.finish(Status::Expanded, "", "", old_root_bytes, new_root_bytes),
        Err(reason) => ctx.finish(
            Status::Partial,
            &format!("swap rebuilt; fs resize failed ({reason})"),
            &manual_s4,
            old_root_bytes,
            new_root_bytes,
        ),
    }
}

// ── TUI 消费接口 ───────────────────────────────────────────────────────

/// /run/grow.result 解析结果（严格 key-value，UI 只做 presentation）。
/// status 使用强类型 Status 而非字符串，避免 TUI 侧重复解析/拼写错误
#[derive(Debug, Clone, Default)]
pub struct GrowOutcome {
    pub status: Status,
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
                out.status = match v.trim() {
                    "disabled" => Status::Disabled,
                    "expanded" => Status::Expanded,
                    "skipped" => Status::Skipped,
                    "partial" => Status::Partial,
                    "failed" => Status::Failed,
                    // 契约外的未知值 → 保守按失败处理（与 UI 兜底一致）
                    _ => Status::Failed,
                };
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