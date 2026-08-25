#!/bin/bash
# ===========
# ImgFlash - 从模板快速构建
# ===========

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="${SCRIPT_DIR}/build.env"
[[ -f "${ENV_FILE}" ]] || { echo "错误：缺少配置文件 ${ENV_FILE}" >&2; exit 1; }
source "${ENV_FILE}"

die() { echo "错误：$*" >&2; exit 1; }

# --- 辅助函数 ---
verify_sha256() {
    local actual=$(sha256sum "$1" | cut -d' ' -f1)
    [[ "$actual" == "$2" ]] && { echo "  SHA256 校验通过"; return 0; }
    echo "  SHA256 校验失败！" >&2
    echo "  预期: $2" >&2
    echo "  实际: $actual" >&2
    return 1
}

retry() {
    local max="$1" delay="$2"
    shift 2
    for i in $(seq 1 "$max"); do
        "$@" && return 0
        [[ $i -eq $max ]] && die "重试 $max 次后仍失败：$*"
        echo "  第 $i/$max 次重试，${delay} 秒后..."
        sleep "$delay"
    done
}

download_image() {
    local url="$1" checksum="$2"
    echo "  正在下载：${url}"
    retry 3 5 curl -Lo "${BUILD_DIR}/downloaded_file" "$url"

    if [[ -n "$checksum" ]]; then
        echo "  正在验证 SHA256..."
        verify_sha256 "${BUILD_DIR}/downloaded_file" "$checksum" || {
            echo "  校验失败，尝试重新下载..."
            rm -f "${BUILD_DIR}/downloaded_file"
            retry 2 3 curl -Lo "${BUILD_DIR}/downloaded_file" "$url"
            verify_sha256 "${BUILD_DIR}/downloaded_file" "$checksum" || die "SHA256 校验再次失败，终止构建"
        }
    fi

    local file_type=$(file --mime-type -b "${BUILD_DIR}/downloaded_file")
    echo "  文件类型：${file_type}"

    local extracted_name=""

    case "${url}" in
        *.tar.gz|*.tar.xz|*.tar.bz2|*.tar.zst|*.tgz)
            tar -xf "${BUILD_DIR}/downloaded_file" -C "${BUILD_DIR}/"
            extracted_name=$(ls "${BUILD_DIR}"/*.img 2>/dev/null | head -1 | xargs basename)
            ;;
    esac

    if [[ -z "${extracted_name}" ]]; then
        case "${file_type}" in
            application/gzip)
                extracted_name=$(basename "$url" | sed 's/\.gz$//')
                gunzip -c "${BUILD_DIR}/downloaded_file" > "${BUILD_DIR}/${extracted_name}"
                ;;
            application/x-xz)
                extracted_name=$(basename "$url" | sed 's/\.xz$//')
                xz -dc "${BUILD_DIR}/downloaded_file" > "${BUILD_DIR}/${extracted_name}"
                ;;
            application/x-bzip2)
                extracted_name=$(basename "$url" | sed 's/\.bz2$//')
                bzip2 -dc "${BUILD_DIR}/downloaded_file" > "${BUILD_DIR}/${extracted_name}"
                ;;
            application/zip)
                unzip -j -o "${BUILD_DIR}/downloaded_file" -d "${BUILD_DIR}/"
                extracted_name=$(ls "${BUILD_DIR}"/*.img 2>/dev/null | head -n1 | xargs basename)
                ;;
            application/x-7z-compressed)
                7z x "${BUILD_DIR}/downloaded_file" -o"${BUILD_DIR}/"
                extracted_name=$(ls "${BUILD_DIR}"/*.img 2>/dev/null | head -n1 | xargs basename)
                ;;
            *)
                extracted_name=$(basename "$url")
                mv "${BUILD_DIR}/downloaded_file" "${BUILD_DIR}/${extracted_name}"
                ;;
        esac
    fi

    rm -f "${BUILD_DIR}/downloaded_file"
    [[ -n "${extracted_name}" && -f "${BUILD_DIR}/${extracted_name}" ]] || die "未找到解压后的镜像文件！"

    mv "${BUILD_DIR}/${extracted_name}" "${BUILD_DIR}/image.img"
    OUTPUT_NAME="${OUTPUT_NAME:-$(basename "${extracted_name}" .img)}"
}

# --- CLI 参数 ---
TEMPLATE_PATH=""
IMAGE_PATH=""
IMAGE_URL=""
SHA256_CHECKSUM=""
OUTPUT_NAME=""

show_help() {
    cat <<EOF
ImgFlash - 从模板快速构建 ISO

用法: $0 [选项]

选项:
  -t, --template   模板 ISO 文件路径
  -i, --image      镜像 .img 文件路径
  -u, --url        从 URL 下载镜像文件
  -c, --checksum   SHA256 校验值（可选）
  -n, --name       输出 ISO 名称（不含 .iso 后缀）
  -l, --label      卷标（默认 IMGFLASH）
  -h, --help       显示此帮助

环境变量:
  ARCH            目标架构（amd64/arm64）
  ENABLE_SECURE_BOOT  启用 Secure Boot（0/1）

自动选择模板:
  如果不指定 -t，将根据 ARCH 和 ENABLE_SECURE_BOOT 自动选择模板：
    - amd64 + secure_boot=0 → templates/amd64-template.iso
    - amd64 + secure_boot=1 → templates/amd64-secureboot-template.iso
    - arm64 + secure_boot=0 → templates/arm64-template.iso
    - arm64 + secure_boot=1 → templates/arm64-secureboot-template.iso
EOF
}

while [[ $# -gt 0 ]]; do
    case $1 in
        -t|--template) TEMPLATE_PATH="$2"; shift 2 ;;
        -i|--image) IMAGE_PATH="$2"; shift 2 ;;
        -u|--url) IMAGE_URL="$2"; shift 2 ;;
        -c|--checksum) SHA256_CHECKSUM="$2"; shift 2 ;;
        -n|--name) OUTPUT_NAME="$2"; shift 2 ;;
        -l|--label) VOLUME_LABEL="$2"; shift 2 ;;
        -h|--help) show_help; exit 0 ;;
        *) echo "未知选项: $1"; show_help; exit 1 ;;
    esac
done

# --- 构建目录 ---
BUILD_DIR="${SCRIPT_DIR}/build/template"
OUTPUT_DIR="${SCRIPT_DIR}/output"

# --- 退出清理 ---
BUILD_SUCCESS=0
cleanup() {
    [[ "${BUILD_SUCCESS}" -eq 0 && -d "${BUILD_DIR}" ]] && { echo "清理构建目录..."; rm -rf "${BUILD_DIR}"; }
    :
}
trap cleanup EXIT

# --- 验证基础输入 ---
[[ -n "${IMAGE_URL}" || -n "${IMAGE_PATH}" ]] || die "必须提供镜像路径 (-i) 或下载 URL (-u)"

# --- 自动选择模板 ---
if [[ -z "${TEMPLATE_PATH}" ]]; then
    SECURE_BOOT="${ENABLE_SECURE_BOOT:-0}"
    
    TEMPLATE_NAME="${ARCH}-template.iso"
    [[ "${SECURE_BOOT}" == "1" ]] && TEMPLATE_NAME="${ARCH}-secureboot-template.iso"
    
    TEMPLATE_PATH="${SCRIPT_DIR}/templates/${TEMPLATE_NAME}"
    echo "自动选择模板: ${TEMPLATE_NAME}"
fi

[[ -f "${TEMPLATE_PATH}" ]] || die "找不到模板文件: ${TEMPLATE_PATH}"

# --- 准备构建目录 ---
rm -rf "${BUILD_DIR}"
mkdir -p "${BUILD_DIR}"

# --- 下载镜像（如果提供URL） ---
if [[ -n "${IMAGE_URL}" ]]; then
    download_image "${IMAGE_URL}" "${SHA256_CHECKSUM}"
    IMAGE_PATH="${BUILD_DIR}/image.img"
fi

# --- 验证镜像文件 ---
[[ -n "${IMAGE_PATH}" ]] || die "必须指定镜像文件 (-i)"
[[ -f "${IMAGE_PATH}" ]] || die "找不到镜像文件: ${IMAGE_PATH}"

# --- 确定输出名称 ---
OUTPUT_NAME="${OUTPUT_NAME:-$(basename "${IMAGE_PATH}" .img)}"

# --- 统一镜像源文件名 ---
if [[ "${IMAGE_PATH}" != "${BUILD_DIR}/image.img" ]]; then
    ln -sf "${IMAGE_PATH}" "${BUILD_DIR}/image.img"
    IMAGE_PATH="${BUILD_DIR}/image.img"
fi

FINAL_ISO="${OUTPUT_DIR}/${OUTPUT_NAME}.iso"
mkdir -p "${OUTPUT_DIR}"

echo ""; echo "=========================================="
echo "  ImgFlash - 快速构建模式"
echo "=========================================="
echo "  模板    : $(basename "${TEMPLATE_PATH}")"
echo "  镜像    : ${IMAGE_PATH}"
echo "  输出    : ${OUTPUT_NAME}.iso"
echo "  卷标    : ${VOLUME_LABEL}"
echo "=========================================="; echo ""

# =============================================================================
# Phase 1: 打包用户镜像
# =============================================================================
echo "[Phase 1] 打包用户镜像 ..."

echo "  原始镜像大小：$(ls -lh "${IMAGE_PATH}" | awk '{print $5}')"

echo "  创建 squashfs（zstd）..."
mksquashfs "${IMAGE_PATH}" "${BUILD_DIR}/image.squashfs" \
    -b 1M -comp zstd -Xcompression-level ${ZSTD_LEVEL} \
    -no-fragments -no-duplicates -no-progress -no-xattrs

echo "  Squashfs 大小：$(ls -lh "${BUILD_DIR}/image.squashfs" | awk '{print $5}')"
echo "  Phase 1 完成。"

# =============================================================================
# Phase 2: 从模板构建 ISO
# =============================================================================
echo ""; echo "[Phase 2] 从模板构建 ISO ..."

# grow 全家桶（conf + 工具 + 许可证）注入 ISO 根 /grow/——与 image.squashfs 同为
# 每次构建可变内容；initramfs 只保留 grow 逻辑与 xfs.ko（模板期定型）
GROW_MAP_ARGS=()
if [[ "${GROW_ENABLED:-0}" == "1" ]]; then
    GROW_BIN_DIR="${SCRIPT_DIR}/binaries/${ARCH^^}/grow"
    [[ -d "${GROW_BIN_DIR}" ]] || die "GROW_ENABLED=1 但缺少 ${GROW_BIN_DIR}，请先运行 grow-tools workflow"

    GROW_STAGE="${BUILD_DIR}/grow"
    mkdir -p "${GROW_STAGE}"
    printf 'enabled=1\npart=%s\n' "${GROW_PART:-auto}" > "${GROW_STAGE}/grow.conf"

    # 精确条目匹配（不用 *ext4* 子串——会误配 ext4foo 之类）
    grow_tool_enabled() {
        tr ',' '\n' <<< "${GROW_TOOLS:-}" | grep -Fxq "$1"
    }

    for t in sfdisk mkswap partx; do
        [[ -f "${GROW_BIN_DIR}/${t}" ]] || die "grow 基础工具 ${t} 缺失"
        cp "${GROW_BIN_DIR}/${t}" "${GROW_STAGE}/"
    done
    if grow_tool_enabled ext4; then
        for t in e2fsck resize2fs; do
            [[ -f "${GROW_BIN_DIR}/${t}" ]] || die "GROW_TOOLS=ext4 但 ${t} 缺失"
            cp "${GROW_BIN_DIR}/${t}" "${GROW_STAGE}/"
        done
    fi
    if grow_tool_enabled xfs; then
        [[ -f "${GROW_BIN_DIR}/xfs_growfs" ]] || die "GROW_TOOLS=xfs 但 xfs_growfs 缺失"
        cp "${GROW_BIN_DIR}/xfs_growfs" "${GROW_STAGE}/"
    fi
    if grow_tool_enabled ntfs; then
        [[ -f "${GROW_BIN_DIR}/ntfsresize" ]] || die "GROW_TOOLS=ntfs 但 ntfsresize 缺失"
        cp "${GROW_BIN_DIR}/ntfsresize" "${GROW_STAGE}/"
    fi

    [[ -f "${GROW_BIN_DIR}/LICENSES.txt" ]] || die "grow LICENSES.txt 缺失（应随 grow-tools workflow 生成）"
    cp "${GROW_BIN_DIR}/LICENSES.txt" "${GROW_STAGE}/"

    GROW_MAP_ARGS=(-map "${GROW_STAGE}" /grow)

    # 粗粒度 fail-fast：覆盖值必须真实存在（sfdisk 对镜像文件可用）
    if [[ "${GROW_PART:-auto}" != "auto" ]] && command -v sfdisk &>/dev/null; then
        sfdisk -d "${IMAGE_PATH}" 2>/dev/null | grep -q "image.img${GROW_PART} :" \
            || die "GROW_PART=${GROW_PART} 在镜像中不存在"
    fi
fi

xorriso -indev "${TEMPLATE_PATH}" \
    -outdev "${FINAL_ISO}" \
    -map "${BUILD_DIR}/image.squashfs" /image.squashfs \
    "${GROW_MAP_ARGS[@]}" \
    -volid "${VOLUME_LABEL}" \
    -boot_image any replay \
    -commit

rm -rf "${BUILD_DIR}"
BUILD_SUCCESS=1

# 修正文件所有者
[ "$(uname)" = "Linux" ] && chown "${SUDO_UID:-$(id -u)}:${SUDO_GID:-$(id -g)}" "${FINAL_ISO}" 2>/dev/null || true

echo ""; echo "=================="
echo "  构建完成！"
echo "=================="
echo "  产物：${FINAL_ISO} ($(du -h "${FINAL_ISO}" | awk '{print $1}'))"