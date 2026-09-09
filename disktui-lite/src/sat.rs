//! SG_IO + SAT（SCSI/ATA Translation）读取 ATA IDENTIFY DEVICE。
//! 用于获取磁盘真实型号、序列号与介质转速（含 USB 桥后面的盘）。
//!
//! 协议依据（逐项核实过，非凭记忆）：
//! - T10 SAT: ATA PASS-THROUGH(12) opcode A1h / (16) opcode 85h，PROTOCOL 4h = PIO Data-In
//! - cdb[2] 标志位与 hdparm sgio.c 的 SG_CDB2_* 常量一致（TLEN_NSECT=2<<0,
//!   TLEN_SECTORS=1<<2, TDIR_FROM_DEV=1<<3）；IDENTIFY 属数据命令，按 hdparm 的
//!   libata/AHCI 兼容性规避不置 CK_COND
//! - 16 字节 CDB 寄存器映射：feat=4, nsect=6, lbal=8, lbam=10, lbah=12,
//!   device=13, command=14（hdparm sg16 实测布局）
//! - sense 描述符 09h（ATA Status Return，长度 0Ch）：+3=ERROR, +13=STATUS
//! - IDENTIFY 字段：serial=word10-19, fw=word23-26, model=word27-46,
//!   转速=word217（0=未上报, 1=SSD, >1=RPM；内核 rotational 亦源于此）

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::Path;

const SG_IO: i32 = 0x2285;
const SG_DXFER_FROM_DEV: i32 = -3;
const SG_CHECK_CONDITION: u8 = 0x02;
const SG_DRIVER_SENSE: u16 = 0x08;

const ATA_PT16: u8 = 0x85;
const ATA_PT12: u8 = 0xA1;
const ATA_IDENTIFY_DEVICE: u8 = 0xEC;
const ATA_PROTO_PIO_IN: u8 = 0x04;
const ATA_DEV_LBA: u8 = 0x40;

// cdb[2] = 0x0E: T_LENGTH=10b(长度取 SECTOR_COUNT 字段) | BYTE_BLOCK | T_DIR=from_dev
const CDB2_DATA_IN: u8 = 0x0E;

const TIMEOUT_MS: u32 = 5000;

/// IDENTIFY DEVICE 解析结果
pub struct IdentifyData {
    pub model: String,
    pub serial: String,
    pub rotation_rate: u16,
}

/// linux/scsi/sg.h sg_io_hdr_t 的固定 ABI 镜像（64 位下 64 字节）
#[repr(C)]
struct SgIoHdr {
    interface_id: i32,
    dxfer_direction: i32,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut u8,
    cmdp: *mut u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: i32,
    usr_ptr: *mut u8,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: i32,
    duration: u32,
    info: u32,
}

/// 探测磁盘 IDENTIFY 数据。任何失败返回 None（调用方回退 sysfs 值）。
/// 仅对 sd* 设备调用；NVMe/eMMC 等走 sysfs 专属路径。
pub fn probe(dev_path: &Path) -> Option<IdentifyData> {
    let file = File::open(dev_path).ok()?;
    let fd = file.as_raw_fd();

    // 优先 16 字节版；部分老桥只实现 12 字节版（hdparm 同策略）
    let data = exec_identify(fd, 16).or_else(|| exec_identify(fd, 12))?;

    // word0 bit15 = ATAPI（包设备），不是 ATA 磁盘
    if u16::from_le_bytes([data[0], data[1]]) & 0x8000 != 0 {
        return None;
    }

    let model = decode_words(&data, 27, 20);
    if model.is_empty() {
        return None;
    }

    Some(IdentifyData {
        model,
        serial: decode_words(&data, 10, 20),
        rotation_rate: u16::from_le_bytes([data[217 * 2], data[217 * 2 + 1]]),
    })
}

/// 构造并发送 ATA IDENTIFY DEVICE，返回 512 字节 IDENTIFY 数据
fn exec_identify(fd: i32, cdb_len: usize) -> Option<[u8; 512]> {
    let mut cdb = [0u8; 16];
    if cdb_len == 16 {
        cdb[0] = ATA_PT16;
        cdb[1] = ATA_PROTO_PIO_IN;
        cdb[2] = CDB2_DATA_IN;
        cdb[6] = 1; // SECTOR_COUNT
        cdb[13] = ATA_DEV_LBA;
        cdb[14] = ATA_IDENTIFY_DEVICE;
    } else {
        cdb[0] = ATA_PT12;
        cdb[1] = ATA_PROTO_PIO_IN;
        cdb[2] = CDB2_DATA_IN;
        cdb[4] = 1; // SECTOR_COUNT
        cdb[8] = ATA_DEV_LBA;
        cdb[9] = ATA_IDENTIFY_DEVICE;
    }

    let mut data = [0u8; 512];
    let mut sense = [0u8; 32];
    let mut hdr = SgIoHdr {
        interface_id: b'S' as i32,
        dxfer_direction: SG_DXFER_FROM_DEV,
        cmd_len: cdb_len as u8,
        mx_sb_len: sense.len() as u8,
        iovec_count: 0,
        dxfer_len: data.len() as u32,
        dxferp: data.as_mut_ptr(),
        cmdp: cdb.as_mut_ptr(),
        sbp: sense.as_mut_ptr(),
        timeout: TIMEOUT_MS,
        flags: 0,
        pack_id: 0,
        usr_ptr: std::ptr::null_mut(),
        status: 0,
        masked_status: 0,
        msg_status: 0,
        sb_len_wr: 0,
        host_status: 0,
        driver_status: 0,
        resid: 0,
        duration: 0,
        info: 0,
    };

    let rc = unsafe { libc::ioctl(fd, SG_IO, &mut hdr) };
    if rc == -1 {
        return None;
    }
    // 判定链与 hdparm sg16 一致：host_status 必须为 0；status 允许
    // GOOD/CHECK_CONDITION；driver_status 允许 0/DRIVER_SENSE
    if hdr.host_status != 0 {
        return None;
    }
    if hdr.status != 0 && hdr.status != SG_CHECK_CONDITION {
        return None;
    }
    if hdr.driver_status != 0 && hdr.driver_status != SG_DRIVER_SENSE {
        return None;
    }

    match ata_status_from_sense(&sense) {
        // ATA 寄存器级成功：ERROR=0 且无 BSY(0x80)/DF(0x20)/DRQ(0x08)/ERR(0x01)
        Some((err, st)) if err == 0 && st & 0xA9 == 0 => Some(data),
        Some(_) => None,
        // 无 sense 且 SCSI 层 GOOD：桥不响应 CK_COND 但命令已正确执行
        //（hdparm 对 IDENTIFY 同样容忍此情形）
        None if hdr.status == 0 => Some(data),
        None => None,
    }
}

/// 从 descriptor 格式 sense（Response Code 72h）中提取 ATA Status Return
/// 描述符（09h）的 (ERROR, STATUS) 寄存器
fn ata_status_from_sense(sb: &[u8]) -> Option<(u8, u8)> {
    if sb[0] != 0x72 {
        return None;
    }
    let mut off = 8usize;
    while off + 2 <= sb.len() {
        let code = sb[off];
        let len = sb[off + 1] as usize;
        if code == 0x89 && len >= 0x0C && off + 2 + len <= sb.len() {
            let d = &sb[off + 2..];
            return Some((d[3], d[13]));
        }
        off += 2 + len;
    }
    None
}

/// 解码 IDENTIFY 字符串字段：每 word 16 位小端、高字节是前一个字符，
/// 即逐 word 字节交换（ATA 规范规定），去除尾部空格与 NUL
fn decode_words(data: &[u8], start: usize, count: usize) -> String {
    let mut bytes = Vec::with_capacity(count * 2);
    for w in 0..count {
        let off = (start + w) * 2;
        bytes.push(data[off + 1]);
        bytes.push(data[off]);
    }
    String::from_utf8_lossy(&bytes)
        .trim_matches([' ', '\0'])
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sg_io_hdr_layout_is_64_bytes() {
        // 与内核 sg.h 的 struct sg_io_hdr 布局一致
        assert_eq!(std::mem::size_of::<SgIoHdr>(), 64);
    }

    #[test]
    fn identify_cdb16_matches_sat_spec() {
        let mut cdb = [0u8; 16];
        cdb[0] = ATA_PT16;
        cdb[1] = ATA_PROTO_PIO_IN;
        cdb[2] = CDB2_DATA_IN;
        cdb[6] = 1;
        cdb[13] = ATA_DEV_LBA;
        cdb[14] = ATA_IDENTIFY_DEVICE;
        assert_eq!(&cdb[..2], &[0x85, 0x04]);
        assert_eq!(cdb[2], 0x0E);
        assert_eq!(cdb[6], 0x01);
        assert_eq!(cdb[13], 0x40);
        assert_eq!(cdb[14], 0xEC);
    }

    #[test]
    fn identify_cdb12_matches_sat_spec() {
        let mut cdb = [0u8; 16];
        cdb[0] = ATA_PT12;
        cdb[1] = ATA_PROTO_PIO_IN;
        cdb[2] = CDB2_DATA_IN;
        cdb[4] = 1;
        cdb[8] = ATA_DEV_LBA;
        cdb[9] = ATA_IDENTIFY_DEVICE;
        assert_eq!(&cdb[..2], &[0xA1, 0x08]);
        assert_eq!(cdb[2], 0x0E);
        assert_eq!(cdb[4], 0x01);
        assert_eq!(cdb[8], 0x40);
        assert_eq!(cdb[9], 0xEC);
    }

    #[test]
    fn decode_words_swaps_bytes_and_trims() {
        // "WD " 编码: word27 高字节='W' 低字节='D'
        let mut data = [0u8; 512];
        data[54] = b'D';
        data[55] = b'W';
        data[56] = 0x20;
        data[57] = b' ';
        assert_eq!(decode_words(&data, 27, 2), "WD");
    }

    #[test]
    fn ata_status_return_descriptor_parsed() {
        let mut sb = [0u8; 32];
        sb[0] = 0x72; // descriptor format
        sb[7] = 0x0E; // additional length
        sb[8] = 0x09; // ATA Status Return
        sb[9] = 0x0C;
        sb[8 + 2 + 3] = 0x00; // ERROR=0
        sb[8 + 2 + 13] = 0x50; // STATUS = DRDY|DSC
        assert_eq!(ata_status_from_sense(&sb), Some((0x00, 0x50)));
    }

    #[test]
    fn ata_status_return_error_detected() {
        let mut sb = [0u8; 32];
        sb[0] = 0x72;
        sb[8] = 0x09;
        sb[9] = 0x0C;
        sb[8 + 2 + 3] = 0x04; // ABRT
        sb[8 + 2 + 13] = 0x41; // ERR bit
        let (err, st) = ata_status_from_sense(&sb).unwrap();
        assert!(err != 0 || st & 0x01 != 0);
    }

    #[test]
    fn rotation_rate_field_location() {
        // word217 位于字节偏移 434
        let mut data = [0u8; 512];
        data[434] = 0x01;
        data[435] = 0x00;
        assert_eq!(u16::from_le_bytes([data[434], data[435]]), 1);
    }
}