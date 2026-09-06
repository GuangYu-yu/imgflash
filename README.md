<div align="center">

# ImgFlash

**将任意 `.img` 镜像打包为可引导 ISO，启动后一键写入磁盘**

<p align="center">
  <img src="https://img.shields.io/badge/language-rust-orange?style=flat-square&logo=rust" alt="Rust">
  <img src="https://img.shields.io/badge/language-bash-yellow?style=flat-square&logo=gnubash" alt="Bash">
  <img src="https://img.shields.io/badge/dockerfile-ready-blue?style=flat-square&logo=docker" alt="Dockerfile">
</p>

<p align="center">
  <a href="https://github.com/GuangYu-yu/imgflash">
    <img src="https://img.shields.io/github/stars/GuangYu-yu/imgflash?style=flat-square&label=Star&color=00ADD8&logo=github" alt="GitHub Stars">
  </a>
  <a href="https://github.com/GuangYu-yu/imgflash/forks">
    <img src="https://img.shields.io/github/forks/GuangYu-yu/imgflash?style=flat-square&label=Fork&color=00ADD8&logo=github" alt="GitHub Forks">
  </a>
</p>

<p align="center">
  <a href="https://deepwiki.com/GuangYu-yu/imgflash">
    <img src="https://deepwiki.com/badge.svg" alt="Ask DeepWiki">
  </a>
  <a href="https://zread.ai/GuangYu-yu/imgflash">
    <img src="https://img.shields.io/badge/Ask_Zread-_.svg?style=flat&color=00b0aa&labelColor=000000&logo=data%3Aimage%2Fsvg%2Bxml%3Bbase64%2CPHN2ZyB3aWR0aD0iMTYiIGhlaWdodD0iMTYiIHZpZXdCb3g9IjAgMCAxNiAxNiIgZmlsbD0ibm9uZSIgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIj4KPHBhdGggZD0iTTQuOTYxNTYgMS42MDAxSDIuMjQxNTZDMS44ODgxIDEuNjAwMSAxLjYwMTU2IDEuODg2NjQgMS42MDE1NiAyLjI0MDFWNC45NjAxQzEuNjAxNTYgNS4zMTM1NiAxLjg4ODEgNS42MDAxIDIuMjQxNTYgNS42MDAxSDQuOTYxNTZDNS4zMTUwMiA1LjYwMDEgNS42MDE1NiA1LjMxMzU2IDUuNjAxNTYgNC45NjAxVjIuMjQwMUM1LjYwMTU2IDEuODg2NjQgNS4zMTUwMiAxLjYwMDEgNC45NjE1NiAxLjYwMDFaIiBmaWxsPSIjZmZmIi8%2BCjxwYXRoIGQ9Ik00Ljk2MTU2IDEwLjM5OTlIMi4yNDE1NkMxLjg4ODEgMTAuMzk5OSAxLjYwMTU2IDEwLjY4NjQgMS42MDE1NiAxMS4wMzk5VjEzLjc1OTlDMS42MDE1NiAxNC4xMTM0IDEuODg4MSAxNC4zOTk5IDIuMjQxNTYgMTQuMzk5OUg0Ljk2MTU2QzUuMzE1MDIgMTQuMzk5OSA1LjYwMTU2IDE0LjExMzQgNS42MDE1NiAxMy43NTk5VjExLjAzOTlDNS42MDE1NiAxMC42ODY0IDUuMzE1MDIgMTAuMzk5OSA0Ljk2MTU2IDEwLjM5OTlaIiBmaWxsPSIjZmZmIi8%2BCjxwYXRoIGQ9Ik0xMy43NTg0IDEuNjAwMUgxMS4wMzg0QzEwLjY4NSAxLjYwMDEgMTAuMzk4NCAxLjg4NjY0IDEwLjM5ODQgMi4yNDAxVjQuOTYwMUMxMC4zOTg0IDUuMzEzNTYgMTAuNjg1IDUuNjAwMSAxMS4wMzg0IDUuNjAwMUgxMy43NTg0QzE0LjExMTkgNS42MDAxIDE0LjM5ODQgNS4zMTM1NiAxNC4zOTg0IDQuOTYwMVYyLjI0MDFDMTQuMzk4NCAxLjg4NjY0IDE0LjExMTkgMS42MDAxIDEzLjc1ODQgMS42MDAxWiIgZmlsbD0iI2ZmZiIvPgo8cGF0aCBkPSJNNCAxMkwxMiA0TDQgMTJaIiBmaWxsPSIjZmZmIi8%2BCjxwYXRoIGQ9Ik00IDEyTDEyIDQiIHN0cm9rZT0iI2ZmZiIgc3Ryb2tlLXdpZHRoPSIxLjUiIHN0cm9rZS1saW5lY2FwPSJyb3VuZCIvPgo8L3N2Zz4K&logoColor=ffffff" alt="zread">
  </a>
</p>

</div>

## 演示

![演示](https://github.com/user-attachments/assets/d98c549c-c015-4896-9839-631735c474e6)

## 架构

纯 initramfs-only，无 rootfs、无 overlayfs、无 init 切换：

- **amd64**：UEFI + BIOS 双启动
  - UEFI 链（Secure Boot）：shim（Microsoft 签名）→ GRUB（Debian 签名）→ vmlinuz（Debian 签名）
  - UEFI 链（非 Secure Boot）：GRUB → vmlinuz
  - BIOS 链：isolinux（syslinux）→ vmlinuz
- **arm64**：UEFI 单启动
  - UEFI 链：同 amd64，按 Secure Boot 配置决定
- **运行时**：
  - TUI 模式：`disktui-lite`（Rust）同时充当 init 和安装器
  - Shell 模式：`init.sh` → `installer.sh`（Bash）

### UEFI 引导布局

ISO 中同时存在三处 GRUB 相关文件，分工如下：

| 路径 | 作用 |
|------|------|
| `EFI/BOOT/{BOOTX64,BOOTAA64}.EFI` | 固件 fallback 直接加载的 shim/GRUB 入口 |
| `EFI/BOOT/grub{aa64,x64}.efi` | Secure Boot 模式下 shim 链式加载的 GRUB |
| `EFI/debian/grub.cfg` | 唯一真实配置（search + menuentry），匹配 GRUB 硬编码 prefix |
| `boot/grub/efi.img` | El Torito 启动镜像，内含引导文件 + stub 配置（`configfile /EFI/debian/grub.cfg`） |

## 安装器模式

| | TUI 模式（默认） | Shell 模式 |
|---|---|---|
| 实现 | `disktui-lite`（Rust + Ratatui） | `installer.sh`（Bash） |
| init | 内建 init 逻辑（替代 init.sh） | `init.sh` |
| 界面 | 终端 TUI，键盘导航 | 纯文本菜单，数字选择 |
| 确认 | 方向键选择 Yes/No | 输入大写 `YES` |
| 进度条 | 实时进度条 + 速度 + ETA | 文字进度 + 速度 |
| BusyBox | 无（内核模块由内置 Rust 加载器 modload 加载） | 系统 `busybox-static` |
| 配置 | `USE_TUI=1` | `USE_TUI=0` |

## 构建流程

| 阶段 | 说明 |
|------|------|
| Phase 1 | mmdebstrap 创建最小 Debian 环境（含引导组件） |
| Phase 2 | 提取内核 / shim（可选） / GRUB / BusyBox |
| Phase 3 | 组装 initramfs（安装器 + 内核模块 + BusyBox） |
| Phase 4 | 将镜像打包为 squashfs 容器（zstd 压缩） |
| Phase 5 | 组装 ISO 文件系统结构（UEFI 引导 + 可选 BIOS 引导） |
| Phase 6 | xorriso 生成最终 ISO |

## 使用方式

### Docker 构建（推荐）

```bash
docker build -t imgflash .

# 从本地镜像构建
docker run --rm --privileged \
  -v "$(pwd)/output:/build/output" \
  imgflash -i /path/to/image.img

# 从 URL 下载镜像并构建
docker run --rm --privileged \
  -v "$(pwd)/output:/build/output" \
  imgflash -u https://example.com/image.img.gz

# 使用自定义配置构建
docker run --rm --privileged \
  -v "$(pwd)/output:/build/output" \
  -v "$(pwd)/build.env:/build/build.env" \
  imgflash -i /path/to/image.img
```

### 本地构建（非 Docker）

需在 Debian/Ubuntu 主机上准备以下依赖：

```bash
apt-get install -y \
    mmdebstrap debian-archive-keyring \
    curl file \
    xorriso squashfs-tools mtools dosfstools syslinux-common isolinux \
    xz-utils bzip2 p7zip-full unzip zstd cpio kmod \
    busybox-static

cd disktui-lite
cargo install --locked cargo-zigbuild
cargo zigbuild --release --target x86_64-unknown-linux-musl  # 或 aarch64-unknown-linux-musl
mkdir -p ../binaries
cp target/x86_64-unknown-linux-musl/release/disktui-lite ../binaries/disktui-lite

cd ..
./build.sh -i /path/to/image.img
```

### 命令行参数

```
用法: build.sh [选项]

选项:
  -i, --image     指定本地 .img 文件路径
  -u, --url       从 URL 下载镜像文件（支持 raw / gz / xz / bz2 / zip / 7z / tar.gz / tar.xz / tar.bz2 / tar.zst / tgz）
  -n, --name      输出 ISO 名称（默认从镜像文件名推导）
  -c, --checksum  SHA256 校验值（可选，下载或本地镜像均生效）
  -h, --help      显示帮助
```

### 构建配置

所有构建参数通过 `build.env` 配置，修改后重新构建即可生效。

| 变量 | 说明 | 默认值 |
|------|------|--------|
| `ARCH` | 目标架构（amd64 / arm64） | `amd64` |
| `DEBIAN_MIRROR` | Debian 镜像源 | `https://ftp.debian.org/debian` |
| `DEBIAN_SUITE` | Debian 套件版本（留空=自动获取最新稳定版代号） | 留空（自动） |
| `VOLUME_LABEL` | ISO 卷标 | `IMGFLASH` |
| `MOD_FILESYSTEM` | boot 文件系统模块（squashfs / isofs / loop，进 initrd） | 见 build.env |
| `MOD_NLS` | NLS 字符集模块 | 见 build.env |
| `MOD_ATA` | ATA/AHCI 控制器模块 | 见 build.env |
| `MOD_USB` | USB 存储模块（含 UAS） | 见 build.env |
| `MOD_CDROM` | 光驱 / SCSI 磁盘模块 | 见 build.env |
| `MOD_INPUT` | 输入设备模块（hid / usbhid） | 见 build.env |
| `MOD_EMMC` | eMMC 核心模块 | 见 build.env |
| `MOD_EMMC_CARDREADER` | eMMC 读卡器模块 | 见 build.env |
| `MOD_EMMC_USB` | USB 外接读卡器模块（默认空） | 见 build.env |
| `MOD_NVME` | NVMe 模块 | `nvme` |
| `MOD_VIRT` | 虚拟化模块（virtio 等） | 见 build.env |
| `INCLUDE_NVME` | NVMe 模块开关 | `1` |
| `INCLUDE_VIRT` | 虚拟化模块开关 | `1` |
| `ENABLE_SECURE_BOOT` | Secure Boot 支持 | `0` |
| `USE_TUI` | 安装器模式（1=TUI / 0=Shell） | `1` |
| `GROW_ENABLED` | dd 后自动扩容（构建期开关，1=工具随构建注入 ISO `/grow/`） | `1` |
| `GROW_PART` | 指定扩容分区号（`auto`=自动选择；候选校验失败则跳过） | `auto` |
| `GROW_TOOLS` | 注入哪些文件系统扩容工具到 ISO `/grow/` 并派生对应 grow 内核模块到 `/grow/modules/`（`ext4,xfs,ntfs,btrfs,lvm` 子集；sfdisk/mkswap/partx 为核心依赖无条件注入；仅 xfs/btrfs 关联内核模块，ext4/ntfs 为纯用户态） | `"ext4,xfs,ntfs,btrfs,lvm"` |
| `BOOT_TIMEOUT` | 启动菜单超时（秒） | `0` |
| `KERNEL_PARAMS` | 内核启动参数 | `quiet` |
| `SCAN_TIMEOUT` | 启动时扫描介质的超时秒数 | `10` |
| `ZSTD_LEVEL` | zstd 压缩级别 | `19` |

## GitHub Actions CI

通过 `workflow_dispatch` 手动触发构建。

### 构建 ISO

工作流：[`.github/workflows/build.yml`](.github/workflows/build.yml)

1. 进入仓库 Actions 页面，选择 "构建 ImgFlash 安装器 ISO" 工作流
2. 填入参数：
   - **下载地址**：磁盘镜像 URL
   - **目标架构**：amd64 / arm64
   - **ISO 名称**：可选，默认从 URL 推导
   - **Secure Boot**：是否启用
   - **TUI**：是否使用 TUI 安装器
   - **释放空间**：CI 磁盘空间不足时启用
   - **不使用缓存**：强制重新构建
   - **SHA256**：可选校验值
3. 构建完成后从 Artifacts 下载 ISO

### 发布 ISO

工作流：[`.github/workflows/publish-iso.yml`](.github/workflows/publish-iso.yml)

在构建 ISO 基础上额外支持：
- 自动压缩（gz / xz / bz2 / zip / 7z）
- 创建 GitHub Release 并上传（tag 名 `<release_name>-latest`，覆盖旧版）

### 更新 BusyBox 二进制

工作流：[`.github/workflows/update-binaries.yml`](.github/workflows/update-binaries.yml)

独立编译 `busybox_MODPROBE` 静态 applet（musl 工具链）提交到 `binaries/<ARCH>/`。
注：TUI initramfs 已内置 Rust 模块加载器（modload），不再打入任何 busybox applet；
此工作流产物仅作备用保留：
- 留空版本号自动检测最新版
- 指定版本号则使用手动值

### 构建 grow 静态工具链

工作流：[`.github/workflows/grow-tools.yml`](.github/workflows/grow-tools.yml)

为自动扩容功能编译 musl 静态工具（sfdisk / mkswap / partx / e2fsck / resize2fs / xfs_growfs / ntfsresize），连同 `LICENSES.txt`（GPL 随附义务）提交到 `binaries/<ARCH>/grow/`：
- 各上游版本号可手动指定，留空用默认
- `GROW_ENABLED=1` 构建前必须先运行此工作流（缺失即构建失败）

## 自动扩容（Auto-Grow）

dd 写入成功后，安装器自动将目标盘尾部空闲空间分配给可扩容分区（末分区；若末分区为 swap，则手术重建 swap 并扩容其前一分区），并扩展其文件系统（ext4 / XFS / NTFS / Btrfs / LVM），填满"盘 > 镜像"产生的尾部空闲空间。失败或跳过仅降级为警告，永不阻塞重启。

### 常见布局与处理规则 (Auto-Grow Behavior)

程序的核心逻辑基于**"只看尾部候选分区"**的安全策略（候选 = 末分区；末分区为 swap 时取其前一分区），以确保不移动、不猜测用户意图。以下是针对常见布局的自动处理行为总结：

#### 第一梯队：极常见 (Dual/Triple Boot)
| 布局 (从首到尾) | 自动扩容行为 | 说明 |
| :--- | :--- | :--- |
| `EFI` + `Root` (ext4/XFS) | **✅ 自动扩容 Root** | 将 Root 分区扩展至盘尾。 |
| `EFI` + `Root` + `Swap` | **✅ Swap 手术，扩容 Root** | 删除 Swap → 扩 Root → 末尾重建 Swap。 |
| `EFI` + `Root` + `Data` | **✅ 自动扩容 Data** | 只看最后一个分区，Data 被扩容。**Root 保持不变**。 |
| `EFI` + `Root` + `Home` | **✅ 自动扩容 Home** | Home 作为最后一个分区被扩容。**Root 保持不变**。 |
| `EFI` + `Root` + `Swap` + `Data` | **✅ 自动扩容 Data** | Swap 在中间，不触发手术。**Root 和 Swap 都保持不变**。 |
| `EFI` + `Root` + `Swap` + `Home` | **✅ 自动扩容 Home** | Swap 在中间，Home 为最后一个分区。**Root 和 Swap 都保持不变**。 |
| `EFI` + `Root` + `Home` + `Data` | **✅ 自动扩容 Data** | 只有最后一个 Data 分区被扩容。**Root 和 Home (被夹在中间) 都保持不变**。 |

#### 第二梯队：LVM 布局
| 布局 (从首到尾) | 自动扩容行为 | 说明 |
| :--- | :--- | :--- |
| `EFI` + `LVM` (单 LV: Root) | **✅ 自动扩容 LVM** | 扩容 PV -> 扩 VG -> 分配给唯一 LV。 |
| `EFI` + `LVM` (多 LV) | **⚠️ 跳过 (Skipped)** | 无法自动分配空间给哪个 LV (例如 Root 还是 Data)，需手动配置 `lv=xxx` 指定目标。 |
| `EFI` + `Swap` + `LVM` | **✅ 自动扩容 LVM** | Swap 在中间保持不变，LVM PV 作为末分区被扩容（扩 PV → 扩 VG → 扩 LV）。多 LV 时仍需 `lv=xxx`。 |

#### 第三梯队：Btrfs / LUKS 嵌套
| 布局 (从首到尾) | 自动扩容行为 | 说明 |
| :--- | :--- | :--- |
| `EFI` + `Boot` + `Btrfs` (单设备) | **✅ 自动扩容 Btrfs** | 直接扩容 Btrfs 分区，支持在线 resize。 |
| `EFI` + `Boot` + `LUKS` (ext4/LVM/Btrfs) | **❌ 跳过 (Skipped)** | 遇到 LUKS 容器直接跳过，风险极高。 |

#### 暂不支持的复杂场景
| 布局 | 典型需求 | 状态 | 说明 |
| :--- | :--- | :--- | :--- |
| `EFI` + `Root` + `Home` | **扩 Root** | **❌ 暂不支持** | `Root` 在中间，需移动 `Home`。违反不移动原则。 |
| `EFI` + `Root` + `Swap` + `Home` | **扩 Root** | **❌ 暂不支持** | `Root` 在中间，且 `Swap` 无法作为屏障被安全跨越。 |
| `EFI` + `Root` + `Home` + `Swap` | **扩 Root / Home** | **⚠️ 部分支持** | `Swap` 在末尾，可触发 Swap 手术扩容 `Home`；但 `Root` 在中间，无法扩容。 |
| `EFI` + `Root` + `Var` + `Home` | **扩 Root / Var** | **❌ 暂不支持** | 被多分区隔离的中间分区无法扩容。 |
| `EFI` + `Root` + `Var` + `Home` + `Tmp` | **扩 Root / Var / Home** | **❌ 暂不支持** | 多层嵌套隔离，无法自动规划。 |

**核心设计哲学**：
*   **不猜测，不移动**：绝不移动已存在的有数据分区。
*   **Swap 是唯一的例外**：Swap 可以被删除和重建（因其无持久数据）。
*   **默认安全**：无法确定意图的场景，默认跳过，由用户手动处理。

### 高级配置与限制

*   **支持的文件系统**：`ext4`, `XFS`, `NTFS`, **`Btrfs` (单设备)**, **`LVM` (单 LV)**。
*   **强制指定目标**：`GROW_PART=<N>`（或 `grow.conf` 中的 `part=N`）。
    *   **限制**：指定的分区必须是**最后一个分区**，或 Swap 手术场景下的**倒数第二个分区**。否则直接跳过（Skip）。
    *   **不存在**：跨多个分区重排或多分区同时扩容的功能。
*   **跳过场景 (Skip)**：
    *   尾部空闲空间不足（< 1MiB）。
    *   文件系统不支持：FAT/exFAT、**Btrfs (多设备)**、**LVM (多 LV)**、LUKS。
    *   分区表类型不支持：MBR 扩展/逻辑分区。
    *   Swap 是最后一个分区，且前一个分区不可扩展（如 FAT）。
*   **逃生门**：内核参数 `grow=off` 可在运行期强制禁用。
*   **架构**：工具 + `grow.conf` 住 ISO `/grow/`；**LICENSES.txt 位于 `binaries/<ARCH>/grow/`**，作为随 Release 发布的源提供物，不注入 ISO。
*   **架构**：initramfs 仅含 boot 必需内核模块闭包（存储/光驱/iso9660/squashfs/loop）；grow 专用内核模块（xfs/btrfs/dm-mod）随模板固化在 ISO `/grow/modules/<ver>/` 作同源来源，fast path 依据 `GROW_TOOLS` 白名单裁剪后注入最终 ISO `/grow/modules/`（仅含实际选用的 fs 模块），运行期由 modload 双根搜索（initrd miss → ISO）按需加载，工具版本与模板内核同源解耦。

安装结果屏会显示一行扩容状态（Expanded / Skipped / Partial / Failed）；Partial 状态附自包含的手动恢复命令（重启后 `/run` 数据即失）。

## 安装器运行时

### TUI 模式

| 按键 | 功能 |
|------|------|
| `↑` / `k` | 上移 |
| `↓` / `j` | 下移 |
| `Enter` | 选择磁盘 |
| `←` / `→` / `Tab` | 切换确认按钮 |
| `r` | 刷新磁盘列表 |
| `s` | 进入 Shell |
| `?` | 帮助 |
| `q` | 退出 |
| `Esc` | 中止写入 |

### Shell 模式

1. 自动枚举可写磁盘（显示大小和型号）
2. 用户选择目标磁盘
3. 二次确认（需输入大写 `YES`）
4. dd 写入并显示实时进度
5. 写入完成后自动重启

选择 `0` 可进入 Shell 进行手动操作。