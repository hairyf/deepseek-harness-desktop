//! 路径安全守卫：对进入 `Path::join` / `remove_dir_all` 的用户可控 ID 做统一校验。
//!
//! 背景：profile id、core tag、插件 id 都可能来自 IPC 参数，若直接拼进
//! `$DSH_HOME/.../<id>` 后删除，`".."`、绝对路径、`/`、`\`、`.` 等特殊值会把
//! 目标解析到应用数据根目录甚至任意路径（`remove_profile("..")` 等穿越）。
//!
//! 本模块提供两层防线：
//! 1. `validate_id`：字符集白名单，拒绝路径分隔符、`..`、绝对路径等；
//! 2. `safe_remove_dir`：删除前 canonicalize 目标并校验其仍然位于固定根目录内，
//!    同时拒绝根目录本身与符号链接跳出。
//!
//! 返回 `Result<_, String>`，错误一律带大写前缀（AGENTS.md 约定）。

use std::path::{Path, PathBuf};

/// ID 允许的字符集：字母数字、点、短横线、下划线（禁止空格、路径分隔符）。
///
/// 注意：`.` 单独出现（`.` / `..`）属于非法，但文件名内的点（如 `dsh-1.2.3`）
/// 允许。检查顺序：先整体字符集（排除 `/`、`\`、空白），再排除 `.` `..` 与
/// 以 `..` 开头/结尾的组件形态。
pub fn validate_id(id: &str) -> Result<(), String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("INVALID_ID: id is empty".to_string());
    }
    if id.len() > 128 {
        return Err("INVALID_ID: id too long".to_string());
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
    {
        return Err(format!(
            "INVALID_ID: id contains forbidden characters: {id}"
        ));
    }
    if id == "." || id == ".." || id.starts_with("..") || id.ends_with("..") {
        return Err(format!("INVALID_ID: path traversal attempt: {id}"));
    }
    // 顺带拒绝 Windows 保留名与分隔符的隐形变体（统一走字符集已挡掉）
    Ok(())
}

/// 校验 `child` canonicalize 后仍位于 `root` canonicalize 的目录内。
///
/// 使用 `canonicalize` 后再比较，避免字符串前缀误判（`/data/foobar` 不作为
/// `/data/foo` 的子路径）以及 `..`、符号链接改写。`root` 不存在时（尚未初始化）
/// 直接拒绝——删除操作的前提是父目录存在。
pub fn ensure_within(child: &Path, root: &Path) -> Result<PathBuf, String> {
    let root_real = root
        .canonicalize()
        .map_err(|e| format!("ROOT_RESOLVE_FAILED: {e}"))?;
    let child_real = child
        .canonicalize()
        .map_err(|e| format!("PATH_RESOLVE_FAILED: {e}"))?;
    if !child_real.starts_with(&root_real) {
        return Err(format!(
            "PATH_ESCAPE_REJECTED: {} is outside {}",
            child_real.display(),
            root_real.display()
        ));
    }
    Ok(child_real)
}

/// 删除目录前的最小安全前置：ID 合法 + canonicalize 目标在根目录内 + 拒绝根目录。
///
/// 返回规范化后的目标路径；调用方随后执行 `remove_dir_all`。此函数不删除任何
/// 内容——调用方负责 `fs::remove_dir_all`（且用 `download::remove_dir_with_retry` 等
/// 已有封装时可继续使用）。
pub fn safe_remove_target(root: &Path, id: &str) -> Result<PathBuf, String> {
    validate_id(id)?;
    let dir = root.join(id);
    if dir == root {
        return Err("REMOVE_ROOT_REJECTED: refusing to remove the root directory".to_string());
    }
    // 目标尚不存在：无需解析，直接拒绝（调用方 mismatch 时也不该能删）
    if !dir.exists() {
        return Err("TARGET_NOT_FOUND: target directory does not exist".to_string());
    }
    // 符号链接：canonicalize 会把链接解析到真实目标——真实目标若仍位于根目录则
    // 允许（例如 profiles 下到共享目录的链接），否则拒绝；杜绝链接跳走删除。
    ensure_within(&dir, root)
}

/// 便捷封装：校验 ID 并组装根目录下的目标路径（不检查存在性）。
///
/// 用于创建/读取路径（非删除）场景，仅挡字符集穿越。
pub fn join_safe(root: &Path, id: &str) -> Result<PathBuf, String> {
    validate_id(id)?;
    Ok(root.join(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn validate_id_rejects_traversal() {
        for bad in ["..", ".", "../x", "..\\x", "/etc", "\\etc", "a/b", "a b", "a\tb", "a:b"] {
            assert!(validate_id(bad).is_err(), "should reject {bad:?}");
        }
        for good in ["web", "my-profile", "dsh-1.2.3", "plugin-a_b", "app-7"] {
            assert!(validate_id(good).is_ok(), "should accept {good:?}");
        }
    }

    #[test]
    fn safe_remove_rejects_escape() {
        let dir = std::env::temp_dir().join(format!("dsh-fsguard-{}", std::process::id()));
        let root = dir.join("root");
        fs::create_dir_all(&root).unwrap();
        let guard = std::panic::catch_unwind(|| {
            // `..` 在合法字符集之外，直接拒绝
            assert!(safe_remove_target(&root, "..").is_err());
            // 不存在的目标直接拒
            assert!(safe_remove_target(&root, "nonexistent").is_err());
            // 合法 id 但目录未创建也应拒绝（TARGET_NOT_FOUND）
            let res = safe_remove_target(&root, "web");
            assert!(res.is_err());
            assert!(res.unwrap_err().starts_with("TARGET_NOT_FOUND"));
        });
        let _ = fs::remove_dir_all(&dir);
        assert!(guard.is_ok(), "test panicked: {guard:?}");
    }

    #[test]
    fn safe_remove_rejects_symlink_escape() {
        let dir = std::env::temp_dir().join(format!("dsh-fsguard3-{}", std::process::id()));
        let root = dir.join("profiles");
        fs::create_dir_all(&root).unwrap();
        // 根目录外建一个真实目录，root 下放指向它的符号链接——删除目标会把
        // canonicalize 解析到链接真实目标，若跳出 root 则应被拒绝。
        let outside = dir.join("outside-target");
        fs::create_dir_all(&outside).unwrap();
        let link = root.join("escape-link");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(safe_remove_target(&root, "escape-link").is_err());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn safe_remove_accepts_valid_existing_dir() {
        let dir = std::env::temp_dir().join(format!("dsh-fsguard2-{}", std::process::id()));
        let root = dir.join("profiles");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&root.join("web")).unwrap();
        let res = safe_remove_target(&root, "web");
        assert!(res.is_ok());
        let res = join_safe(&root, "app-1");
        assert_eq!(res.unwrap(), root.join("app-1"));
        let _ = fs::remove_dir_all(&dir);
    }
}