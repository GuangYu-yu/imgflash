/// 磁盘逻辑扇区字节数（512B）——单一来源，disk/grow 共用
pub const SECTOR: u64 = 512;

/// 安装镜像路径（单一来源，app/init/handler 共用）
pub const IMAGE_FILE: &str = "/image/image.img";
/// 安装介质挂载点（单一来源，init/grow 共用）
pub const BOOT_MEDIA_DIR: &str = "/media/cdrom";

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: &[(u64, &str)] = &[
        (1_000_000_000_000, "TB"),
        (1_000_000_000, "GB"),
        (1_000_000, "MB"),
        (1_000, "KB"),
    ];

    for &(threshold, suffix) in UNITS {
        if bytes >= threshold {
            if bytes.is_multiple_of(threshold) {
                return format!("{}{}", bytes / threshold, suffix);
            } else {
                let val = bytes as f64 / threshold as f64;
                return format!("{:.1}{}", val, suffix);
            }
        }
    }
    format!("{}B", bytes)
}