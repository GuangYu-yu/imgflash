#!/bin/bash
# ===========
# ImgFlash - 创建构建模板
# ===========

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="${SCRIPT_DIR}/build.env"
[[ -f "${ENV_FILE}" ]] || { echo "错误：缺少配置文件 ${ENV_FILE}" >&2; exit 1; }
source "${ENV_FILE}"

die() { echo "错误：$*" >&2; exit 1; }

# --- DEBIAN_SUITE 留空时自动获取最新稳定版代号（dists/stable/Release 的 Codename） ---
if [[ -z "${DEBIAN_SUITE:-}" ]]; then
    DEBIAN_SUITE="$(curl -fsSL "${DEBIAN_MIRROR}/dists/stable/Release" | awk '/^Codename:/{print $2; exit}')" \
        || die "无法获取 Debian 最新稳定版代号（${DEBIAN_MIRROR}）"
    [[ -n "${DEBIAN_SUITE}" ]] || die "Debian Release 文件中未找到 Codename"
fi

# --- CLI 参数 ---
OUTPUT_NAME=""
SKIP_BOOTSTRAP=0

show_help() {
    cat <<EOF
ImgFlash - 创建构建模板

用法: $0 [选项]

选项:
  --arch          目标架构（amd64/arm64，默认 amd64）
  --secure-boot   启用 Secure Boot（默认禁用）
  --skip-bootstrap  跳过 Phase 1-2，复用已有 boot cache
  -o, --output    输出模板名称（不含 .iso 后缀）
  -h, --help      显示此帮助

环境变量:
  ARCH            目标架构（amd64/arm64）
  ENABLE_SECURE_BOOT  启用 Secure Boot（0/1）

输出:
  模板 ISO 将保存到 templates/ 目录
  默认名称格式: {arch}-template.iso 或 {arch}-secureboot-template.iso

示例:
  $0 --arch amd64 --secure-boot -o amd64-secureboot
  $0 --arch arm64
EOF
}

# --- 参数解析 ---
while [[ $# -gt 0 ]]; do
    case $1 in
        --arch) ARCH="$2"; shift 2 ;;
        --secure-boot) ENABLE_SECURE_BOOT="1"; shift ;;
        --skip-bootstrap) SKIP_BOOTSTRAP=1; shift ;;
        -o|--output) OUTPUT_NAME="$2"; shift 2 ;;
        -h|--help) show_help; exit 0 ;;
        *) echo "未知选项: $1"; show_help; exit 1 ;;
    esac
done

# --- 确定输出名称 ---
if [[ -z "${OUTPUT_NAME}" ]]; then
    OUTPUT_NAME="${ARCH}-template"
    [[ "${ENABLE_SECURE_BOOT}" == "1" ]] && OUTPUT_NAME="${ARCH}-secureboot-template"
fi

TEMPLATES_DIR="${SCRIPT_DIR}/templates"
mkdir -p "${TEMPLATES_DIR}"
FINAL_ISO="${TEMPLATES_DIR}/${OUTPUT_NAME}.iso"

# --- 架构映射 ---
case "${ARCH}" in
    amd64)
        KERNEL_PKG="linux-image-amd64"
        SHIM_PKG="shim-signed"
        GRUB_PKG="grub-efi-amd64-signed"
        SHIM_FIND="shimx64.efi.signed"
        GRUB_FIND="grubx64.efi.signed"
        EFI_SHIM_NAME="BOOTX64.EFI"
        EFI_GRUB_NAME="grubx64.efi"
        HAS_BIOS=1
        ;;
    arm64)
        KERNEL_PKG="linux-image-arm64"
        SHIM_PKG="shim-signed"
        GRUB_PKG="grub-efi-arm64-signed"
        SHIM_FIND="shimaa64.efi.signed"
        GRUB_FIND="grubaa64.efi.signed"
        EFI_SHIM_NAME="BOOTAA64.EFI"
        EFI_GRUB_NAME="grubaa64.efi"
        HAS_BIOS=0
        ;;
    *) die "不支持的架构 '${ARCH}'" ;;
esac

ISOLINUX_BIN=$(find /usr -name isolinux.bin 2>/dev/null | head -1)
LDLINUX_C32=$(find /usr -name ldlinux.c32 2>/dev/null | head -1)
ISOHDPFX_PATH=$(find /usr -name isohdpfx.bin 2>/dev/null | head -1)

# --- 构建目录 ---
BUILD_DIR="${SCRIPT_DIR}/build/${ARCH}"
ROOTFS_DIR="${BUILD_DIR}/rootfs"
INITRAMFS_DIR="${BUILD_DIR}/initramfs"
ISO_DIR="${BUILD_DIR}/iso"

# --- 退出清理 ---
BUILD_SUCCESS=0
cleanup() {
    [[ "${BUILD_SUCCESS}" -eq 0 && -d "${BUILD_DIR}" ]] && { echo "清理构建目录..."; rm -rf "${BUILD_DIR}"; }
    :
}
trap cleanup EXIT

echo ""; echo "=========================================="
echo "  ImgFlash - 创建模板"
echo "=========================================="
echo "  架构      : ${ARCH}"
echo "  Secure Boot: $([ "${ENABLE_SECURE_BOOT}" == "1" ] && echo "启用" || echo "禁用")"
echo "  输出      : ${OUTPUT_NAME}.iso"
echo "=========================================="; echo ""

# =============================================================================
# Phase 1-2: 创建 rootfs 并提取组件
# =============================================================================

if [[ "${SKIP_BOOTSTRAP}" == "1" ]]; then
    echo "[跳过] Phase 1-2：复用 ${BUILD_DIR}"
    [[ -d "${ROOTFS_DIR}" ]] || die "rootfs 不存在，无法跳过 bootstrap"

    VMLINUZ=$(ls "${ROOTFS_DIR}"/boot/vmlinuz-* 2>/dev/null | head -1)
    [[ -n "${VMLINUZ}" ]] || die "复用 rootfs 中未找到 vmlinuz"
    KVER=$(basename "${VMLINUZ}" | sed 's/^vmlinuz-//')
    echo "  内核版本：${KVER}"

    GRUB_SRC=$(find "${ROOTFS_DIR}" -name "${GRUB_FIND}" -print -quit 2>/dev/null)
    [[ -n "${GRUB_SRC}" ]] || die "复用 rootfs 中未找到 GRUB EFI 文件"

    if [[ "${ENABLE_SECURE_BOOT}" == "1" ]]; then
        SHIM_SRC=$(find "${ROOTFS_DIR}" -name "${SHIM_FIND}" -print -quit 2>/dev/null)
        [[ -n "${SHIM_SRC}" ]] || die "复用 rootfs 中未找到 shim"
    fi
else
    rm -rf "${BUILD_DIR}"
    mkdir -p "${BUILD_DIR}"

    SIGNED_PKGS="${KERNEL_PKG},${GRUB_PKG}"
    [[ "${ENABLE_SECURE_BOOT}" == "1" ]] && SIGNED_PKGS="${KERNEL_PKG},${SHIM_PKG},${GRUB_PKG}"

    echo "[Phase 1] mmdebstrap ${DEBIAN_SUITE} (${ARCH}) ..."
    mmdebstrap --variant=essential \
        --include="${SIGNED_PKGS}" \
        "${DEBIAN_SUITE}" "${ROOTFS_DIR}" "${DEBIAN_MIRROR}"
    echo "  Phase 1 完成。"

    # =============================================================================
    # Phase 2: 提取组件
    # =============================================================================
    echo ""; echo "[Phase 2] 提取组件 ..."

    VMLINUZ=$(ls "${ROOTFS_DIR}"/boot/vmlinuz-* 2>/dev/null | head -1)
    [[ -n "${VMLINUZ}" ]] || die "rootfs 中未找到 vmlinuz"
    KVER=$(basename "${VMLINUZ}" | sed 's/^vmlinuz-//')
    echo "  内核版本：${KVER}"

    GRUB_SRC=$(find "${ROOTFS_DIR}" -name "${GRUB_FIND}" -print -quit 2>/dev/null)
    [[ -n "${GRUB_SRC}" ]] || die "rootfs 中未找到 GRUB EFI 文件"

    if [[ "${ENABLE_SECURE_BOOT}" == "1" ]]; then
        SHIM_SRC=$(find "${ROOTFS_DIR}" -name "${SHIM_FIND}" -print -quit 2>/dev/null)
        [[ -n "${SHIM_SRC}" ]] || die "rootfs 中未找到 shim"
    fi

    rm -rf "${ROOTFS_DIR}/var/lib/apt/lists"/* \
           "${ROOTFS_DIR}/var/cache/apt"/*

    echo "  Phase 2 完成。"
fi

# =============================================================================
# Phase 3: 组装 initramfs
# =============================================================================
echo ""; echo "[Phase 3] 组装 initramfs ..."

rm -rf "${INITRAMFS_DIR}"
mkdir -p "${INITRAMFS_DIR}"/{bin,sbin,etc,proc,sys,dev,run,tmp}
mkdir -p "${INITRAMFS_DIR}"/{usr/bin,usr/sbin,lib}
mkdir -p "${INITRAMFS_DIR}"/{media/cdrom,image,var/log,root}
mkdir -p "${INITRAMFS_DIR}"/{dev/pts,dev/shm}

# initramfs 用户态仅 disktui-lite；内核模块由其内置加载器
# （modules.dep + finit_module）加载
[[ -f "${SCRIPT_DIR}/binaries/disktui-lite" ]] || die "找不到 disktui-lite，请先构建"
cp "${SCRIPT_DIR}/binaries/disktui-lite" "${INITRAMFS_DIR}/usr/bin/disktui-lite"
chmod +x "${INITRAMFS_DIR}/usr/bin/disktui-lite"
ln -s /usr/bin/disktui-lite "${INITRAMFS_DIR}/init"

# --- grow：fs 内核模块派生进 GROW_MODULES，随模板注入 ISO /grow/modules/<ver>/ ---
# 工具/conf 由 fast path 注入 /grow/，但 grow 专用模块树必须随模板固化
# （fast path 无模板 KVER 的 .ko）。模板仅含模块树，不含 grow 二进制。
# xfs/btrfs 在线扩容需 mount（内核驱动）；lvm 需 device-mapper
# crc32c_generic 前置于 xfs/btrfs：libcrc32c 有 softdep(pre: crc32c)，内置
# 模块加载器与 modprobe 同样不解析 modules.softdep，不显式先载则
# libcrc32c init 时找不到 "crc32c" 算法而失败
GROW_MODULES=""
if [[ "${GROW_ENABLED:-0}" == "1" ]]; then
    if tr ',' '\n' <<< "${GROW_TOOLS:-}" | grep -Fxq xfs; then
        GROW_MODULES="${GROW_MODULES} crc32c_generic xfs"
    fi
    if tr ',' '\n' <<< "${GROW_TOOLS:-}" | grep -Fxq btrfs; then
        GROW_MODULES="${GROW_MODULES} crc32c_generic btrfs"
    fi
    if tr ',' '\n' <<< "${GROW_TOOLS:-}" | grep -Fxq lvm; then
        GROW_MODULES="${GROW_MODULES} dm-mod"
    fi
fi
GROW_MODULES="${GROW_MODULES#" "}"

# 模块列表
BASE_MODULES="${MOD_FILESYSTEM} ${MOD_NLS} ${MOD_ATA} ${MOD_USB} ${MOD_CDROM} ${MOD_INPUT} ${MOD_EMMC} ${MOD_EMMC_CARDREADER} ${MOD_EMMC_USB:-}"
OPT_NVME=$([[ "${INCLUDE_NVME}" != "0" ]] && echo "${MOD_NVME}" || echo "")
OPT_VIRT=$([[ "${INCLUDE_VIRT}" != "0" ]] && echo "${MOD_VIRT}" || echo "")
REQUIRED_MODULES="${BASE_MODULES} ${OPT_NVME} ${OPT_VIRT}"

echo "${REQUIRED_MODULES}" | tr ' ' '\n' > "${INITRAMFS_DIR}/etc/modules"

echo "  精简内核模块 ..."
MOD_SRC="${ROOTFS_DIR}/lib/modules/${KVER}"
[[ -d "${MOD_SRC}" ]] || die "找不到内核模块目录 ${MOD_SRC}"

depmod -b "${ROOTFS_DIR}" "${KVER}"

echo "  正在解析模块依赖链（boot）..."
BOOT_FILES=""
for mod in ${REQUIRED_MODULES}; do
    deps=$(modprobe -d "${ROOTFS_DIR}" -S "${KVER}" --show-depends "$mod" 2>/dev/null \
        | awk '/^insmod/ {print $2}')
    BOOT_FILES="${BOOT_FILES} ${deps}"
done
BOOT_FILES=$(echo "$BOOT_FILES" | tr ' ' '\n' | sort -u | grep .)

[[ -n "$BOOT_FILES" ]] || die "modprobe 未能解析 boot 模块依赖，构建环境异常"

MOD_DEST="${INITRAMFS_DIR}/lib/modules/${KVER}"
echo "  boot 模块：$(echo "$BOOT_FILES" | wc -l) 个（含依赖）"

for mod_file in $BOOT_FILES; do
    rel_path=$(echo "$mod_file" | sed "s|${MOD_SRC}/||")
    dest_dir="${MOD_DEST}/$(dirname "$rel_path")"
    mkdir -p "$dest_dir"
    xz -dc "$mod_file" > "${dest_dir}/$(basename "${rel_path%.xz}")"
done

for f in modules.builtin modules.builtin.modinfo; do
    [ -f "${MOD_SRC}/$f" ] && cp "${MOD_SRC}/$f" "${MOD_DEST}/"
done

# grow 专用模块闭包 → 暂存，Phase4 注入模板 ISO /grow/modules/<ver>/（不进 initrd）
GROW_TREE="${BUILD_DIR}/grow-modules"
if [[ -n "${GROW_MODULES}" ]]; then
    GROW_FILES=""
    for mod in ${GROW_MODULES}; do
        deps=$(modprobe -d "${ROOTFS_DIR}" -S "${KVER}" --show-depends "$mod" 2>/dev/null \
            | awk '/^insmod/ {print $2}')
        GROW_FILES="${GROW_FILES} ${deps}"
    done
    GROW_FILES=$(echo "$GROW_FILES" | tr ' ' '\n' | sort -u | grep .)
    echo "  grow 模块：$(echo "$GROW_FILES" | wc -l) 个（含依赖，入模板 ISO /grow/modules/）"
    for mod_file in $GROW_FILES; do
        rel_path=$(echo "$mod_file" | sed "s|${MOD_SRC}/||")
        dest_dir="${GROW_TREE}/${KVER}/$(dirname "$rel_path")"
        mkdir -p "$dest_dir"
        xz -dc "$mod_file" > "${dest_dir}/$(basename "${rel_path%.xz}")"
    done
fi

cp "${VMLINUZ}"  "${BUILD_DIR}/vmlinuz"
cp "${GRUB_SRC}" "${BUILD_DIR}/grub.efi"
VMLINUZ="${BUILD_DIR}/vmlinuz"
GRUB_SRC="${BUILD_DIR}/grub.efi"

if [[ "${ENABLE_SECURE_BOOT}" == "1" ]]; then
    cp "${SHIM_SRC}" "${BUILD_DIR}/shim.efi"
    SHIM_SRC="${BUILD_DIR}/shim.efi"
fi

# initrd 独一份全量 modules.dep（depmod -b ROOTFS 已生成，含 boot + grow 全部条目，
# 是 modload 构建完整依赖 map 的唯一来源）。grow 树只放 .ko，不重复带 modules.dep。
cp "${MOD_SRC}/modules.dep" "${MOD_DEST}/modules.dep"

rm -rf "${ROOTFS_DIR}"

MOD_COUNT=$(find "${INITRAMFS_DIR}/lib/modules" -name '*.ko' | wc -l)
MOD_SIZE=$(du -sh "${INITRAMFS_DIR}/lib/modules" | awk '{print $1}')
echo "  模块：${MOD_COUNT} 个文件，${MOD_SIZE}"

echo "  创建 initramfs 归档 ..."
cd "${INITRAMFS_DIR}"
find . -print0 | sort -z | cpio --null -o -H newc --owner root:root 2>/dev/null | zstd -T0 -${ZSTD_LEVEL} > "${BUILD_DIR}/initrd.img"
cd "${SCRIPT_DIR}"

INITRD_SIZE=$(ls -lh "${BUILD_DIR}/initrd.img" | awk '{print $5}')
echo "  Initramfs 大小：${INITRD_SIZE}"

rm -rf "${INITRAMFS_DIR}"
echo "  Phase 3 完成。"

# =============================================================================
# Phase 4: 组装 ISO 结构
# =============================================================================
echo ""; echo "[Phase 4] 组装模板 ISO ..."

mkdir -p "${ISO_DIR}/boot"
mv "${VMLINUZ}" "${ISO_DIR}/boot/vmlinuz"
mv "${BUILD_DIR}/initrd.img" "${ISO_DIR}/boot/initrd.img"
# grow 工具/conf/许可证均由 fast path（build-from-template.sh）注入 ISO 根 /grow/，
# 但 grow 专用内核模块树随模板固化（fast path 无模板 KVER 的 .ko）。
# 与模板内核同源，消除模块与工具版本的脱同步风险
if [[ "${GROW_ENABLED:-0}" == "1" && -d "${GROW_TREE}" ]]; then
    mkdir -p "${ISO_DIR}/grow/modules"
    cp -a "${GROW_TREE}/." "${ISO_DIR}/grow/modules/"
fi

# --- BIOS 引导（syslinux，仅 amd64） ---
if [[ "${HAS_BIOS}" -eq 1 ]]; then
    [[ -n "${ISOLINUX_BIN}" && -n "${LDLINUX_C32}" && -n "${ISOHDPFX_PATH}" ]] || \
        die "未找到 syslinux 引导文件。请安装 syslinux-common 和 isolinux 包。"

    mkdir -p "${ISO_DIR}/boot/syslinux"
    cp "${ISOLINUX_BIN}"  "${ISO_DIR}/boot/syslinux/isolinux.bin"
    cp "${LDLINUX_C32}"   "${ISO_DIR}/boot/syslinux/ldlinux.c32"

    SYSLINUX_TIMEOUT=$(( BOOT_TIMEOUT * 10 ))

    cat > "${ISO_DIR}/boot/syslinux/syslinux.cfg" << EOF
DEFAULT imgflash
PROMPT 0
TIMEOUT ${SYSLINUX_TIMEOUT}

LABEL imgflash
  KERNEL /boot/vmlinuz
  INITRD /boot/initrd.img
  APPEND ${KERNEL_PARAMS}
EOF
fi

# --- UEFI 引导：ISO 根目录（厂商 fallback） + efi.img（El Torito 标准） ---
# ISO 根目录：EFI/BOOT/ 供固件 fallback 直接加载
# ISO 根目录：EFI/debian/grub.cfg 存放唯一真实配置
# efi.img：放引导文件 + 2 行 stub（search + configfile 指向 ISO 根的真实配置）

TIMEOUT_STYLE=$([[ "${BOOT_TIMEOUT}" -eq 0 ]] && echo "hidden" || echo "menu")

# 唯一真实配置
CONFIG_CONTENT=$(cat << CONFIG_EOF
search --no-floppy --label --set=root ${VOLUME_LABEL}
set timeout=${BOOT_TIMEOUT}
set timeout_style=${TIMEOUT_STYLE}
set default=0

menuentry "ImgFlash" {
    linux /boot/vmlinuz ${KERNEL_PARAMS}
    initrd /boot/initrd.img
}
CONFIG_EOF
)

# 1. ISO 根：EFI 引导文件 + 真实配置
mkdir -p "${ISO_DIR}/EFI/BOOT" "${ISO_DIR}/EFI/debian"

src="${GRUB_SRC}"
[[ "${ENABLE_SECURE_BOOT}" == "1" ]] && src="${SHIM_SRC}"
cp "$src" "${ISO_DIR}/EFI/BOOT/${EFI_SHIM_NAME}"
[[ "${ENABLE_SECURE_BOOT}" == "1" ]] && cp "${GRUB_SRC}" "${ISO_DIR}/EFI/BOOT/${EFI_GRUB_NAME}"

echo "${CONFIG_CONTENT}" > "${ISO_DIR}/EFI/debian/grub.cfg"

# 2. efi.img：引导文件 + stub 配置 → 指向 ISO 根的真实配置
mkdir -p "${ISO_DIR}/boot/grub"
EFI_IMG="${ISO_DIR}/boot/grub/efi.img"
EFI_FILES="${GRUB_SRC}"
[[ "${ENABLE_SECURE_BOOT}" == "1" ]] && EFI_FILES="${SHIM_SRC} ${GRUB_SRC}"

# 动态计算FAT镜像大小
FILE_SIZE_KB=$(du -skL ${EFI_FILES} 2>/dev/null | awk '{s+=$1} END {print s}')
FILE_COUNT=$(echo "${EFI_FILES}" | wc -w)
FAT_OVERHEAD=80
DIR_ENTRIES=$((FILE_COUNT * 4))
EFI_SIZE_KB=$((FILE_SIZE_KB + FAT_OVERHEAD + DIR_ENTRIES + 20))
echo "  EFI 镜像: ${EFI_SIZE_KB} KB (${FILE_COUNT} 个文件, ${FILE_SIZE_KB}KB)"

dd if=/dev/zero of="${EFI_IMG}" bs=1k count="${EFI_SIZE_KB}" 2>/dev/null
mkfs.vfat "${EFI_IMG}" >/dev/null
mmd -i "${EFI_IMG}" ::EFI ::EFI/BOOT ::EFI/debian

src="${GRUB_SRC}"
[[ "${ENABLE_SECURE_BOOT}" == "1" ]] && src="${SHIM_SRC}"
mcopy -i "${EFI_IMG}" "$src" ::EFI/BOOT/${EFI_SHIM_NAME}
[[ "${ENABLE_SECURE_BOOT}" == "1" ]] && mcopy -i "${EFI_IMG}" "${GRUB_SRC}" ::EFI/BOOT/${EFI_GRUB_NAME}

# stub：先定位 ISO 根目录，再加载真实配置
mcopy -i "${EFI_IMG}" - ::EFI/debian/grub.cfg << STUB_EOF
search --no-floppy --label --set=root ${VOLUME_LABEL}
configfile /EFI/debian/grub.cfg
STUB_EOF

echo "  Phase 4 完成。"

# =============================================================================
# Phase 5: 生成最终 ISO
# =============================================================================
echo ""; echo "[Phase 5] 生成 ISO ..."

if [[ "${HAS_BIOS}" -eq 1 ]]; then
    xorriso -as mkisofs \
        -iso-level 3 \
        -o "${FINAL_ISO}" \
        -full-iso9660-filenames \
        -volid "${VOLUME_LABEL}" \
        -isohybrid-mbr "${ISOHDPFX_PATH}" \
        -eltorito-boot boot/syslinux/isolinux.bin \
            -no-emul-boot \
            -boot-info-table \
            --eltorito-catalog boot/syslinux/boot.cat \
        -eltorito-alt-boot \
            -e boot/grub/efi.img \
            -no-emul-boot \
        -isohybrid-gpt-basdat \
        -append_partition 2 0xef "${ISO_DIR}/boot/grub/efi.img" \
        "${ISO_DIR}"
else
    xorriso -as mkisofs \
        -iso-level 3 \
        -o "${FINAL_ISO}" \
        -full-iso9660-filenames \
        -volid "${VOLUME_LABEL}" \
        -e boot/grub/efi.img \
        -no-emul-boot \
        "${ISO_DIR}"
fi

rm -rf "${ISO_DIR}"
BUILD_SUCCESS=1

echo ""; echo "=================="
echo "  模板创建完成！"
echo "=================="
echo "  产物：${FINAL_ISO} ($(du -h "${FINAL_ISO}" | awk '{print $1}'))"