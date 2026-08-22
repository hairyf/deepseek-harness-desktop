//! 插件异常修复（recovery）：启动失败 / 运行期异常时定位问题插件，并提供
//! 「卸除此插件并继续检测」的一键离线修复。
//!
//! 参考 dataelement/dsh-desktop 的插件异常修复模式（PR #94/#96）：
//! - **定位**：从启动日志按错误特征提取插件引用（duplicate route / loader entry /
//!   cannot resolve bundle / no dsh.bundle / slot conflict / failed to import），再
//!   按 profile `package.json` + `node_modules` 归属回配置的根插件——只有拿到确凿
//!   证据才动手，绝不瞎猜。
//! - **卸载**：直接改 profile 清单（`dependencies` + `dsh.profile.bundles`）、删除
//!   `node_modules/<id>`、剥离 `cordis.patch.yml` 中该插件的补丁层、清掉
//!   `pnpm-lock.yaml`（best-effort），保留其它插件与配置。与 [`super::install`] 走
//!   `dsh plugin` 子进程不同：本模块离线、精准，不需要网络。
//!
//! 卸载成功后由前端 `restart()` 重启并重新检测；若仍有问题，启动失败再次触发
//! 定位，形成「继续检测」循环。

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use tauri::AppHandle;

use super::errors;
use super::installed::profile_dir;

/// 前端监听的事件名：需要弹出插件异常修复界面时推送。
pub(crate) const RECOVERY_REQUIRED_EVENT: &str = "plugin-recovery-required";

/// 核心 bundle / 官方包：无论如何不可被「修复卸载」删除。
fn is_core_package(name: &str) -> bool {
    name == "dshmarket"
        || name == "@deepseek-ai/dsh-base"
        || name == "@deepseek-ai/dsh-web-app"
        || name.starts_with("@deepseek-ai/")
}

/// 是否为合法的 npm 包名（可带 scope）。用于过滤日志里提取到的候选引用。
fn is_package_name(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.contains(':') || s.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    // 路径穿越防护：`.` 与 `..` 虽由字符集放行（包名允许点），但单独出现
    // 或作为路径片段时会被 `Path::join` 解析为上级目录——必须显式拒绝。
    if s == "." || s == ".." || s.starts_with("..") || s.ends_with('/') || s.ends_with("..") {
        return false;
    }
    let body = s.strip_prefix('@').unwrap_or(s);
    // scoped 必须带 `/`（@scope/name）
    if s.starts_with('@') && !body.contains('/') {
        return false;
    }
    body.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' || c == '/')
}

/// 是否是可行动的第三方插件引用（排除核心包与 @deepseek-ai 官方包）。
pub(crate) fn is_actionable_plugin_ref(s: &str) -> bool {
    is_package_name(s) && !is_core_package(s.trim())
}

/// 启动失败时前端读到的日志行（已清洗 ANSI），序列化给前端（camelCase）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PluginRecoveryInfo {
    /// 定位到的插件 id（npm 包名），可能为空（未定位到具体插件）
    pub plugins: Vec<String>,
    /// 失败原因判别键：duplicate_route / duplicate_loader_entry / cannot_resolve_bundle /
    /// no_dsh_bundle / slot_conflict / load_failed / runtime / unknown
    pub reason: String,
    /// 动态详情（如冲突的路由 / 槽位 / 服务组件 id），用于 I18n 插值
    pub detail: String,
    /// 原始错误信息（技术详情查看）
    pub raw_error: String,
}

// ---- 纯提取函数（便于单元测试）----

/// 从日志文本中提取插件引用（多个错误特征的正则，去重）。
fn extract_plugin_refs(text: &str) -> Vec<String> {
    let mut refs = HashSet::new();
    let patterns = [
        // failed to apply/import loader entry <name> (<pkg>)
        r#"failed to (?:apply|import) loader entry[^\n]*\(([^)]+)\)"#,
        // cannot resolve profile bundle "<pkg>"
        r#"cannot resolve profile bundle\s+["']?([^"'\n]+)["']?"#,
        // profile bundle "<pkg>" declares no dsh.bundle
        r#"profile bundle\s+["']?([^"'\n]+)["']?\s+declares no dsh\.bundle"#,
        // plugin(s) failed to load: <pkg>
        r#"plugins? failed to load:\s*([A-Za-z0-9@/_.\-]+)"#,
    ];
    for pattern in patterns {
        if let Ok(re) = Regex::new(pattern) {
            for cap in re.captures_iter(text) {
                if let Some(m) = cap.get(1) {
                    let cand = m.as_str().trim();
                    if is_actionable_plugin_ref(cand) {
                        refs.insert(cand.to_string());
                    }
                }
            }
        }
    }
    // 「Failed to load plugins」错误卡片：紧随其后的若干行通常是包名。
    for m in Regex::new(r"(?m)^Failed to load plugins\s*$").expect("literal")
        .find_iter(text)
    {
        let rest = &text[m.end()..];
        for line in rest.lines().take(12) {
            let cand = line.trim().trim_end_matches(['.', ',', ' ']);
            if is_actionable_plugin_ref(cand) {
                refs.insert(cand.to_string());
            }
        }
    }
    refs.into_iter().collect()
}

/// 抽取「重复 loader entry id」。
fn extract_duplicate_loader_entry(text: &str) -> Option<String> {
    let re = Regex::new(r#"duplicate loader entry id:\s*["']?([^"'\s]+)["']?"#).ok()?;
    re.captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
}

/// 抽取「界面槽位冲突」的槽位名。
fn extract_slot_conflict(text: &str) -> Option<String> {
    let re = Regex::new(r#"single slot\s+["']([^"']+)["']\s+already has a registration"#).ok()?;
    if let Some(c) = re.captures(text) {
        return c.get(1).map(|m| m.as_str().trim().to_string());
    }
    let re = Regex::new(r#"UI slot\s+["']([^"']+)["']\s+has duplicate registrations"#).ok()?;
    re.captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
}

/// 对日志文本分类失败原因，返回（判别键, 动态详情）。
fn classify_reason(text: &str) -> (String, String) {
    let dup_route =
        Regex::new(r#"duplicate prefix route\s+["']([^"']+)["']"#).expect("literal");
    if let Some(c) = dup_route.captures(text) {
        let route = c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
        return ("duplicate_route".into(), route);
    }
    if let Some(entry) = extract_duplicate_loader_entry(text) {
        return ("duplicate_loader_entry".into(), entry);
    }
    if let Some(re) = Regex::new(r#"cannot resolve profile bundle\s+["']?([^"'\n]+)["']?"#)
        .ok()
    {
        if let Some(c) = re.captures(text) {
            return (
                "cannot_resolve_bundle".into(),
                c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default(),
            );
        }
    }
    if text.contains("declares no dsh.bundle") {
        let pkg = extract_plugin_refs(text).into_iter().next().unwrap_or_default();
        return ("no_dsh_bundle".into(), pkg);
    }
    if let Some(slot) = extract_slot_conflict(text) {
        return ("slot_conflict".into(), slot);
    }
    if text.contains("failed to import loader entry") || text.contains("failed to apply loader entry") {
        return ("load_failed".into(), String::new());
    }
    ("unknown".into(), String::new())
}

// ---- 归属：把日志里的引用映射回 profile 配置的根插件 ----

/// 读取 role 包自身的 package.json，返回一个轻量视图。
#[derive(Default)]
struct PluginMeta {
    deps: Vec<String>,
    optional_deps: Vec<String>,
    patch_path: Option<String>,
}

fn read_plugin_meta(dir: &Path) -> Option<PluginMeta> {
    let content = fs::read_to_string(dir.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let mut meta = PluginMeta::default();
    if let Some(deps) = v.get("dependencies").and_then(|d| d.as_object()) {
        meta.deps = deps.keys().cloned().collect();
    }
    if let Some(deps) = v.get("optionalDependencies").and_then(|d| d.as_object()) {
        meta.optional_deps = deps.keys().cloned().collect();
    }
    meta.patch_path = v
        .get("dsh")
        .and_then(|d| d.get("bundle"))
        .and_then(|b| b.get("patch"))
        .and_then(|p| p.as_str())
        .map(String::from);
    Some(meta)
}

/// 当前档案配置的根插件：第三方依赖且在 `dsh.profile.bundles` 中（只有 bundles
/// 才会随启动加载，才可能引起启动失败）。
fn configured_roots(app_handle: &AppHandle) -> Vec<String> {
    let dir = profile_dir(app_handle);
    let content = match fs::read_to_string(dir.join("package.json")) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let v: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let bundles: HashSet<String> = v
        .get("dsh")
        .and_then(|d| d.get("profile"))
        .and_then(|p| p.get("bundles"))
        .and_then(|b| b.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let deps: Vec<String> = v
        .get("dependencies")
        .and_then(|d| d.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    deps.iter()
        .filter(|d| bundles.contains(*d) && is_actionable_plugin_ref(d))
        .cloned()
        .collect()
}

/// 判断某个根 bundle 是否「拥有」被报告的子包：其依赖里直接声明了该子包，或其
/// patch 层里引用该包名。
fn bundle_owns_package(profile: &Path, bundle: &str, package: &str) -> bool {
    let dir = profile.join("node_modules").join(bundle);
    let Some(meta) = read_plugin_meta(&dir) else {
        return false;
    };
    if meta.deps.iter().any(|d| d == package)
        || meta.optional_deps.iter().any(|d| d == package)
    {
        return true;
    }
    if let Some(patch) = &meta.patch_path {
        if let Ok(content) = fs::read_to_string(dir.join(patch)) {
            return content.contains(package);
        }
    }
    false
}

/// 判断某个根插件的运行时代码/依赖是否引用了给定包集合之一（用于「动态创建官方
/// UI 包」的归属）。
fn plugin_references_packages(profile: &Path, plugin: &str, packages: &HashSet<String>) -> bool {
    let dir = profile.join("node_modules").join(plugin);
    if let Some(meta) = read_plugin_meta(&dir) {
        if meta
            .deps
            .iter()
            .chain(meta.optional_deps.iter())
            .any(|d| packages.contains(d))
        {
            return true;
        }
    }
    for file in ["cordis.patch.yml", "index.js", "lib/index.js", "dist/index.js"] {
        if let Ok(content) = fs::read_to_string(dir.join(file)) {
            if packages.iter().any(|p| content.contains(p)) {
                return true;
            }
        }
    }
    false
}

/// 判断某个根插件的 patch 层是否声明了重复的 loader entry id。
fn plugin_declares_loader_entry(profile: &Path, plugin: &str, entry_id: &str) -> bool {
    let dir = profile.join("node_modules").join(plugin);
    let Some(meta) = read_plugin_meta(&dir) else {
        return false;
    };
    let Some(patch) = &meta.patch_path else {
        return false;
    };
    let Ok(content) = fs::read_to_string(dir.join(patch)) else {
        return false;
    };
    let re = Regex::new(&format!(r#"^\s*-\s+id:\s*["']?{}["']?(?:\s*(?:#.*)?)?$"#,
        regex::escape(entry_id))).ok();
    re.map(|re| re.is_match(&content)).unwrap_or(false)
}

/// 判断某个根插件的文件是否包含给定槽位名。
fn plugin_matches_slot(profile: &Path, plugin: &str, slot: &str) -> bool {
    let dir = profile.join("node_modules").join(plugin);
    for file in [
        "cordis.patch.yml",
        "client.js",
        "lib/client.js",
        "dist/client.js",
        "package.json",
        "index.js",
        "lib/index.js",
        "dist/index.js",
    ] {
        if let Ok(content) = fs::read_to_string(dir.join(file)) {
            if content.contains(slot) {
                return true;
            }
        }
    }
    false
}

/// 提供某槽位的官方 UI 客户端包（用于槽位冲突归属）。
fn packages_providing_slot(profile: &Path, slot: &str) -> Vec<String> {
    let scope = profile.join("node_modules").join("@deepseek-ai");
    let Ok(entries) = fs::read_dir(&scope) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("dsh-client-ui-") {
            continue;
        }
        let package = format!("@deepseek-ai/{name}");
        let dir = entry.path();
        for file in ["client.js", "lib/client.js", "dist/client.js"] {
            if let Ok(content) = fs::read_to_string(dir.join(file)) {
                if content.contains(slot) {
                    out.push(package);
                    break;
                }
            }
        }
    }
    out
}

/// 把日志提取到的引用归属回 profile 配置的根插件；只在一对一（证据唯一）时返回，
/// 否则返回空（绝不瞎猜）。
fn resolve_recovery_plugins(
    app_handle: &AppHandle,
    detected_refs: &[String],
    duplicate_entry: Option<&str>,
    slot_conflict: Option<&str>,
) -> Vec<String> {
    let profile = profile_dir(app_handle);
    let roots = configured_roots(app_handle);
    if roots.is_empty() {
        return Vec::new();
    }
    let roots_set: HashSet<&String> = roots.iter().collect();

    // 1) 直接命中，或证明某报告子包被唯一根插件拥有。
    let mut matched = HashSet::new();
    for detected in detected_refs {
        if !is_package_name(detected) {
            continue;
        }
        if roots_set.contains(detected) {
            matched.insert(detected.clone());
            continue;
        }
        let owners: Vec<&String> = roots
            .iter()
            .filter(|root| bundle_owns_package(&profile, root, detected))
            .collect();
        if owners.len() == 1 {
            matched.insert(owners[0].clone());
        }
    }
    if matched.len() == 1 {
        return matched.into_iter().collect();
    }

    // 1b) 兜底：某官方叶包（loader 报错常指这个）被动态创建，归属回引用它的唯一根。
    let mut dynamic_owners = HashSet::new();
    for detected in detected_refs {
        if !is_package_name(detected) || roots_set.contains(detected) {
            continue;
        }
        let packages: HashSet<String> = [detected.to_string()].into();
        let owners: Vec<&String> = roots
            .iter()
            .filter(|root| plugin_references_packages(&profile, root, &packages))
            .collect();
        if owners.len() == 1 {
            dynamic_owners.insert(owners[0].clone());
        }
    }
    if dynamic_owners.len() == 1 {
        return dynamic_owners.into_iter().collect();
    }

    // 2) 重复 loader entry：命中唯一根插件。
    if let Some(entry) = duplicate_entry {
        let owners: Vec<&String> = roots
            .iter()
            .filter(|root| plugin_declares_loader_entry(&profile, root, entry))
            .collect();
        if owners.len() == 1 {
            return owners.into_iter().map(|s| s.clone()).collect();
        }
    }

    // 3) 槽位冲突：命中唯一根插件；否则找提供槽位的官方包，再由唯一根引用它。
    if let Some(slot) = slot_conflict {
        let matched: Vec<&String> = roots
            .iter()
            .filter(|root| plugin_matches_slot(&profile, root, slot))
            .collect();
        if matched.len() == 1 {
            return matched.into_iter().map(|s| s.clone()).collect();
        }
        let providers: HashSet<String> =
            packages_providing_slot(&profile, slot).into_iter().collect();
        if !providers.is_empty() {
            let owners: Vec<&String> = roots
                .iter()
                .filter(|root| plugin_references_packages(&profile, root, &providers))
                .collect();
            if owners.len() == 1 {
                return owners.into_iter().map(|s| s.clone()).collect();
            }
        }
    }

    Vec::new()
}

/// 定位启动失败的问题插件：给定日志行，返回恢复信息（未定位到则 plugins 为空）。
pub fn detect(app_handle: &AppHandle, log_lines: &[String]) -> PluginRecoveryInfo {
    let text = log_lines.join("\n");
    let refs = extract_plugin_refs(&text);
    let duplicate_entry = extract_duplicate_loader_entry(&text);
    let slot_conflict = extract_slot_conflict(&text);
    let (reason, detail) = classify_reason(&text);
    let plugins = resolve_recovery_plugins(app_handle, &refs, duplicate_entry.as_deref(), slot_conflict.as_deref());
    let raw_error = refs_text(&text);
    PluginRecoveryInfo {
        plugins,
        reason,
        detail,
        raw_error,
    }
}

/// 原始错误描述：尽量取关键错误行，供「查看技术详情」。
fn refs_text(text: &str) -> String {
    // 从日志里搜出带错误标记的行（最多 8 行），没有则取尾部。
    let marker_any =
        Regex::new(r"(?i)error|duplicate|fatal|panic|throw|failed|exception|✖").expect("literal");
    let mut err_lines: Vec<&str> = text
        .lines()
        .filter(|l| marker_any.is_match(l))
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if err_lines.is_empty() {
        let tail: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let start = tail.len().saturating_sub(8);
        err_lines = tail[start..].to_vec();
    }
    let joined = err_lines.join("\n");
    joined.chars().take(2000).collect()
}

// ---- 卸载（直接改 profile 清单）----

/// 从 manifest 中移除指定插件（`dependencies` + `dsh.profile.bundles`），返回是否有改动。
fn remove_plugin_from_manifest(manifest: &mut serde_json::Value, id: &str) -> bool {
    let mut modified = false;
    if let Some(deps) = manifest.get_mut("dependencies").and_then(|d| d.as_object_mut()) {
        if deps.remove(id).is_some() {
            modified = true;
        }
    }
    if let Some(bundles) = manifest
        .get_mut("dsh")
        .and_then(|d| d.get_mut("profile"))
        .and_then(|p| p.get_mut("bundles"))
        .and_then(|b| b.as_array_mut())
    {
        let before = bundles.len();
        bundles.retain(|b| b.as_str() != Some(id));
        if bundles.len() != before {
            modified = true;
        }
    }
    modified
}

/// 删除 `node_modules/<id>`；scoped 目录删除后若 scope 空则一并清理。
fn remove_plugin_dir(profile: &Path, id: &str) {
    let node_modules = profile.join("node_modules");
    let dir = node_modules.join(id);
    if let Err(e) = fs::remove_dir_all(&dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!("failed to remove plugin dir {}: {e}", dir.display());
        }
    }
    if let Some(scope) = id.starts_with('@').then(|| id.split('/').next().unwrap_or_default()) {
        if !scope.is_empty() && scope != id {
            let scope_dir = node_modules.join(scope);
            if scope_dir.is_dir() && scope_dir.read_dir().map(|mut d| d.next().is_none()).unwrap_or(false) {
                let _ = fs::remove_dir_all(&scope_dir);
            }
        }
    }
}

/// 从 `cordis.patch.yml` 中剥离目标插件的 patch 条目（保留其它插件的 patch）。
///
/// 与 dsh-desktop「直接重置为 []」不同：这里只移除与目标插件相关的条目，不破坏
/// 其它插件的配置层，符合「其它插件不会被删除」的承诺。解析失败则原样保留。
fn strip_cordis_patch_for(profile: &Path, id: &str) {
    let path = profile.join("cordis.patch.yml");
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&content) else {
        return;
    };
    let Some(entries) = doc.as_sequence() else {
        return;
    };
    let kept: Vec<serde_yaml::Value> = entries
        .iter()
        .filter(|e| !patch_entry_targets(e, id))
        .cloned()
        .collect();
    if kept.len() == entries.len() {
        return;
    }
    if let Ok(rendered) = serde_yaml::to_string(&serde_yaml::Value::Sequence(kept)) {
        let _ = fs::write(&path, rendered);
        log::info!("Stripped cordis.patch.yml entries for plugin {id}");
    }
}

/// 一个 patch 条目是否「针对」目标插件：顶层 id 字段或任意字段值等于该包名。
fn patch_entry_targets(entry: &serde_yaml::Value, id: &str) -> bool {
    match entry {
        serde_yaml::Value::Mapping(map) => map.iter().any(|(k, v)| {
            k.as_str() == Some(id) || v.as_str() == Some(id)
        }),
        serde_yaml::Value::String(s) => s == id,
        _ => false,
    }
}

/// 修复模式卸载：精准删除指定插件（离线、不破坏其它插件）。
pub fn uninstall(app_handle: &AppHandle, id: &str) -> Result<(), String> {
    if !is_actionable_plugin_ref(id) {
        return Err(format!(
            "PLUGIN_RECOVERY_REFUSED: refusing to remove core/official package {id}"
        ));
    }
    let profile = profile_dir(app_handle);
    let manifest_path = profile.join("package.json");
    if !manifest_path.exists() {
        return Err("PLUGIN_RECOVERY_NO_MANIFEST: profile package.json missing".to_string());
    }
    let content = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("PLUGIN_RECOVERY_READ: {e}"))?;
    let mut manifest: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("PLUGIN_RECOVERY_PARSE: {e}"))?;

    let modified = remove_plugin_from_manifest(&mut manifest, id);
    if modified {
        let rendered = serde_json::to_string_pretty(&manifest)
            .map_err(|e| format!("PLUGIN_RECOVERY_RENDER: {e}"))?;
        std::fs::write(&manifest_path, format!("{rendered}\n"))
            .map_err(|e| format!("PLUGIN_RECOVERY_WRITE: {e}"))?;
        log::info!("Recovery uninstall removed plugin {id} from profile manifest");
    }

    remove_plugin_dir(&profile, id);
    strip_cordis_patch_for(&profile, id);
    // 清掉 lockfile，让 pnpm 重装时重建干净依赖图（best-effort）。
    if let Err(e) = fs::remove_file(profile.join("pnpm-lock.yaml")) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!("failed to remove pnpm-lock.yaml during recovery: {e}");
        }
    }
    if let Err(e) = errors::clear(app_handle, id) {
        log::warn!("failed to clear plugin error for {id} during recovery: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_name_validation() {
        assert!(is_package_name("dshmarket"));
        assert!(is_package_name("@scope/pkg"));
        assert!(is_package_name("dsh-better-sidebar"));
        assert!(!is_package_name(""));
        assert!(!is_package_name("has space"));
        assert!(!is_package_name("with:colon"));
        assert!(!is_package_name("@bare"));
        assert!(!is_actionable_plugin_ref("@deepseek-ai/dsh-base"));
        assert!(!is_actionable_plugin_ref("dshmarket"));
        assert!(is_actionable_plugin_ref("dsh-better-sidebar"));
    }

    #[test]
    fn extract_refs_from_failure_log() {
        let log = r#"
[stderr] failed to apply loader entry dshSidebarApi (@omdsh-dev/dsh-better-sidebar)
[stderr] cannot resolve profile bundle "dsh-web-ui-all"
"#;
        let refs = extract_plugin_refs(log);
        assert!(refs.contains(&"@omdsh-dev/dsh-better-sidebar".to_string()));
        assert!(refs.contains(&"dsh-web-ui-all".to_string()));
    }

    #[test]
    fn extract_refs_from_boot_card() {
        let log = "Failed to load plugins\ndsh-better-sidebar\n@scope/another\nAn unknown error occurred\n";
        let refs = extract_plugin_refs(log);
        assert!(refs.contains(&"dsh-better-sidebar".to_string()));
        assert!(refs.contains(&"@scope/another".to_string()));
        // 非包名行不应被当作插件引用
        assert!(!refs.iter().any(|r| r.contains("unknown")));
    }

    #[test]
    fn extract_duplicate_and_slot() {
        let route_log = r#"duplicate prefix route "/sidebar/api""#;
        assert_eq!(classify_reason(route_log).0, "duplicate_route");
        let entry_log = "duplicate loader entry id: \"dshSidebarApi\"";
        assert_eq!(extract_duplicate_loader_entry(entry_log).as_deref(), Some("dshSidebarApi"));
        let slot_log = r#"single slot "sidebar" already has a registration"#;
        assert_eq!(extract_slot_conflict(slot_log).as_deref(), Some("sidebar"));
    }

    #[test]
    fn remove_plugin_from_manifest_edits_deps_and_bundles() {
        let mut manifest = serde_json::json!({
            "name": "dsh-profile-web",
            "private": true,
            "dependencies": { "dshmarker": "1.0.0", "dsh-better-sidebar": "1.0.0" },
            "dsh": { "profile": { "bundles": ["@deepseek-ai/dsh-base", "dsh-better-sidebar"] } }
        });
        let modified = remove_plugin_from_manifest(&mut manifest, "dsh-better-sidebar");
        assert!(modified);
        assert!(manifest["dependencies"].get("dsh-better-sidebar").is_none());
        assert!(manifest["dependencies"].get("dshmarker").is_some());
        assert_eq!(
            manifest["dsh"]["profile"]["bundles"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn patch_strip_targets_plugin_only() {
        let patch = serde_yaml::to_string(&serde_yaml::Value::Sequence(vec![
            serde_yaml::from_str::<serde_yaml::Value>("id: dsh-better-sidebar\nfoo: 1").unwrap(),
            serde_yaml::from_str::<serde_yaml::Value>("id: dsh-web-ui-all\nbar: 2").unwrap(),
        ]))
        .unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&patch).unwrap();
        let kept: Vec<serde_yaml::Value> = doc
            .as_sequence()
            .unwrap()
            .iter()
            .filter(|e| !patch_entry_targets(e, "dsh-better-sidebar"))
            .cloned()
            .collect();
        assert_eq!(kept.len(), 1);
        let kept_doc = serde_yaml::to_string(&serde_yaml::Value::Sequence(kept)).unwrap();
        assert!(kept_doc.contains("dsh-web-ui-all"));
        assert!(!kept_doc.contains("dsh-better-sidebar"));
    }
}
