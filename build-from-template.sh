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

source "${SCRIPT_DIR}/grow-tools.sh"

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

# 探针取自模板 /grow/probe，与 initramfs 里跑的是同一个二进制。提取动作与
# GROW_ENABLED 无关：模板带不带 /grow/probe 决定产物是否需要 -rm_r 摘除——
# fast path 是模板全量复制后修改，不摘则探针会混入最终 ISO
GROW_PROBE_RM_ARGS=()
PROBE_BIN="${BUILD_DIR}/probe"
rm -f "${PROBE_BIN}"
xorriso -osirrox on -indev "${TEMPLATE_PATH}" \
    -extract /grow/probe "${PROBE_BIN}" >/dev/null 2>&1
if [[ -f "${PROBE_BIN}" ]]; then
    chmod +x "${PROBE_BIN}"
    GROW_PROBE_RM_ARGS=(-rm_r /grow/probe --)
fi

if [[ "${GROW_ENABLED:-0}" == "1" ]]; then
    GROW_BIN_DIR="${SCRIPT_DIR}/binaries/${ARCH^^}/grow"
    [[ -d "${GROW_BIN_DIR}" ]] || die "GROW_ENABLED=1 但缺少 ${GROW_BIN_DIR}，请先运行 grow-tools workflow"

    GROW_STAGE="${BUILD_DIR}/grow"
    mkdir -p "${GROW_STAGE}"
    printf 'enabled=1\npart=%s\n' "${GROW_PART:-auto}" > "${GROW_STAGE}/grow.conf"

    # 旧模板没有 /grow/probe 时由 grow_resolve_tools 回退全量
    grow_resolve_tools "${PROBE_BIN}" "${IMAGE_PATH}" "${GROW_PART:-auto}"
    rm -f "${PROBE_BIN}"
    echo "  grow 工具集：${GROW_TOOLS:-（空，镜像无需 fs 扩容工具）}"

    grow_stage_tools "${GROW_BIN_DIR}" "${GROW_STAGE}"

    GROW_MAP_ARGS=(-map "${GROW_STAGE}" /grow)

    # --- grow 内核模块：模板带全量作源，此处按 GROW_TOOLS 裁剪后注入 ---
    # 模板 /grow/modules/<ver>/ 含全量 grow 模块 .ko + 逐 fs .manifest 闭包清单。
    # fast path 无需模块源/modprobe，直接读清单做白名单拷贝：只复制 GROW_TOOLS
    # 涉及 fs 的清单并集，其余 .ko 与 .manifest 一并丢弃。裁剪在本地文件系统完成，
    # boot 元数据由主命令 -boot_image any replay 保留，不受文件树删除影响。
    GROW_MOD_STAGE=""   # set -u 安全网：先定义空，GROW_ENABLED 已含或未命中则保持空
    if [[ "${GROW_ENABLED:-0}" == "1" ]] && command -v xorriso >/dev/null; then
        MOD_SRC_X="${BUILD_DIR}/grow-modules-src"
        Gkeep="${BUILD_DIR}/grow.keep"
        CLEAN_X="${BUILD_DIR}/grow-modules-clean"
        rm -rf "${MOD_SRC_X}" "${Gkeep}" "${CLEAN_X}"
        mkdir -p "${CLEAN_X}"
        xorriso -osirrox on -indev "${TEMPLATE_PATH}" \
            -extract /grow/modules "${MOD_SRC_X}" >/dev/null 2>&1
        VER_DIR=$(find "${MOD_SRC_X}" -mindepth 1 -maxdepth 1 -type d | head -1)
        VER_NAME=$(basename "${VER_DIR}")
        if [[ -n "${VER_NAME}" && -d "${VER_DIR}/.manifest" ]]; then
            : > "${Gkeep}"
            for fs in xfs btrfs lvm; do
                if grow_tool_enabled "$fs"; then
                    L="${VER_DIR}/.manifest/${fs}"
                    [[ -f "${L}" ]] && grep -v '^$' "${L}" >> "${Gkeep}"
                fi
            done
            # 白名单拷贝：清单内相对路径（相对 <ver>）逐一复制，保持 <ver> 结构
            while IFS= read -r rel; do
                src="${VER_DIR}/${rel}"
                [[ -f "${src}" ]] || continue
                dst="${CLEAN_X}/${VER_NAME}/${rel}"
                mkdir -p "$(dirname "${dst}")"
                cp "${src}" "${dst}"
            done < "${Gkeep}"
            # 保留 grow 树 modules.dep（grow 依赖解析依赖它；多余 fs 条目无害，modload 只加载实际调用模块）
            if [[ -n "$(cat "${Gkeep}" 2>/dev/null)" ]]; then
                # 仅当裁剪后有模块保留时才 map：纯用户态 fs（ext4/ntfs 无内核模块）时 grow 树应为空，
                # 此时不 map，避免空目录注入
                mkdir -p "${CLEAN_X}/${VER_NAME}"
                [[ -f "${VER_DIR}/modules.dep" ]] && cp "${VER_DIR}/modules.dep" "${CLEAN_X}/${VER_NAME}/modules.dep"
                GROW_MOD_STAGE="${CLEAN_X}"
            fi
        fi
        rm -rf "${MOD_SRC_X}" "${Gkeep}"
    fi
fi

GROW_MOD_ARGS=(-rm_r /grow/modules --)
if [[ "${GROW_ENABLED:-0}" == "1" && -n "${GROW_MOD_STAGE}" && -d "${GROW_MOD_STAGE}" ]]; then
    GROW_MOD_ARGS+=(-map "${GROW_MOD_STAGE}" /grow/modules)
fi

xorriso -indev "${TEMPLATE_PATH}" \
    -outdev "${FINAL_ISO}" \
    -map "${BUILD_DIR}/image.squashfs" /image.squashfs \
    "${GROW_PROBE_RM_ARGS[@]}" \
    "${GROW_MAP_ARGS[@]}" \
    "${GROW_MOD_ARGS[@]}" \
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