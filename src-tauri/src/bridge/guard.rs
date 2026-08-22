//! bridge 层文件系统命令的参数白名单。
//!
//! `reveal_in_folder` / `open_dir` 接收来自前端（内嵌 iframe 的 Harness 页面，可
//! 被第三方插件注入脚本操纵）的路径参数，直接交给系统 `open`/`explorer`/`reveal`，
//! 若不加约束，任意 frame 都能驱动宿主打开任意目录/文件（例如把恶意网页路径
//! 交给系统默认处理器）。本模块把这两条命令限制在**预期根目录集合**内：
//! - 系统下载目录（Session 日志下载完成的「在文件夹中显示」）；
//! - 应用数据目录（核心版本「打开目录」、历史核心槽位、updates 安装包）；
//! - 官方 `$DSH_HOME`（用户数据目录，部分入口也指向它）。
//!
//! 实现用 canonicalize 后做前缀匹配，避免字符串前缀误判与符号链接跳出。

use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};

/// 允许打开/定位的根目录集合。
pub fn allowed_roots(app_handle: &AppHandle) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(download_dir) = app_handle.path().download_dir() {
        roots.push(download_dir);
    }
    if let Ok(data_dir) = app_handle.path().app_data_dir() {
        roots.push(data_dir);
    }
    let dsh_home = crate::config::get_dsh_data_path(app_handle);
    roots.push(dsh_home);
    roots
}

/// 实际用于前缀匹配的根：仅保留 canonicalize 后仍指向目录的根
/// （下载目录/数据目录在全新安装时可能尚未创建）。
fn allowed_root(app_handle: &AppHandle) -> Vec<std::path::PathBuf> {
    allowed_roots(app_handle)
        .into_iter()
        .filter(|root| root.is_dir())
        .collect()
}

/// `path` canonicalize 后是否位于任一允许根目录内。
pub fn is_allowed_path(app_handle: &AppHandle, path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let Ok(real) = path.canonicalize() else {
        return false;
    };
    allowed_root(app_handle)
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| real.starts_with(&root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 前缀匹配必须尊重组件边界：`/data/app2/x` 不是 `/data/app` 的子路径
    /// （字符串前缀判断易在 canonicalize 前误判；canonicalize 后 components 对齐可避免）。
    #[test]
    fn prefix_match_respects_component_boundary() {
        let inside = PathBuf::from("/data/app/dependencies/dsh");
        let root = PathBuf::from("/data/app");
        assert!(inside.starts_with(&root));
        let sibling = PathBuf::from("/data/app2/x");
        assert!(!sibling.starts_with(&root));
    }
}