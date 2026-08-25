//! grow.rs 单元测试：纯逻辑层（策略解析 / 分区表解析 / fs 魔数 / 分析决策）。
//! 不触真实设备与 /run IPC 文件——那些属于 Gate 手动验证矩阵。

use std::io::Cursor;
use std::path::PathBuf;

use disktui_lite::grow::{
    self, FsKind, GrowAction, GrowPolicy, Label, PartSpec, analyze_with, load_policy_from,
    parse_table, part_dev_name, read_swap_info, sniff_fs,
};

const S: u64 = 512;

// ── 镜像构造工具 ────────────────────────────────────────────────────────

fn mbr_entry(img: &mut Vec<u8>, slot: usize, ptype: u8, start: u64, sectors: u64) {
    let off = 446 + slot * 16;
    img[off + 4] = ptype;
    img[off + 8..off + 12].copy_from_slice(&(start as u32).to_le_bytes());
    img[off + 12..off + 16].copy_from_slice(&(sectors as u32).to_le_bytes());
}

fn sign_mbr(img: &mut Vec<u8>) {
    img[510] = 0x55;
    img[511] = 0xAA;
}

fn mbr_disk(parts: &[(u8, u64, u64)], total_sectors: u64) -> Vec<u8> {
    let mut img = vec![0u8; (total_sectors * S) as usize];
    for (i, &(t, s, n)) in parts.iter().enumerate() {
        mbr_entry(&mut img, i, t, s, n);
    }
    sign_mbr(&mut img);
    img
}

/// GPT：protective MBR + header（LBA1）+ 128 条目区（LBA2 起）
fn gpt_disk(entries: &[([u8; 16], u64, u64)], total_sectors: u64, last_usable: u64) -> Vec<u8> {
    let mut img = vec![0u8; (total_sectors * S) as usize];
    mbr_entry(&mut img, 0, 0xEE, 1, (total_sectors - 1).min(u32::MAX as u64));
    sign_mbr(&mut img);

    let h = S as usize;
    img[h..h + 8].copy_from_slice(b"EFI PART");
    img[h + 64..h + 72].copy_from_slice(&last_usable.to_le_bytes());
    img[h + 72..h + 80].copy_from_slice(&2u64.to_le_bytes()); // entries start LBA
    img[h + 80..h + 84].copy_from_slice(&128u32.to_le_bytes()); // num entries
    img[h + 84..h + 88].copy_from_slice(&128u32.to_le_bytes()); // entry size

    for (i, &(guid, first, last)) in entries.iter().enumerate() {
        let off = 2 * S as usize + i * 128;
        img[off..off + 16].copy_from_slice(&guid); // type GUID（unique GUID 留零）
        img[off + 32..off + 40].copy_from_slice(&first.to_le_bytes());
        img[off + 40..off + 48].copy_from_slice(&last.to_le_bytes());
    }
    img
}

fn put_ext4(img: &mut Vec<u8>, off: u64, blocks: u64) {
    let b = off as usize;
    img[b + 0x438] = 0x53;
    img[b + 0x439] = 0xEF; // 魔数（sniff 与 superblock 校验同址）
    let sb = b + 1024;
    img[sb + 0x04..sb + 0x08].copy_from_slice(&(blocks as u32).to_le_bytes());
    img[sb + 0x18..sb + 0x1C].copy_from_slice(&0u32.to_le_bytes()); // log_bs=0 → 1K 块
}

fn put_xfs(img: &mut Vec<u8>, off: u64) {
    let b = off as usize;
    img[b..b + 4].copy_from_slice(b"XFSB");
}

fn put_ntfs(img: &mut Vec<u8>, off: u64) {
    let b = off as usize;
    img[b + 3..b + 11].copy_from_slice(b"NTFS    ");
}

fn put_fat(img: &mut Vec<u8>, off: u64) {
    let b = off as usize;
    img[b + 0x36..b + 0x39].copy_from_slice(b"FAT");
}

fn put_btrfs(img: &mut Vec<u8>, off: u64) {
    let b = off as usize;
    img[b + 0x10040..b + 0x10048].copy_from_slice(b"_BHRfS_M");
}

fn put_swap(img: &mut Vec<u8>, off: u64, uuid: &[u8; 16], label: &str) {
    let b = off as usize;
    img[b + 4086..b + 4096].copy_from_slice(b"SWAPSPACE2");
    img[b + 1036..b + 1052].copy_from_slice(uuid);
    let lab = label.as_bytes();
    img[b + 1052..b + 1052 + lab.len()].copy_from_slice(lab);
}

fn temp_img(name: &str, img: &[u8]) -> PathBuf {
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("grow-test-{uniq}-{name}.img"));
    std::fs::write(&path, img).unwrap();
    path
}

fn enabled_policy() -> GrowPolicy {
    GrowPolicy { enabled: true, part: PartSpec::Auto }
}

// ── 策略解析 ────────────────────────────────────────────────────────────

#[test]
fn policy_defaults_when_file_missing() {
    let p = load_policy_from(PathBuf::from("definitely-not-exists.conf").as_path());
    assert!(!p.enabled);
    assert_eq!(p.part, PartSpec::Auto);
}

#[test]
fn policy_parses_known_keys() {
    let path = temp_img("conf", b"enabled=1\npart=3\n");
    let p = load_policy_from(&path);
    assert!(p.enabled);
    assert_eq!(p.part, PartSpec::Number(3));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn policy_part_auto_empty_and_invalid_fallback() {
    // auto / 空值 / 非法值 → Auto；合法数字 → Number
    for (content, expect) in [
        ("enabled=1\npart=auto\n", PartSpec::Auto),
        ("enabled=1\npart=\n", PartSpec::Auto),
        ("enabled=1\npart=xyz\n", PartSpec::Auto),
        ("enabled=1\npart=5\n", PartSpec::Number(5)),
        ("enabled=1\npart=-1\n", PartSpec::Auto), // 负数解析失败 → 回退
    ] {
        let path = temp_img("conf", content.as_bytes());
        let p = load_policy_from(&path);
        assert_eq!(p.part, expect, "content: {content}");
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn policy_ignores_unknown_keys_and_comments() {
    let path = temp_img(
        "conf",
        b"# comment\nenabled=1\nunknown_key=value\npart=2\n\nnot-a-pair\n",
    );
    let p = load_policy_from(&path);
    assert!(p.enabled);
    assert_eq!(p.part, PartSpec::Number(2));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn policy_disabled_by_default_when_enabled_missing() {
    let path = temp_img("conf", b"part=2\n");
    let p = load_policy_from(&path);
    assert!(!p.enabled);
    let _ = std::fs::remove_file(&path);
}

// ── 分区表解析 ──────────────────────────────────────────────────────────

#[test]
fn parse_table_mbr_basic_and_container() {
    // p1: ext4 数据分区；p2: 扩展分区容器（type 0x05）
    let mut img = mbr_disk(&[(0x83, 2048, 4096), (0x05, 6144, 1024)], 10000);
    put_ext4(&mut img, 2048 * S, 0);
    let mut cur = Cursor::new(img);
    let t = parse_table(&mut cur).unwrap();
    assert_eq!(t.label, Label::Mbr);
    assert_eq!(t.entries.len(), 2);
    assert_eq!(t.entries[0].num, 1);
    assert_eq!(t.entries[0].first_lba, 2048);
    assert_eq!(t.entries[0].last_lba, 2048 + 4096 - 1);
    assert!(!t.entries[0].is_container);
    assert_eq!(t.entries[0].ptype, "83");
    assert!(t.entries[1].is_container);
    assert_eq!(t.entries[1].ptype, "05");
    assert!(t.gpt_meta.is_none());
}

#[test]
fn parse_table_gpt_entries_and_header_fields() {
    let linux_guid: [u8; 16] = [
        0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47,
        0x7D, 0xE4,
    ];
    let img = gpt_disk(&[(linux_guid, 2048, 6143)], 10000, 9967);
    let mut cur = Cursor::new(img);
    let t = parse_table(&mut cur).unwrap();
    assert_eq!(t.label, Label::Gpt);
    assert_eq!(t.entries.len(), 1);
    let e = &t.entries[0];
    assert_eq!(e.first_lba, 2048);
    assert_eq!(e.last_lba, 6143);
    assert_eq!(
        e.ptype,
        "0fc63daf-8483-4772-8e79-3d69d8477de4" // Linux filesystem GUID 规范形式
    );
    assert_eq!(t.gpt_meta, Some((128, 128)));
    assert_eq!(t.gpt_last_usable_lba, Some(9967));
}

#[test]
fn parse_table_superfloppy_when_no_signature() {
    let mut img = vec![0u8; 10000 * S as usize];
    put_ext4(&mut img, 0, 1000);
    let mut cur = Cursor::new(img);
    let t = parse_table(&mut cur).unwrap();
    assert_eq!(t.label, Label::None);
    assert!(t.entries.is_empty());
}

#[test]
fn parse_table_mbr_all_empty_entries_is_superfloppy() {
    let mut img = vec![0u8; 1000 * S as usize];
    sign_mbr(&mut img); // 有 0x55AA 但 4 条目全空
    let mut cur = Cursor::new(img);
    let t = parse_table(&mut cur).unwrap();
    assert_eq!(t.label, Label::None);
}

// ── fs 魔数 ────────────────────────────────────────────────────────────

#[test]
fn sniff_fs_detects_each_magic() {
    let mut img = vec![0u8; 0x10048];
    put_ext4(&mut img, 0, 0);
    assert_eq!(sniff_fs(&mut Cursor::new(img.clone()), 0), FsKind::Ext);

    let mut img = vec![0u8; 0x10048];
    put_xfs(&mut img, 0);
    assert_eq!(sniff_fs(&mut Cursor::new(img.clone()), 0), FsKind::Xfs);

    let mut img = vec![0u8; 0x10048];
    put_ntfs(&mut img, 0);
    assert_eq!(sniff_fs(&mut Cursor::new(img.clone()), 0), FsKind::Ntfs);

    let mut img = vec![0u8; 0x10048];
    put_fat(&mut img, 0);
    assert_eq!(sniff_fs(&mut Cursor::new(img.clone()), 0), FsKind::Fat);

    let mut img = vec![0u8; 0x10048];
    put_btrfs(&mut img, 0);
    assert_eq!(sniff_fs(&mut Cursor::new(img.clone()), 0), FsKind::Btrfs);

    let mut img = vec![0u8; 0x10048];
    put_swap(&mut img, 0, &[1; 16], "");
    assert_eq!(sniff_fs(&mut Cursor::new(img.clone()), 0), FsKind::Swap);

    // 空白 → Unknown
    let img = vec![0u8; 0x10048];
    assert_eq!(sniff_fs(&mut Cursor::new(img), 0), FsKind::Unknown);
}

#[test]
fn sniff_fs_respects_partition_offset() {
    let mut img = vec![0u8; 0x10048 + 0x20000];
    put_xfs(&mut img, 0x20000); // fs 魔数不在 0，在分区偏移处
    assert_eq!(sniff_fs(&mut Cursor::new(img), 0x20000), FsKind::Xfs);
}

#[test]
fn read_swap_info_extracts_uuid_and_label() {
    let mut img = vec![0u8; 8192];
    let uuid = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
        0xFF, 0x00,
    ];
    put_swap(&mut img, 0, &uuid, "myswap");
    let si = read_swap_info(&mut Cursor::new(img), 0).unwrap();
    assert_eq!(si.uuid, "112233445566778899aabbccddeeff00");
    assert_eq!(si.label, "myswap");
}

#[test]
fn read_swap_info_rejects_non_swap() {
    let img = vec![0u8; 8192];
    assert!(read_swap_info(&mut Cursor::new(img), 0).is_none());
}

// ── 分析决策 ────────────────────────────────────────────────────────────

#[test]
fn part_dev_name_naming_rules() {
    assert_eq!(part_dev_name("sda", 3), "sda3");
    assert_eq!(part_dev_name("nvme0n1", 2), "nvme0n1p2");
    assert_eq!(part_dev_name("mmcblk0", 1), "mmcblk0p1");
    assert_eq!(part_dev_name("vd b", 1), "vd b1"); // 非 nvme/mmc 前缀直接拼接
}

#[test]
fn analyze_mbr_grows_last_ext4() {
    let mut img = mbr_disk(&[(0x83, 2048, 4096)], 10000);
    put_ext4(&mut img, 2048 * S, 0);
    let path = temp_img("img", &img);

    let plan = analyze_with(&path, "sda", 10000, &enabled_policy());
    let Some(GrowAction::PartitionGrow {
        part_num, fs, surgery, expected_new_sectors, old_sectors, is_gpt, ..
    }) = plan.action
    else {
        panic!("expected PartitionGrow, got skip: {:?}", plan.skip_reason);
    };
    assert_eq!(part_num, 1);
    assert_eq!(fs, FsKind::Ext);
    assert!(surgery.is_none());
    assert!(!is_gpt);
    assert_eq!(old_sectors, 4096);
    // MBR usable_end = min(device, 2^32) = 10000 → 期望新尺寸 = 10000 − 2048
    assert_eq!(expected_new_sectors, Some(10000 - 2048));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn analyze_mbr_swap_surgery_plan() {
    // p1: ext4 root [2048, 6143]；p2: swap [6144, 7167]
    let mut img = mbr_disk(&[(0x83, 2048, 4096), (0x82, 6144, 1024)], 10000);
    put_ext4(&mut img, 2048 * S, 0);
    put_swap(&mut img, 6144 * S, &[0xAB; 16], "sw");
    let path = temp_img("img", &img);

    let plan = analyze_with(&path, "sda", 10000, &enabled_policy());
    let Some(GrowAction::PartitionGrow { part_num, fs, surgery, .. }) = plan.action else {
        panic!("expected surgery plan, got skip: {:?}", plan.skip_reason);
    };
    assert_eq!(part_num, 1); // 扩容目标是倒数第二分区（root）
    assert_eq!(fs, FsKind::Ext);
    let s = surgery.expect("surgery plan");
    assert_eq!(s.swap_num, 2);
    assert_eq!(s.root_num, 1);
    assert_eq!(s.root_first_lba, 2048);
    assert_eq!(s.root_ptype, "83");
    assert_eq!(s.swap_first_lba, 6144);
    assert_eq!(s.swap_sectors, 1024);
    assert_eq!(s.swap_ptype, "82");
    assert_eq!(s.swap_uuid, "ab".repeat(16));
    assert_eq!(s.swap_label, "sw");
    assert!(s.swap_partuuid.is_none()); // MBR 无 PARTUUID
    let _ = std::fs::remove_file(&path);
}

#[test]
fn analyze_gpt_grows_last_and_sets_gpt_flag() {
    let linux_guid: [u8; 16] = [
        0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47,
        0x77, 0xDE,
    ];
    let mut img = gpt_disk(&[(linux_guid, 2048, 6143)], 10000, 9967);
    put_ext4(&mut img, 2048 * S, 0);
    let path = temp_img("img", &img);

    let plan = analyze_with(&path, "sda", 10000, &enabled_policy());
    let Some(GrowAction::PartitionGrow {
        part_num, is_gpt, expected_new_sectors, surgery, ..
    }) = plan.action
    else {
        panic!("expected PartitionGrow, got skip: {:?}", plan.skip_reason);
    };
    assert_eq!(part_num, 1);
    assert!(is_gpt);
    assert!(surgery.is_none());
    // GPT usable_end = device − 33（128 条目 × 128B = 32 扇区 + backup header 1）
    assert_eq!(expected_new_sectors, Some(10000 - 33 - 2048));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn analyze_skips_when_no_free_space() {
    // 末分区贴到 usable_end（free=0）
    let mut img = mbr_disk(&[(0x83, 2048, 7952)], 10000);
    put_ext4(&mut img, 2048 * S, 0);
    let path = temp_img("img", &img);
    let plan = analyze_with(&path, "sda", 10000, &enabled_policy());
    assert!(plan.action.is_none());
    assert_eq!(plan.skip_reason.as_deref(), Some("no free space after last partition"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn analyze_skips_fat_last_partition() {
    let mut img = mbr_disk(&[(0x0b, 2048, 4096)], 10000);
    put_fat(&mut img, 2048 * S);
    let path = temp_img("img", &img);
    let plan = analyze_with(&path, "sda", 10000, &enabled_policy());
    assert!(plan.action.is_none());
    assert_eq!(plan.skip_reason.as_deref(), Some("FAT/exFAT cannot be resized in place"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn analyze_skips_mbr_extended_container_last() {
    let mut img = mbr_disk(&[(0x83, 2048, 4096), (0x05, 6144, 1024)], 10000);
    put_ext4(&mut img, 2048 * S, 0);
    let path = temp_img("img", &img);
    let plan = analyze_with(&path, "sda", 10000, &enabled_policy());
    assert!(plan.action.is_none());
    assert_eq!(plan.skip_reason.as_deref(), Some("MBR logical/extended not supported in v1"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn analyze_swap_only_partition_skips() {
    let mut img = mbr_disk(&[(0x82, 2048, 1024)], 10000);
    put_swap(&mut img, 2048 * S, &[1; 16], "");
    let path = temp_img("img", &img);
    let plan = analyze_with(&path, "sda", 10000, &enabled_policy());
    assert!(plan.action.is_none());
    assert_eq!(plan.skip_reason.as_deref(), Some("swap is the only partition"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn analyze_swap_last_with_fat_prev_skips() {
    // 末尾 swap，但倒数第二是 FAT → 无可扩候选
    let mut img = mbr_disk(&[(0x0b, 2048, 4096), (0x82, 6144, 1024)], 10000);
    put_fat(&mut img, 2048 * S);
    put_swap(&mut img, 6144 * S, &[1; 16], "");
    let path = temp_img("img", &img);
    let plan = analyze_with(&path, "sda", 10000, &enabled_policy());
    assert!(plan.action.is_none());
    assert_eq!(
        plan.skip_reason.as_deref(),
        Some("swap last, no growable partition before it")
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn analyze_superfloppy_ext4_filesystem_only() {
    // 无分区表，ext4 占 1000×1K，盘 10000 扇区 → 有尾部空闲
    let mut img = vec![0u8; 10000 * S as usize];
    put_ext4(&mut img, 0, 1000);
    let path = temp_img("img", &img);
    let plan = analyze_with(&path, "sda", 10000, &enabled_policy());
    let Some(GrowAction::FilesystemOnly { fs, fs_dev }) = plan.action else {
        panic!("expected FilesystemOnly, got skip: {:?}", plan.skip_reason);
    };
    assert_eq!(fs, FsKind::Ext);
    assert_eq!(fs_dev, path.display().to_string());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn analyze_superfloppy_full_skips() {
    // ext4 末尾估算 + 1MiB ≥ 盘容量 → NoUsefulSpace
    let blocks = 9000u64; // 9000 × 1K = 9,216,000 B；盘 10000×512 = 5,120,000 B
    let mut img = vec![0u8; 10000 * S as usize];
    put_ext4(&mut img, 0, blocks);
    let path = temp_img("img", &img);
    let plan = analyze_with(&path, "sda", 10000, &enabled_policy());
    assert!(plan.action.is_none());
    assert_eq!(plan.skip_reason.as_deref(), Some("no free space after filesystem"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn analyze_declared_part_mismatch_skips() {
    // 候选是分区 1，声明 part=2 → 拒绝
    let mut img = mbr_disk(&[(0x83, 2048, 4096)], 10000);
    put_ext4(&mut img, 2048 * S, 0);
    let path = temp_img("img", &img);
    let policy = GrowPolicy { enabled: true, part: PartSpec::Number(2) };
    let plan = analyze_with(&path, "sda", 10000, &policy);
    assert!(plan.action.is_none());
    assert!(plan.skip_reason.as_deref().unwrap().contains("not the growth candidate"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn analyze_declared_part_matching_candidate_proceeds() {
    let mut img = mbr_disk(&[(0x83, 2048, 4096)], 10000);
    put_ext4(&mut img, 2048 * S, 0);
    let path = temp_img("img", &img);
    let policy = GrowPolicy { enabled: true, part: PartSpec::Number(1) };
    let plan = analyze_with(&path, "sda", 10000, &policy);
    assert!(plan.action.is_some(), "declared part matching candidate should grow");
    let _ = std::fs::remove_file(&path);
}

// ── TUI 消费接口（固定 /run 路径，仅验证缺席时的安全行为） ─────────────

#[test]
fn grow_constants_ipc_paths() {
    assert_eq!(grow::STATUS_FILE, "/run/grow.status");
    assert_eq!(grow::RESULT_FILE, "/run/grow.result");
    assert_eq!(grow::LOG_FILE, "/run/grow.log");
}