//! 内核模块加载：解析 `/lib/modules/<ver>/modules.dep` 依赖表，
//! 按依赖序经 finit_module(2) 加载模块。
//!
//! 前提（由模板构建与 /etc/modules 清单保证）：
//! - 模块以裸 .ko 文件存在（未压缩），条目路径相对模块目录根
//! - 不处理 alias / options；符号依赖由 modules.dep 覆盖并自动按序加载，
//!   softdep（如 libcrc32c 的 pre: crc32c）不在依赖表中，需清单显式前置
//! - 模块名即 .ko 文件名 stem；`-` 与 `_` 等价（内核侧 canonical 为下划线）
//!
//! 多根搜索：依赖知识由 initrd `/lib/modules/<ver>/modules.dep` 一份全量提供
//! （含 grow 条目）；grow 阶段经 `add_media_module_root()` 追加 ISO
//! `/grow/modules/<ver>/` 作为 .ko 物理挂载点（grow 专用模块不进 initrd，见
//! build GROW_TOOLS），该根不含独立 modules.dep。finit 按下标升序找文件，
//! 跨根依赖由全量 map + 双根自动覆盖。

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use nix::kmod::{finit_module, ModuleInitFlags};

/// 模块根目录：initrd 优先，boot 介质 /grow/modules 兜底（grow 专用模块）。
/// 搜索顺序保证 boot 闭包模块留在 initrd、grow 模块从 ISO 加载时互不冲突。
const BOOT_MEDIA_MODULES_REL: &str = "grow/modules";

/// 依赖表缓存：构建一次，多次加载共用
pub struct ModuleLoader {
    /// 模块检索根（相对 modules.dep 条目的 base）。[0] 为 initrd；
    /// grow 阶段追加 ISO /grow/modules/<ver>。加载失败时按下标升序遍历。
    bases: Vec<PathBuf>,
    map: HashMap<String, Entry>,
}

struct Entry {
    /// modules.dep 内的相对路径（裸 .ko）
    path: String,
    /// 依赖的文件名 stem 列表
    deps: Vec<String>,
}

impl ModuleLoader {
    /// 读 /proc/sys/kernel/osrelease 并解析 initrd 的 modules.dep；失败即无模块可载
    pub fn new() -> Result<Self, String> {
        let ver = fs::read_to_string("/proc/sys/kernel/osrelease")
            .map_err(|e| format!("read osrelease: {e}"))?;
        let base = PathBuf::from("/lib/modules").join(ver.trim());
        let dep_text = fs::read_to_string(base.join("modules.dep"))
            .map_err(|e| format!("read modules.dep: {e}"))?;
        Ok(Self {
            bases: vec![base.clone()],
            map: parse_modules_dep(&fold_continuations(&dep_text)),
        })
    }

    /// 追加 boot 介质上的 grow 模块根（grow 专用模块的 .ko 物理挂载点）。
    /// 依赖知识（map）已由 initrd 的『全量 modules.dep』完整提供，含 grow 条目；
    /// grow 树只放 .ko，不重复带 modules.dep。此处仅追加根用于 finit 找文件。
    /// 仅 grow 阶段调用——此时 BOOT_MEDIA_DIR 已挂载。
    pub fn add_media_module_root(&mut self) {
        let Some(ver) = self.bases[0].file_name().and_then(|s| s.to_str()).map(String::from) else {
            return;
        };
        let secondary = PathBuf::from(crate::utils::BOOT_MEDIA_DIR)
            .join(BOOT_MEDIA_MODULES_REL).join(&ver);
        // 幂等：同一根不重复追加
        if !self.bases.contains(&secondary) {
            self.bases.push(secondary);
        }
    }

    /// 单模块入口：依赖先载，已加载则跳过（幂等）。
    /// 模块不存在或加载失败返回 Err
    pub fn load(&self, name: &str) -> Result<(), String> {
        let mut order = Vec::new();
        let mut visited = HashSet::new();
        let mut on_stack = HashSet::new();
        self.resolve_order(name, &mut order, &mut visited, &mut on_stack)?;
        for stem in order {
            if self.loaded(&stem) {
                continue;
            }
            self.finit(&stem)?;
        }
        Ok(())
    }

    /// 展开依赖序加载序列。on_stack 检测环（须先于 visited 判断——
    /// 环边上的节点必然已在 visited 中，顺序颠倒会使环静默通过）；
    /// visited 保证菱形依赖只出现一次
    fn resolve_order(
        &self,
        name: &str,
        order: &mut Vec<String>,
        visited: &mut HashSet<String>,
        on_stack: &mut HashSet<String>,
    ) -> Result<(), String> {
        let stem = self.lookup_stem(name)?;
        if on_stack.contains(&stem) {
            return Err(format!("dependency cycle at '{stem}'"));
        }
        if !visited.insert(stem.clone()) {
            return Ok(());
        }
        on_stack.insert(stem.clone());
        let deps = self.map.get(&stem).map(|e| e.deps.clone()).unwrap_or_default();
        for dep in deps {
            self.resolve_order(&dep, order, visited, on_stack)
                .map_err(|e| format!("dep of {stem}: {e}"))?;
        }
        on_stack.remove(&stem);
        order.push(stem);
        Ok(())
    }

    /// 依赖表以 canonical 名为键，直接按 canonical 查询
    fn lookup_stem(&self, name: &str) -> Result<String, String> {
        let stem = canonical(name);
        if self.map.contains_key(&stem) {
            Ok(stem)
        } else {
            Err(format!("'{name}' not in modules.dep"))
        }
    }

    fn loaded(&self, name: &str) -> bool {
        Path::new("/sys/module").join(canonical(name)).exists()
    }

    fn finit(&self, stem: &str) -> Result<(), String> {
        let Some(entry) = self.map.get(stem) else {
            return Err(format!("'{stem}' not in modules.dep"));
        };
        // 按下标顺序扫描多根：initrd 无此 .ko 时落到 ISO /grow/modules/<ver>
        for base in &self.bases {
            let full_path = base.join(&entry.path);
            let Ok(file) = File::open(&full_path) else {
                continue;
            };
            // 无模块参数；EEXIST 表示模块已在内核中，同样视为成功
            if let Err(e) = finit_module(&file, c"", ModuleInitFlags::empty()) {
                match e {
                    nix::errno::Errno::EEXIST => return Ok(()),
                    _ => return Err(format!("finit_module {}: {e}", full_path.display())),
                }
            }
            return Ok(());
        }
        Err(format!("kext '{}' not found in any module root", entry.path))
    }
}

/// 内核模块 canonical 名（`-` 以 `_` 形式注册于 /sys/module）
fn canonical(name: &str) -> String {
    name.replace('-', "_")
}

/// depmod(8) 输出："<相对路径>: <依赖相对路径>..."，依赖空格分隔；
/// 索引键 = .ko 文件名 stem 的 canonical 形式（下划线）
fn parse_modules_dep(text: &str) -> HashMap<String, Entry> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let Some((path, deps)) = line.split_once(':') else { continue };
        let path = path.trim();
        let deps = deps.split_whitespace().map(stem_of).collect();
        map.insert(canonical(&stem_of(path)), Entry { path: path.to_string(), deps });
    }
    map
}

/// depmod 对超长行以 "反斜杠+换行" 折行，拼回完整行
fn fold_continuations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        match line.strip_suffix('\\') {
            Some(stripped) => {
                out.push_str(stripped.trim_end());
                out.push(' ');
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

fn stem_of(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_suffix(".ko"))
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loader(text: &str) -> ModuleLoader {
        ModuleLoader {
            bases: vec![PathBuf::from("/lib/modules/test")],
            map: parse_modules_dep(&fold_continuations(text)),
        }
    }

    #[test]
    fn parse_plain_line_and_deps() {
        let l = loader(
            "kernel/fs/xfs/xfs.ko: kernel/lib/crc32c.ko kernel/crypto/crc32c_generic.ko\n\
             kernel/lib/crc32c.ko:\n",
        );
        let entry = l.map.get("xfs").unwrap();
        assert_eq!(entry.path, "kernel/fs/xfs/xfs.ko");
        assert_eq!(entry.deps, &["crc32c", "crc32c_generic"]);
    }

    #[test]
    fn parse_folded_continuation_lines() {
        let l = loader(
            "kernel/fs/btrfs/btrfs.ko: kernel/lib/zlib.ko \\\n\tkernel/lib/xxhash.ko \\\n\tkernel/crypto/sha256.ko\nkernel/fs/ext4/ext4.ko:\n",
        );
        let entry = l.map.get("btrfs").unwrap();
        assert_eq!(entry.deps, &["zlib", "xxhash", "sha256"]);
        assert!(l.map.contains_key("ext4"));
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let l = loader("this line has no colon\n\nkernel/a.ko:\n");
        assert_eq!(l.map.len(), 1);
        assert!(l.map.contains_key("a"));
    }

    #[test]
    fn diamond_dependency_appears_once() {
        let l = loader(
            "a.ko: c.ko b.ko\nb.ko: c.ko\nc.ko:\n",
        );
        let mut order = Vec::new();
        l.resolve_order("a", &mut order, &mut HashSet::new(), &mut HashSet::new()).unwrap();
        assert_eq!(order, &["c", "b", "a"]);
    }

    #[test]
    fn cycle_is_detected() {
        let l = loader("a.ko: b.ko\nb.ko: a.ko\n");
        let mut order = Vec::new();
        let err = l
            .resolve_order("a", &mut order, &mut HashSet::new(), &mut HashSet::new())
            .unwrap_err();
        assert!(err.contains("cycle"), "unexpected error: {err}");
    }

    #[test]
    fn dash_underscore_names_resolve() {
        let l = loader("kernel/drivers/md/dm-mod.ko:\n");
        // 索引键为 canonical（下划线），两种输入形态均可命中
        assert_eq!(l.lookup_stem("dm_mod").unwrap(), "dm_mod");
        assert_eq!(l.lookup_stem("dm-mod").unwrap(), "dm_mod");
        assert_eq!(canonical("dm-mod"), "dm_mod");
    }

    #[test]
    fn stem_of_various() {
        assert_eq!(stem_of("kernel/fs/xfs/xfs.ko"), "xfs");
        assert_eq!(stem_of("dm-mod.ko"), "dm-mod");
        assert_eq!(stem_of("noext"), "noext");
    }
}