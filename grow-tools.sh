#!/bin/bash
# grow 载荷的单一来源：fs → 二进制 / 内核模块 的映射，以及工具集的解析。
# build.sh、build-from-template.sh、create-template.sh 都 source 本文件；
# 只定义函数与变量，source 时不产生副作用。调用方需已定义 die()。
#
# 语义边界：GROW_TOOLS 是构建期能力（打包哪些 fs 工具），永不进 grow.conf；
# sfdisk/mkswap/partx 是 grow 核心依赖，与该变量无关，恒打包。
#
# LVM 目标要额外依赖构建容器里的 losetup 与 lvm（--privileged 下才有 loop/dm）

# 全量 fs 工具集：探针不可用时的回退值，也是模板侧（与具体镜像无关）的取值
GROW_TOOLS_FULL="ext4,xfs,ntfs,btrfs,lvm,f2fs"

# LVM 目标的候选集合：PV 在、但内层 LV 未能识别时的回退。对齐运行期 resize_lvm
# 的白名单（ext/xfs/btrfs/f2fs）——不含 ntfs，LV 上的 NTFS 运行期直接拒绝
GROW_TOOLS_LVM_FALLBACK="lvm,ext4,xfs,btrfs,f2fs"

# GROW_TOOLS 条目精确匹配（不做子串，避免 ext4 误配 ext4foo）。读全局 GROW_TOOLS
grow_tool_enabled() {
    tr ',' '\n' <<< "${GROW_TOOLS:-}" | grep -Fxq "${1:-}"
}

# fs 条目 → 打包的二进制（不含恒打的 sfdisk/mkswap/partx）
grow_bins_of() {
    case "${1:-}" in
        ext4)  printf '%s\n' e2fsck resize2fs ;;
        xfs)   printf '%s\n' xfs_growfs ;;
        ntfs)  printf '%s\n' ntfsresize ;;
        btrfs) printf '%s\n' btrfs ;;
        f2fs)  printf '%s\n' fsck.f2fs resize.f2fs ;;
        lvm)   printf '%s\n' lvm ;;
    esac
}

# fs 条目 → 随载荷注入的内核模块。xfs/btrfs 前置 crc32c_generic：
# libcrc32c 有 softdep(pre: crc32c)，内置加载器与 modprobe 都不解析
# modules.softdep，不显式先载则 libcrc32c init 时找不到 "crc32c" 算法
grow_modules_of() {
    case "${1:-}" in
        xfs)   printf '%s\n' crc32c_generic xfs ;;
        btrfs) printf '%s\n' crc32c_generic btrfs ;;
        lvm)   printf '%s\n' dm-mod ;;
    esac
}

# GROW_TOOLS → 内核模块清单（空白分隔，供 modprobe 闭包解析迭代）
grow_module_list() {
    local fs mods=""
    for fs in ext4 xfs ntfs btrfs f2fs lvm; do
        grow_tool_enabled "$fs" || continue
        mods="${mods} $(grow_modules_of "$fs")"
    done
    echo ${mods}
}

# 探针输出的 fs 名 → GROW_TOOLS 条目。lvm 不在此列：它是"PV 在、内层 fs 待识别"
# 的中间态，由 grow_resolve_tools 接上 grow_lvm_inner_fs 后拼出最终清单
grow_tools_for_fs() {
    case "${1:-}" in
        ext)   echo ext4 ;;
        xfs)   echo xfs ;;
        ntfs)  echo ntfs ;;
        btrfs) echo btrfs ;;
        f2fs)  echo f2fs ;;
        *)     echo "" ;;   # unknown：只需分区级扩容，无 fs 工具
    esac
}

# 摘掉循环设备，并先卸下其上被激活的 VG。清理尽力而为：容器一次性使用，
# 失败也不影响宿主
grow_lvm_release() {
    [[ -n "${1:-}" ]] || return 0
    vgchange -an --devices "$1" --nohints >/dev/null 2>&1 || true
    losetup -d "$1" >/dev/null 2>&1 || true
}

# 识别 LVM 目标的内层文件系统。离线态直接可读：把镜像里目标分区那一段字节用
# 循环设备接出来（PV 的 label 就在该分区起始处），激活其中的 VG，再对 LV 设备
# 跑同一个探针——LV 内没有分区表，走 superfloppy 路径，报出的即内层 fs。
# $1 = 探针，$2 = 镜像，$3 = 目标分区在镜像内的字节偏移（0 = 整盘）。
# 任何一步不成立都返回空串交给调用方回退；本函数不 die，保证循环设备一定被摘下
grow_lvm_inner_fs() {
    local probe_bin="$1" image="$2" offset="$3"
    local loop="" out="" count="" attr="" dm="" fs="" pout=""
    # 不假设 loop 模块已加载（runner 无此契约）；已内置或已加载时该命令同样无害
    modprobe loop >/dev/null 2>&1 || true
    if [[ "${offset}" == "0" ]]; then
        loop="$(losetup -f --show "${image}" 2>/dev/null || true)"
    else
        loop="$(losetup -f --show -o "${offset}" "${image}" 2>/dev/null || true)"
    fi
    if [[ -n "${loop}" ]]; then
        # --devices 把本次命令可见的设备限定到这一个循环设备（覆盖 devices file），
        # --nohints 不借 hints 定位 PV：两者合起来保证不碰镜像之外的其它 PV
        pvscan --devices "${loop}" --nohints >/dev/null 2>&1 || true
        vgscan --devices "${loop}" --nohints >/dev/null 2>&1 || true
        if vgchange -ay --devices "${loop}" --nohints >/dev/null 2>&1; then
            out="$(lvs --noheadings -o lv_name,lv_attr,lv_dm_path --devices "${loop}" --nohints 2>/dev/null || true)"
            count="$(grep -c '[^[:space:]]' <<< "${out}" || true)"
            # 选择规则与运行期同构：单 LV 自动；多 LV 时运行期会因缺少 grow.conf
            # `lv=` 声明而拒绝扩容，故此处不识别；thin pool（lv_attr 首字符 t）
            # 标记为 thin 交调用方——扩池数据区即达成目标，无 fs 需要识别
            if [[ "${count}" == "1" ]]; then
                attr="$(awk '{print $2}' <<< "${out}")"
                if [[ "${attr:0:1}" == "t" ]]; then
                    fs="thin"
                else
                    dm="$(awk '{print $3}' <<< "${out}")"
                    if [[ -n "${dm}" ]]; then
                        pout="$("${probe_bin}" --probe "${dm}" auto 2>/dev/null || true)"
                        fs="$(awk -F= '$1=="fs"{print $2}' <<< "${pout}")"
                    fi
                fi
            fi
        fi
        grow_lvm_release "${loop}"
    fi
    printf '%s' "${fs}"
}

# 解析要打包的 fs 工具集：纯自动，没有配置项——由探针（$1）分析镜像（$2）得出。
# $3 = GROW_PART（auto 或分区号），与运行期 grow.conf 的 part= 取同一值。
# 探针不可用或执行失败 → 回退全量（旧模板未含 --probe 时不打断构建）。
# 声明了分区号而探针判其不是扩容候选 → die，把错配挡在构建期
# LVM 目标由 grow_lvm_inner_fs 离线识别内层 fs，识别不出 → 回退候选集合
grow_resolve_tools() {
    local probe_bin="$1" image="$2" part="$3"
    if [[ ! -x "${probe_bin}" ]]; then
        echo "  警告：探针不可用（${probe_bin}），GROW_TOOLS 回退全量" >&2
        echo "${GROW_TOOLS_FULL}"
        return 0
    fi
    local out
    if ! out="$("${probe_bin}" --probe "${image}" "${part}")"; then
        echo "  警告：探针执行失败，GROW_TOOLS 回退全量（要求各 fs 工具二进制齐备）" >&2
        echo "${GROW_TOOLS_FULL}"
        return 0
    fi
    local ok="" fs="" reason="" offset="" line
    while IFS= read -r line; do
        case "${line%%=*}" in
            ok) ok="${line#*=}" ;;
            fs) fs="${line#*=}" ;;
            reason) reason="${line#*=}" ;;
            offset_bytes) offset="${line#*=}" ;;
        esac
    done <<< "${out}"
    if [[ "${ok}" == "1" ]]; then
        if [[ "${fs}" != "lvm" ]]; then
            grow_tools_for_fs "${fs}"
            return 0
        fi
        # LVM 目标：内层 fs 离线识别（循环设备接出该分区 → 激活 VG → 同一个探针读 LV）。
        # 内层在运行期白名单（ext/xfs/btrfs/f2fs）内 → 打对应 fs 工具；
        # thin pool（扩池即止）或白名单外的内层（运行期拒绝内层 resize）→ 只需 lvm；
        # 识别不出时回退到运行期白名单全集：多打只是体积，少打会让扩容在 lvextend
        # 之后半途失败（Partial + 手动命令）
        local inner="" inner_tools=""
        inner="$(grow_lvm_inner_fs "${probe_bin}" "${image}" "${offset:-0}")"
        if [[ -z "${inner}" ]]; then
            echo "  警告：未识别出 LVM 内层文件系统，按候选集合打包" >&2
            echo "${GROW_TOOLS_LVM_FALLBACK}"
            return 0
        fi
        case "${inner}" in
            ext | xfs | btrfs | f2fs)
                inner_tools="$(grow_tools_for_fs "${inner}")"
                echo "lvm${inner_tools:+,${inner_tools}}"
                ;;
            *)
                echo "lvm"
                ;;
        esac
        return 0
    fi
    if [[ "${part}" != "auto" ]]; then
        die "GROW_PART=${part} 不是该镜像的扩容候选：${reason:-unknown}"
    fi
    echo ""   # 镜像本无可扩目标（不支持的 fs / 无剩余空间）→ 无 fs 工具
}

# 拷贝 grow 载荷二进制。$1 = 源目录（binaries/<ARCH>/grow），$2 = 目标目录。
# 基础工具恒打，fs 工具按 GROW_TOOLS 逐条展开
grow_stage_tools() {
    local src="$1" dst="$2" t fs
    for t in sfdisk mkswap partx; do
        [[ -f "${src}/${t}" ]] || die "grow 基础工具 ${t} 缺失（${src}）"
        cp "${src}/${t}" "${dst}/"
    done
    for fs in ext4 xfs ntfs btrfs f2fs lvm; do
        grow_tool_enabled "$fs" || continue
        for t in $(grow_bins_of "$fs"); do
            [[ -f "${src}/${t}" ]] || die "GROW_TOOLS 含 ${fs} 但 ${t} 缺失（${src}）"
            cp "${src}/${t}" "${dst}/"
        done
    done
}