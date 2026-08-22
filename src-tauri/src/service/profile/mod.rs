//! 档案管理。
//!
//! 档案 = `$DSH_HOME/profiles/<id>` 目录，与官方 dsh CLI 的 profile 语义一致
//! （`dsh --profile <id>` 启动 / `dsh plugin --profile <id>` 管理插件）。
//! 桌面端把「当前使用哪个档案」持久化在自己的 store 设置（`active_profile`，
//! 默认 `web`），服务启动、插件安装/升级/卸载全部以它为准——不再写死 web。
//!
//! 新建档案时按官方 `dsh-app-boot` 的 `initProfile` 形态初始化目录：
//! `package.json`（含 web 模板 bundles）+ `cordis.patch.yml` + `pnpm-workspace.yaml`，
//! 与 CLI 侧产物完全一致，两边可互相操作。

use crate::config;
use crate::service::fs_guard;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use tauri::AppHandle;

/// 桌面端默认档案（内置，不可删除）
pub const DEFAULT_PROFILE: &str = "web";

/// 新建档案的初始 bundles：web 模板（`@deepseek-ai/dsh-base` +
/// `@deepseek-ai/dsh-web-app`，与 dsh-app-boot `PROFILE_TEMPLATES.web` 一致）。
/// 桌面端内嵌的是 dsh web 应用，新档案不带 `dsh-web-app` 将无法渲染任何界面。
const WEB_PROFILE_BUNDLES: [&str; 2] = ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"];

/// dsh `initProfile` 生成的空 patch 层（与官方一致）
const PROFILE_PATCH_TEMPLATE: &str = "# Your patch layer for this dsh profile, applied after every bundle layer:\n# a top-level YAML array of loader patch entries (id-targeted config\n# overrides, disables, and insert lists; `!!js` expressions allowed).\n[]\n";

/// dsh `initProfile` 生成的 pnpm 设置（与官方一致）
const PROFILE_PNPM_WORKSPACE: &str = "packages:\n  - .\n\nnodeLinker: hoisted\nautoInstallPeers: false\n";

/// 档案行（序列化 camelCase 给前端）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    /// 档案 id（目录名，npm 包名语义）
    pub id: String,
    /// 展示名（manifest.name 去 `dsh-profile-` 前缀，缺失回落 id）
    pub name: String,
    /// 是否桌面端内置默认档案（web）
    pub default: bool,
    /// 是否当前使用中的档案
    pub active: bool,
}

/// 指定档案的目录（`$DSH_HOME/profiles/<id>`）
pub fn profile_dir_of(app_handle: &AppHandle, id: &str) -> PathBuf {
    config::get_dsh_data_path(app_handle)
        .join("profiles")
        .join(id)
}

/// 当前使用的档案 id。
///
/// 读取桌面端持久化的 `active_profile`；若记录的档案目录已不存在（被删除/外部
/// 清理），回退默认 web。全新机器上 `profiles/` 尚未初始化时同样回退 web
/// （web 由 dsh 启动/插件操作时按需初始化）。
pub fn active_profile(app_handle: &AppHandle) -> String {
    let stored = config::get_store_dat_setting(app_handle).active_profile;
    if !stored.is_empty() && stored != DEFAULT_PROFILE && profile_dir_of(app_handle, &stored).is_dir()
    {
        stored
    } else {
        DEFAULT_PROFILE.to_string()
    }
}

/// 读取档案 manifest 的展示名：`dsh-profile-<id>` → `<id>`（首字母大写）。
fn manifest_display_name(dir: &Path, id: &str) -> String {
    let raw = fs::read_to_string(dir.join("package.json"))
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(String::from))
        .unwrap_or_default();
    let stripped = raw
        .strip_prefix("dsh-profile-")
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| raw);
    let fallback = id.to_string();
    let name = if stripped.is_empty() { fallback } else { stripped };
    // 首字母大写，与既有「Web」展示风格一致
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => name,
    }
}

/// 档案列表（含 active/default 标记）。web 未初始化（全新安装）时也展示默认档案。
pub fn list(app_handle: &AppHandle) -> Vec<Profile> {
    let active = active_profile(app_handle);
    let profiles_root = config::get_dsh_data_path(app_handle).join("profiles");
    let mut out: Vec<Profile> = Vec::new();
    if let Ok(entries) = fs::read_dir(&profiles_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(id) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // 跳过隐藏/系统目录（如 node_modules 回退链接区、.dsh 内部目录）
            if id.starts_with('.') || id == "node_modules" {
                continue;
            }
            out.push(Profile {
                id: id.to_string(),
                name: manifest_display_name(&path, id),
                default: id == DEFAULT_PROFILE,
                active: id == active,
            });
        }
    }
    if !out.iter().any(|p| p.id == DEFAULT_PROFILE) {
        out.push(Profile {
            id: DEFAULT_PROFILE.to_string(),
            name: "Web".to_string(),
            default: true,
            active: active == DEFAULT_PROFILE,
        });
    }
    // 稳定排序：默认档案在前，其余按 id 字典序
    out.sort_by_key(|p| (!p.default, p.id.clone()));
    out
}

/// 把展示名规范为档案 id：小写、非字母数字转 `-`（连续分隔符合并）、去首尾 `-`。
fn normalize_profile_id(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending_sep = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.push('-');
            }
            pending_sep = false;
            out.push(c);
        } else if c == ' ' || c == '-' || c == '_' {
            pending_sep = true;
        }
        // 其余字符（中文/符号）丢弃
    }
    out.trim_matches('-').to_string()
}

/// 新建档案：初始化 `$DSH_HOME/profiles/<id>`（manifest + patch + pnpm 设置）。
pub fn create(app_handle: &AppHandle, name: &str) -> Result<Profile, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("PROFILE_EMPTY_NAME: profile name is empty".to_string());
    }
    let id = normalize_profile_id(trimmed);
    if id.is_empty() {
        return Err("PROFILE_INVALID_NAME: profile name has no usable characters".to_string());
    }
    if id.len() > 64 {
        return Err("PROFILE_NAME_TOO_LONG: profile id exceeds 64 characters".to_string());
    }
    if id == DEFAULT_PROFILE {
        return Err("PROFILE_RESERVED: this name is reserved".to_string());
    }
    let dir = profile_dir_of(app_handle, &id);
    if dir.is_dir() {
        return Err(format!("PROFILE_EXISTS: profile {id} already exists"));
    }
    init_profile_dir(&dir, &id)?;
    Ok(Profile {
        id,
        name: trimmed.to_string(),
        default: false,
        active: false,
    })
}

/// 切换当前使用中的档案（持久化到桌面端 store）。
pub fn set_active(app_handle: &AppHandle, id: &str) -> Result<Profile, String> {
    // 路径安全：拒绝 `..`、绝对路径、分隔符（防御式——id 理论上来自
    // normalize 产物，但 CLI/配置可能把任意字符串塞进设置）
    fs_guard::validate_id(id)?;
    let dir = profile_dir_of(app_handle, id);
    if id != DEFAULT_PROFILE && !dir.is_dir() {
        return Err(format!("PROFILE_NOT_FOUND: profile {id} does not exist"));
    }
    let mut setting = config::get_store_dat_setting(app_handle);
    setting.active_profile = id.to_string();
    config::set_store_dat_setting(app_handle, setting);
    list(app_handle)
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| "PROFILE_NOT_FOUND: profile disappeared after switch".to_string())
}

/// 删除档案（默认档案与使用中的档案不可删除）。
pub fn remove(app_handle: &AppHandle, id: &str) -> Result<(), String> {
    if id == DEFAULT_PROFILE {
        return Err("PROFILE_DEFAULT_NOT_REMOVABLE: the default profile cannot be removed".to_string());
    }
    if id == active_profile(app_handle) {
        return Err("PROFILE_ACTIVE_NOT_REMOVABLE: the active profile cannot be removed".to_string());
    }
    // 路径安全：ID 字符集白名单 + 目标必须位于 profiles 根目录内（防 `..` 穿越）
    let profiles_root = config::get_dsh_data_path(app_handle).join("profiles");
    let dir = fs_guard::safe_remove_target(&profiles_root, id)?;
    if !dir.is_dir() {
        return Err(format!("PROFILE_NOT_FOUND: profile {id} does not exist"));
    }
    fs::remove_dir_all(&dir).map_err(|e| format!("PROFILE_REMOVE_FAILED: {e}"))
}

/// 初始化档案目录：与官方 `dsh-app-boot::initProfile` 的产物一致
/// （web 模板 bundles；已有文件绝不覆盖，重跑为 no-op）。
fn init_profile_dir(dir: &Path, id: &str) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("PROFILE_MKDIR: {e}"))?;

    let manifest_path = dir.join("package.json");
    if !manifest_path.exists() {
        let manifest = serde_json::json!({
            "name": format!("dsh-profile-{id}"),
            "private": true,
            "dependencies": {},
            "dsh": { "profile": { "bundles": WEB_PROFILE_BUNDLES } }
        });
        let content = serde_json::to_string_pretty(&manifest)
            .map_err(|e| format!("PROFILE_MANIFEST_RENDER: {e}"))?;
        fs::write(&manifest_path, format!("{content}\n"))
            .map_err(|e| format!("PROFILE_MANIFEST_WRITE: {e}"))?;
    }

    let patch_path = dir.join("cordis.patch.yml");
    if !patch_path.exists() {
        fs::write(&patch_path, PROFILE_PATCH_TEMPLATE)
            .map_err(|e| format!("PROFILE_PATCH_WRITE: {e}"))?;
    }

    let workspace_path = dir.join("pnpm-workspace.yaml");
    if !workspace_path.exists() {
        fs::write(&workspace_path, PROFILE_PNPM_WORKSPACE)
            .map_err(|e| format!("PROFILE_WORKSPACE_WRITE: {e}"))?;
    }

    // pnpm 无 TTY 环境重装/更新会触发交互确认（ERR_PNPM_ABORTED_REMOVE_MODULES_DIR_NO_TTY），
    // 与 ensure_profile_npmrc 一致地预写 .npmrc（幂等，绝不覆盖已有配置）。
    let npmrc_path = dir.join(".npmrc");
    let npmrc_existing = fs::read_to_string(&npmrc_path).unwrap_or_default();
    if !npmrc_existing.lines().any(|l| l.trim() == "confirmModulesPurge=false") {
        let mut content = npmrc_existing;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str("confirmModulesPurge=false\n");
        fs::write(&npmrc_path, content).map_err(|e| format!("PROFILE_NPMRC_WRITE: {e}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_id_lowercases_and_joins() {
        assert_eq!(normalize_profile_id("My Work Space"), "my-work-space");
        assert_eq!(normalize_profile_id("  dev--stage  "), "dev-stage");
        assert_eq!(normalize_profile_id("中文档案"), "");
        assert_eq!(normalize_profile_id("a_b-c"), "a-b-c");
    }

    #[test]
    fn display_name_strips_manifest_prefix() {
        let dir = std::env::temp_dir().join(format!("dsh-profile-name-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 无 manifest → 回落 id
        assert_eq!(manifest_display_name(&dir, "beta"), "Beta");
        // manifest 带 dsh-profile- 前缀 → 剥离后首字母大写
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"dsh-profile-beta","private":true}"#,
        )
        .unwrap();
        assert_eq!(manifest_display_name(&dir, "beta"), "Beta");
        // 非标准 name → 原样
        std::fs::write(dir.join("package.json"), r#"{"name":"my-profile"}"#).unwrap();
        assert_eq!(manifest_display_name(&dir, "beta"), "My-profile");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn init_profile_dir_scaffolds_official_shape() {
        let dir = std::env::temp_dir().join(format!("dsh-profile-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        init_profile_dir(&dir, "beta").unwrap();

        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["name"], "dsh-profile-beta");
        assert_eq!(manifest["dependencies"], serde_json::json!({}));
        assert_eq!(
            manifest["dsh"]["profile"]["bundles"],
            serde_json::json!(["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"])
        );
        assert!(dir.join("cordis.patch.yml").is_file());
        assert!(dir.join("pnpm-workspace.yaml").is_file());
        let npmrc = std::fs::read_to_string(dir.join(".npmrc")).unwrap();
        assert!(npmrc.contains("confirmModulesPurge=false"));

        // 幂等：再次初始化不报错、不重复写 .npmrc
        init_profile_dir(&dir, "beta").unwrap();
        let npmrc2 = std::fs::read_to_string(dir.join(".npmrc")).unwrap();
        assert_eq!(npmrc, npmrc2);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 路径穿越回归：`..`、`.`、绝对路径、含分隔符的 id 一律在 remove 前被拦截，
    /// 绝不进入 `remove_dir_all`（防 `remove_profile("..")` 删到 $DSH_HOME 本级）。
    #[test]
    fn remove_rejects_path_traversal_ids() {
        for bad in ["..", ".", "../x", "/etc", "a/b", "..\\x", "a\\b"] {
            assert!(
                fs_guard::validate_id(bad).is_err(),
                "id {bad:?} 必须被字符集白名单拦截"
            );
        }
        for good in ["web", "my-profile", "dsh-1.2.3"] {
            assert!(fs_guard::validate_id(good).is_ok(), "id {good:?} 应合法");
        }
        // safe_remove_target 对不存在目标拒绝（不触发删除）
        let tmp = std::env::temp_dir().join(format!("dsh-profile-guard-{}", std::process::id()));
        let root = tmp.join("profiles");
        std::fs::create_dir_all(&root).unwrap();
        let res = std::panic::catch_unwind(|| {
            std::fs::create_dir_all(&root.join("web")).unwrap();
            let ok = crate::service::fs_guard::safe_remove_target(&root, "web");
            assert!(ok.is_ok(), "存在的合法目录应通过守卫: {ok:?}");
            let bad = crate::service::fs_guard::safe_remove_target(&root, "..");
            assert!(bad.is_err(), "`..` 必须被拒绝");
        });
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(res.is_ok(), "test panicked: {res:?}");
    }
}
