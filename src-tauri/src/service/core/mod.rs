//! Harness 核心管理。
//!
//! 核心来源：
//! - `local`：用户通过 CLI（npm/pnpm 全局安装）自行安装的 dsh，安装目录与
//!   配置（`$DSH_HOME`）都不归桌面端管理；
//! - `app`：桌面端预打包管理的 deepseek-harness-pkg 多版本副本。激活版本固定
//!   位于 `dependencies/dsh`（既有代码全部依赖该路径），通过「核心」面板下载的
//!   历史版本存放在 `dependencies/dsh-<tag>` 槽位，切换时两个目录互换。
//!
//! 启动优先级（需求）：本地核心存在时优先使用本地核心；未检测到才走预打包。
//! 用户在「核心」面板可显式切回预打包；显式选择持久化在 store 设置
//! （`active_core`），`None` = 自动（本地优先）。
//!
//! 本地核心更新通过其包管理器 CLI 完成（npm `update -g` / pnpm `add -g`），
//! 不触碰用户安装本身之外的文件。

use crate::config;
use crate::service::{cli, download, fs_guard, workflow};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

/// 核心来源
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CoreSource {
    /// 用户通过 CLI 安装的本地核心
    Local,
    /// 桌面端预打包核心
    App,
}

impl CoreSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CoreSource::Local => "local",
            CoreSource::App => "app",
        }
    }

    pub fn parse(source: &str) -> Option<CoreSource> {
        match source {
            "local" => Some(CoreSource::Local),
            "app" => Some(CoreSource::App),
            _ => None,
        }
    }
}

/// 核心列表项（序列化 camelCase 给前端）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessCore {
    /// `local` | `app`（无 tag 记录的旧激活行）| `app-<tag>`
    pub id: String,
    pub source: CoreSource,
    /// 版本号（不含 `v` 前缀；缺失为空串）
    pub version: String,
    /// 完整 release tag（如 `dsh-0.1.0-rc.8-32331963388`；local 行为空串）
    pub tag: String,
    /// 核心入口（cli path）：本地核心为 bin.js 绝对路径，预打包为安装目录
    pub path: String,
    /// 「打开目录」入口：本地核心为包目录，预打包为安装/槽位目录；未下载为空
    pub dir: String,
    /// 本地是否可用（文件在盘/可解析）
    pub present: bool,
    /// 当前是否使用中的核心
    pub active: bool,
    pub error: Option<String>,
}

/// 解析出的本地核心信息
struct LocalCore {
    package_dir: PathBuf,
    version: String,
    /// `lib/bin.js` 入口
    bin: PathBuf,
}

/// 用户 dsh 可执行文件：PATH 中排除本应用 shim 目录（与 pnpm shim 的
/// 用户优先策略一致）。
fn find_user_dsh_bin(app_handle: &AppHandle) -> Option<PathBuf> {
    let bin_dir = cli::get_bin_dir(app_handle);
    let candidates: &[&str] = if cfg!(windows) {
        &["dsh.cmd", "dsh.exe", "dsh.bat"]
    } else {
        &["dsh"]
    };
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        if dir == bin_dir || dir.as_os_str().is_empty() {
            continue;
        }
        for name in candidates {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// 在给定前缀下探测 `node_modules/@deepseek-ai/dsh` 包目录。
fn probe_package_dir(prefix: &Path) -> Option<PathBuf> {
    let p = prefix.join("node_modules").join("@deepseek-ai").join("dsh");
    p.join("package.json").is_file().then_some(p)
}

/// 解析用户 dsh 的包目录（npm / pnpm 全局布局探测，纯文件系统、不派生子进程）：
/// 1. PATH 命中项同级推导（npm：bin 与 node_modules 同前缀；pnpm Windows：
///    bin 在 `%LOCALAPPDATA%\pnpm`，包在 `global/<n>/node_modules`）；
/// 2. pnpm 全局布局扫描（`<prefix>/global/*/node_modules/@deepseek-ai/dsh`）。
fn user_dsh_package_dir(app_handle: &AppHandle) -> Option<PathBuf> {
    let bin = find_user_dsh_bin(app_handle)?;
    let prefix = bin.parent()?;
    // npm 布局：`<prefix>/node_modules/@deepseek-ai/dsh`
    if let Some(dir) = probe_package_dir(prefix) {
        return Some(dir);
    }
    // pnpm 布局：`<prefix>/global/<n>/node_modules/@deepseek-ai/dsh`
    if let Ok(entries) = std::fs::read_dir(prefix.join("global")) {
        for entry in entries.flatten() {
            if let Some(dir) = probe_package_dir(&entry.path()) {
                return Some(dir);
            }
        }
    }
    None
}

/// 读取包目录 package.json 的 version 字段
fn read_package_version(dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("version")
        .and_then(|v| v.as_str())
        .map(|s| s.trim_start_matches('v').to_string())
}

/// 本地核心：包目录 + 版本 + bin.js 入口。任一环节缺失视为不存在。
fn local_core(app_handle: &AppHandle) -> Option<LocalCore> {
    let package_dir = user_dsh_package_dir(app_handle)?;
    let version = read_package_version(&package_dir)?;
    let bin = package_dir.join("lib").join("bin.js");
    if !bin.is_file() {
        return None;
    }
    Some(LocalCore {
        package_dir,
        version,
        bin,
    })
}

/// 当前活动核心来源（需求 3：本地核心存在时优先，除非用户显式选择预打包）。
pub fn active_source(app_handle: &AppHandle) -> CoreSource {
    let setting = config::get_store_dat_setting(app_handle);
    let local_present = local_core(app_handle).is_some();
    match setting.active_core.as_deref().and_then(CoreSource::parse) {
        Some(CoreSource::App) => CoreSource::App,
        // 显式选择本地但本地已失效 → 回退预打包
        Some(CoreSource::Local) if local_present => CoreSource::Local,
        // 未设置（自动）或显式本地已失效：本地存在时优先
        _ => {
            if local_present {
                CoreSource::Local
            } else {
                CoreSource::App
            }
        }
    }
}

/// 当前活动核心的 dsh 入口（bin.js 绝对路径）。
///
/// 供服务启动（workflow::launch）与插件操作（plugin::install 等）统一取用，
/// 本地核心解析在调用瞬间失效时回退预打包入口。
pub fn active_dsh_binary(app_handle: &AppHandle) -> PathBuf {
    match active_source(app_handle) {
        CoreSource::Local => local_core(app_handle)
            .map(|c| c.bin)
            .unwrap_or_else(|| config::get_dsh_binary_path(app_handle)),
        CoreSource::App => config::get_dsh_binary_path(app_handle),
    }
}

/// 当前活动核心的版本号（`--no-open` 等按版本判定的能力以它为准）。
pub fn active_version(app_handle: &AppHandle) -> Option<String> {
    match active_source(app_handle) {
        CoreSource::Local => local_core(app_handle).map(|c| c.version),
        CoreSource::App => config::get_dsh_version(app_handle),
    }
}

/// `dependencies` 目录（激活 `dsh` 与历史 `dsh-<tag>` 槽位的共同父级）。
fn dependencies_dir(app_handle: &AppHandle) -> PathBuf {
    config::get_dsh_install_path(app_handle)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 历史版本槽位（新命名）：`dependencies/<tag>`。release tag 本身以 `dsh-` 开头
/// （如 `dsh-0.1.0-rc.8-32331963388`），因此槽位目录名即 tag，不再叠加前缀。
fn slot_dir(app_handle: &AppHandle, tag: &str) -> PathBuf {
    dependencies_dir(app_handle).join(tag)
}

/// 定位已下载的槽位：优先新命名 `dependencies/<tag>`，兼容旧版遗留的双前缀
/// `dependencies/dsh-<tag>`（tag 以 `dsh-` 开头时旧命名会产生 `dsh-dsh-...`）。
fn existing_slot_dir(app_handle: &AppHandle, tag: &str) -> Option<PathBuf> {
    let deps = dependencies_dir(app_handle);
    let new = deps.join(tag);
    if new.is_dir() {
        return Some(new);
    }
    let legacy = deps.join(format!("dsh-{tag}"));
    legacy.is_dir().then_some(legacy)
}

/// 读取发行版目录 `package.json` 中 `@deepseek-ai/dsh` 依赖版本（历史槽位展示用）。
fn read_manifest_dsh_version(dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("dependencies")?
        .get("@deepseek-ai/dsh")?
        .as_str()
        .map(|s| s.trim_start_matches(['^', '~', '=', '>', '<']).to_string())
}

/// 核心列表：本地核心 + deepseek-harness-pkg 各发布版本（按版本去重）。
///
/// 版本行数据源为 GitHub tags（`fetch_dsh_pkg_tags`，最新在前）。pkg 仓库会对同一
/// 版本打多个 tag（含测试打包），这里按版本去重——同一版本只保留**最后一个** tag。
/// 激活的预打包版本不置顶，而是作为普通版本行按自然顺序排列并标记 `active`。
/// tags 拉取失败（离线/限流）时降级为磁盘扫描，只列出本地、激活与已下载的历史版本。
pub async fn list(app_handle: &AppHandle) -> Vec<HarnessCore> {
    let source = active_source(app_handle);
    let local = local_core(app_handle);
    let local_bin = local
        .as_ref()
        .map(|c| c.bin.to_string_lossy().into_owned())
        .or_else(|| find_user_dsh_bin(app_handle).map(|b| b.to_string_lossy().into_owned()));

    let mut rows: Vec<HarnessCore> = vec![HarnessCore {
        id: "local".to_string(),
        source: CoreSource::Local,
        version: local.as_ref().map(|c| c.version.clone()).unwrap_or_default(),
        tag: String::new(),
        path: local_bin.clone().unwrap_or_default(),
        dir: local
            .as_ref()
            .map(|c| c.package_dir.to_string_lossy().into_owned())
            .unwrap_or_default(),
        present: local.is_some(),
        active: source == CoreSource::Local,
        error: None,
    }];

    // 激活的预打包信息：tag（可空，旧安装无记录）+ 安装目录状态
    let active_tag = config::get_dsh_pkg_tag(app_handle);
    let active_dir = config::get_dsh_install_path(app_handle);
    let active_present = config::get_dsh_binary_path(app_handle).exists();
    // 激活核心按「版本」而非 tag 匹配版本行：pkg 仓库会对同一版本重打包/打
    // 测试 tag，版本行去重后保留的 tag 未必等于本机安装时的记录 tag。按 tag
    // 精确匹配会让激活版本行误标「未下载」并在列表底部多出一条重复激活行。
    let active_version = if source == CoreSource::App {
        active_app_version(&active_tag, config::get_dsh_version(app_handle))
    } else {
        None
    };

    // 版本行：GitHub tags（最新在前）→ 按版本去重，同版本只保留最后一个 tag
    let tags = match download::fetch_dsh_pkg_tags().await {
        Ok(tags) => tags,
        Err(e) => {
            log::warn!("Failed to fetch dsh pkg tags ({}), showing only on-disk versions", e);
            Vec::new()
        }
    };
    let mut version_tags: Vec<(String, String)> = Vec::new(); // (version, tag)，保持首次出现顺序
    for (tag, _commit) in &tags {
        let Some(version) = download::parse_version_from_tag(tag) else {
            continue;
        };
        if let Some(entry) = version_tags.iter_mut().find(|(v, _)| v == &version) {
            // 同版本重复（测试打包）：保留最后一个 tag
            entry.1 = tag.clone();
        } else {
            version_tags.push((version, tag.clone()));
        }
    }

    // 激活行就地标记：按版本匹配激活核心（不置顶，作为普通版本行标 active）
    let mut active_rendered = false;
    for (version, tag) in &version_tags {
        let is_active = active_version.as_deref() == Some(version.as_str());
        if is_active {
            active_rendered = true;
        }
        let slot = existing_slot_dir(app_handle, tag);
        let present = slot.is_some();
        let (path, dir) = if is_active {
            (
                active_dir.to_string_lossy().into_owned(),
                active_dir.to_string_lossy().into_owned(),
            )
        } else if let Some(slot) = slot {
            let s = slot.to_string_lossy().into_owned();
            (s.clone(), s)
        } else {
            (String::new(), String::new())
        };
        rows.push(HarnessCore {
            id: format!("app-{tag}"),
            source: CoreSource::App,
            version: version.clone(),
            tag: tag.clone(),
            path,
            dir,
            present: if is_active { active_present } else { present },
            active: is_active,
            error: None,
        });
    }

    // 激活版本未出现在版本列表（离线/限流/tag 被移除/旧版无 tag 记录）：
    // 追加在版本行之后，保持列表不置顶
    if source == CoreSource::App && !active_rendered {
        rows.push(HarnessCore {
            id: active_tag
                .as_ref()
                .map(|t| format!("app-{t}"))
                .unwrap_or_else(|| "app".to_string()),
            source: CoreSource::App,
            version: config::get_dsh_version(app_handle).unwrap_or_default(),
            tag: active_tag.clone().unwrap_or_default(),
            path: active_dir.to_string_lossy().into_owned(),
            dir: active_dir.to_string_lossy().into_owned(),
            present: active_present,
            active: true,
            error: None,
        });
    }

    // 磁盘扫描：tags 拉取失败/限流，或存在已下载但不在 tags 列表的版本（被移除的
    // 测试打包）时，把已下载的 `dsh-*` 槽位补进列表；同样按版本去重。
    // 激活版本已由版本行（或底部激活行）呈现，先放入 seen 避免扫描再补一条重复行。
    let mut seen_versions: HashSet<String> = version_tags.iter().map(|(v, _)| v.clone()).collect();
    if let Some(v) = &active_version {
        seen_versions.insert(v.clone());
    }
    if let Ok(entries) = std::fs::read_dir(dependencies_dir(app_handle)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // 新命名槽位目录名即 tag（`dsh-0.1.0-rc.8-...`）；旧版双前缀
            // `dsh-dsh-...` 剥一层 `dsh-` 还原 tag。`dsh` 为激活目录，跳过。
            let tag = if let Some(rest) = name.strip_prefix("dsh-dsh-") {
                Some(format!("dsh-{rest}"))
            } else if let Some(rest) = name.strip_prefix("dsh-") {
                if rest.is_empty() {
                    None
                } else {
                    Some(name.clone())
                }
            } else {
                None
            };
            let Some(tag) = tag else { continue };
            if !entry.path().is_dir() {
                continue;
            }
            let version = read_manifest_dsh_version(&entry.path())
                .or_else(|| download::parse_version_from_tag(&tag))
                .unwrap_or_default();
            if version.is_empty() || !seen_versions.insert(version.clone()) {
                continue;
            }
            let dir = entry.path();
            rows.push(HarnessCore {
                id: format!("app-{tag}"),
                source: CoreSource::App,
                version,
                tag,
                path: dir.to_string_lossy().into_owned(),
                dir: dir.to_string_lossy().into_owned(),
                present: true,
                active: false,
                error: None,
            });
        }
    }

    rows
}

/// 切换活动核心（持久化 + 预打包版本目录互换；服务重启由前端负责）。
///
/// `id` 取值：`local` | `app`（无 tag 记录的旧激活行）| `app-<tag>`。
pub async fn set_active(app_handle: &AppHandle, id: &str) -> Result<HarnessCore, String> {
    if id == "local" {
        if local_core(app_handle).is_none() {
            return Err("CORE_LOCAL_NOT_FOUND: no local core detected".to_string());
        }
        let mut setting = config::get_store_dat_setting(app_handle);
        setting.active_core = Some(CoreSource::Local.as_str().to_string());
        config::set_store_dat_setting(app_handle, setting);
    } else if id == "app" {
        if !config::get_dsh_binary_path(app_handle).exists() {
            return Err("CORE_APP_NOT_FOUND: bundled core is not installed".to_string());
        }
        let mut setting = config::get_store_dat_setting(app_handle);
        setting.active_core = Some(CoreSource::App.as_str().to_string());
        config::set_store_dat_setting(app_handle, setting);
    } else if let Some(tag) = id.strip_prefix("app-") {
        switch_app_version(app_handle, tag).await?;
    } else {
        return Err(format!("CORE_INVALID_ID: {id}"));
    }

    Ok(list(app_handle)
        .await
        .into_iter()
        .find(|c| c.active)
        .ok_or_else(|| "CORE_NOT_FOUND: active core disappeared after switch".to_string())?)
}

/// 切换到指定 tag 的预打包版本（已下载的历史槽位）。
///
/// 磁盘布局：激活版本固定为 `dependencies/dsh`（既有代码全部依赖该路径），
/// 已下载的历史版本存放在 `dependencies/<tag>`（tag 以 `dsh-` 开头）。切换 = 目录
/// 互换：先把当前激活目录改名为自己的 tag 槽位（清理残留同名槽位），再把目标槽位
/// 改名为激活目录；任一步失败回滚。切换前先停服务，避免目录被进程句柄锁定（Windows DLL 锁）。
async fn switch_app_version(app_handle: &AppHandle, tag: &str) -> Result<(), String> {
    let deps = dependencies_dir(app_handle);
    let active_dir = config::get_dsh_install_path(app_handle);
    let target_dir = existing_slot_dir(app_handle, tag)
        .ok_or_else(|| format!("CORE_VERSION_NOT_DOWNLOADED: {tag}"))?;
    let cur_tag = config::get_dsh_pkg_tag(app_handle);

    // 激活目录已是目标版本（tag 相同）→ 仅切来源标记（如 local → app 同版本）
    if cur_tag.as_deref() == Some(tag) {
        let mut setting = config::get_store_dat_setting(app_handle);
        setting.active_core = Some(CoreSource::App.as_str().to_string());
        config::set_store_dat_setting(app_handle, setting);
        return Ok(());
    }

    // 切换前停止运行中的服务，避免目录被进程句柄锁定
    if workflow::has_owned_process() {
        if let Err(e) = workflow::stop(app_handle.clone()).await {
            log::warn!("failed to stop harness before core switch: {e}");
        }
    }

    // 1. 当前激活目录让出激活位：改名为自己的 tag 槽位（残留槽位先清理）
    let backup_dir = match &cur_tag {
        Some(cur) => deps.join(cur),
        // 无 tag 记录（旧版安装）：用版本号兜底命名槽位
        None => deps.join(format!(
            "dsh-{}",
            config::get_dsh_version(app_handle).unwrap_or_else(|| "unknown".to_string())
        )),
    };
    if active_dir.exists() {
        if backup_dir.exists() && !download::remove_dir_with_retry(&backup_dir).await {
            return Err(format!(
                "CORE_SWITCH_FAILED: cannot clean old backup {}",
                backup_dir.display()
            ));
        }
        download::rename_with_retry(&active_dir, &backup_dir).await.map_err(|e| {
            format!(
                "CORE_SWITCH_FAILED: {} -> {}: {e}",
                active_dir.display(),
                backup_dir.display()
            )
        })?;
    }

    // 2. 目标版本进入激活位；失败回滚
    if let Err(e) = download::rename_with_retry(&target_dir, &active_dir).await {
        let _ = download::rename_with_retry(&backup_dir, &active_dir).await;
        return Err(format!(
            "CORE_SWITCH_FAILED: {} -> {}: {e}",
            target_dir.display(),
            active_dir.display()
        ));
    }

    // 3. 记录切换：tag + commit（commit 从 tags 列表反查，失败保留原值）
    let commit = match download::fetch_dsh_pkg_tags().await {
        Ok(tags) => tags.into_iter().find(|(t, _)| t == tag).map(|(_, c)| c),
        Err(e) => {
            log::warn!("failed to resolve commit for tag {tag}: {e}");
            None
        }
    };
    let mut setting = config::get_store_dat_setting(app_handle);
    setting.active_core = Some(CoreSource::App.as_str().to_string());
    setting.dsh_pkg_tag = Some(tag.to_string());
    if let Some(c) = commit {
        setting.dsh_pkg_commit = Some(c);
    }
    config::set_store_dat_setting(app_handle, setting);
    Ok(())
}

/// 下载指定 tag 的预打包核心到历史槽位 `dependencies/<tag>`（不激活，切换由
/// `set_active` 完成）。幂等：已下载时直接返回该版本行。
pub async fn download_version(app_handle: &AppHandle, tag: &str) -> Result<HarnessCore, String> {
    // 路径安全：tag 直接进入 `dependencies/<tag>` 槽位路径，需挡 `..`/分隔符
    fs_guard::validate_id(tag)?;
    let dest = slot_dir(app_handle, tag);
    if dest.exists() {
        return Ok(row_for_tag(app_handle, tag, &dest));
    }

    // 1. 拉该 tag 的资产地址 + 可信摘要（digest 缺失时安全中止，沿用
    //    DSH_INTEGRITY_UNAVAILABLE 设计：不下载无法验证完整性的内容）
    let info = download::fetch_dsh_pkg_asset(tag)
        .await
        .map_err(|e| format!("CORE_METADATA_FAILED: {e}"))?;
    let digest = info.digest.ok_or_else(|| {
        format!("CORE_INTEGRITY_UNAVAILABLE: trusted SHA-256 unavailable for {tag}, cannot download safely")
    })?;

    // 2. 下载 + 校验 + 原子解压到历史槽位（两阶段进度：下载 0-50，解压 50-100）
    //    下载默认走 GitHub 官方直连，失败自动切换 ghfast.top 镜像兜底。
    let window = app_handle
        .get_webview_window("main")
        .ok_or("WINDOW_NOT_FOUND: main window missing")?;
    let mut tracker = download::ProgressTracker::new(&window, 2);
    tracker.start_phase("download", &format!("正在下载核心版本 {tag}"));
    let urls = vec![
        info.asset_url.clone(),
        config::mirror_download_url(&info.asset_url),
    ];
    let buffer = download::download_file_from_sources(&tracker, urls)
        .await
        .map_err(|e| format!("CORE_DOWNLOAD_FAILED: {e}"))?;
    download::verify_sha256(&buffer, &digest)
        .map_err(|e| format!("CORE_INTEGRITY_FAILED: {e}"))?;
    tracker.end_phase();
    let name = info
        .asset_url
        .rsplit('/')
        .next()
        .unwrap_or(&info.asset_url)
        .to_string();
    tracker.start_phase("extract", &format!("正在解压核心版本 {tag}"));
    download::ensure_extract(&tracker, name, buffer, dest.clone())
        .await
        .map_err(|e| format!("CORE_EXTRACT_FAILED: {e}"))?;
    tracker.end_phase();
    log::info!("Downloaded dsh core {tag} to {}", dest.display());

    Ok(row_for_tag(app_handle, tag, &dest))
}

/// 卸载已下载的历史版本（激活中的版本不可卸载）。
pub async fn remove_version(app_handle: &AppHandle, id: &str) -> Result<(), String> {
    let Some(tag) = id.strip_prefix("app-") else {
        return Err(format!("CORE_INVALID_ID: {id}"));
    };
    // 路径安全：tag 需通过字符集白名单（tag 形如 `dsh-0.1.0-rc.8-<commit>`），
    // 拒绝 `..`、分隔符等，防止 `remove_core("app-..")` 把目标推出依赖根目录。
    fs_guard::validate_id(tag)?;
    let cur_tag = config::get_dsh_pkg_tag(app_handle);
    if cur_tag.as_deref() == Some(tag) && active_source(app_handle) == CoreSource::App {
        return Err(format!("CORE_ACTIVE_VERSION: cannot remove in-use version {tag}"));
    }
    let dir = existing_slot_dir(app_handle, tag)
        .ok_or_else(|| format!("CORE_VERSION_NOT_FOUND: {tag}"))?;

    // 停止服务避免句柄锁定（被删目录可能是上一份激活副本，句柄未释放）
    if workflow::has_owned_process() {
        if let Err(e) = workflow::stop(app_handle.clone()).await {
            log::warn!("failed to stop harness before core removal: {e}");
        }
    }
    if !download::remove_dir_with_retry(&dir).await {
        return Err(format!(
            "CORE_REMOVE_FAILED: cannot remove {}",
            dir.display()
        ));
    }
    Ok(())
}

/// 解析激活预打包核心的版本号：优先记录 tag（`dsh-<version>-<commit>`），
/// 解析不出（无 tag 记录/格式不符）时用安装目录清单版本兜底。
fn active_app_version(active_tag: &Option<String>, manifest_version: Option<String>) -> Option<String> {
    active_tag
        .as_deref()
        .and_then(download::parse_version_from_tag)
        .or(manifest_version)
}

/// 构造某个已下载 tag 的核心行（下载完成/已存在时返回）。
fn row_for_tag(app_handle: &AppHandle, tag: &str, dir: &Path) -> HarnessCore {
    let active = config::get_dsh_pkg_tag(app_handle).as_deref() == Some(tag)
        && active_source(app_handle) == CoreSource::App;
    let dir_str = dir.to_string_lossy().into_owned();
    HarnessCore {
        id: format!("app-{tag}"),
        source: CoreSource::App,
        version: download::parse_version_from_tag(tag).unwrap_or_default(),
        tag: tag.to_string(),
        path: dir_str.clone(),
        dir: dir_str,
        present: true,
        active,
        error: None,
    }
}

/// 本地核心是否由 pnpm 全局布局管理（决定更新走 pnpm 还是 npm）。
fn local_core_uses_pnpm(app_handle: &AppHandle) -> bool {
    let Some(bin) = find_user_dsh_bin(app_handle) else {
        return false;
    };
    let Some(prefix) = bin.parent() else {
        return false;
    };
    // npm 布局：`<prefix>/node_modules/@deepseek-ai/dsh` 命中 → npm 管理
    let npm_layout = probe_package_dir(prefix).is_some();
    // pnpm 布局：`<prefix>/global/<n>/node_modules/@deepseek-ai/dsh` 命中 → pnpm 管理
    let pnpm_layout = std::fs::read_dir(prefix.join("global"))
        .map(|entries| entries.flatten().any(|e| probe_package_dir(&e.path()).is_some()))
        .unwrap_or(false);
    !npm_layout && pnpm_layout
}

/// 通过用户包管理器 CLI 更新本地核心（npm `update -g` / pnpm `add -g`）。
///
/// 返回更新后的版本号。失败返回错误（附进程输出尾部，便于排查）。
pub async fn update_local_core(app_handle: AppHandle) -> Result<String, String> {
    let Some(core) = local_core(&app_handle) else {
        return Err("CORE_LOCAL_NOT_FOUND: no local core to update".to_string());
    };
    log::info!("Updating local dsh core at {}", core.package_dir.display());

    let uses_pnpm = local_core_uses_pnpm(&app_handle);
    // 更新到最新：pnpm 用 `add -g @latest`，npm 用 `install -g @latest`
    // （npm update 对已固定版本号的安装可能不生效，install @latest 才能升级）
    let (pm, args): (&str, Vec<String>) = if uses_pnpm {
        (
            "pnpm",
            vec!["add".to_string(), "-g".to_string(), "@deepseek-ai/dsh@latest".to_string()],
        )
    } else {
        (
            "npm",
            vec!["install".to_string(), "-g".to_string(), "@deepseek-ai/dsh@latest".to_string()],
        )
    };
    log::info!("Updating local dsh core via `{pm} {args:?}`");

    // GUI 进程下 npm/pnpm 均以控制台程序方式派生子进程，Windows 上需隐藏窗口
    let program = if cfg!(windows) { format!("{pm}.cmd") } else { pm.to_string() };
    let (status, stdout, stderr) = tauri::async_runtime::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(&program);
        cmd.args(&args);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        match cmd.output() {
            Ok(out) => (out.status.success(), out.stdout, out.stderr),
            Err(e) => (false, Vec::new(), format!("spawn failed: {e}").into_bytes()),
        }
    })
    .await
    .map_err(|e| format!("CORE_UPDATE_JOIN: {e}"))?;

    let tail = |bytes: Vec<u8>| -> String {
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    };

    if !status {
        let output = tail(stdout.into_iter().chain(stderr.into_iter()).collect());
        return Err(format!("CORE_UPDATE_FAILED: {output}"));
    }

    // 更新成功后按新包目录回读版本
    let version = local_core(&app_handle)
        .map(|c| c.version)
        .unwrap_or_default();
    if version.is_empty() {
        log::warn!("Local core updated but version could not be re-read");
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_source_round_trips() {
        assert_eq!(CoreSource::parse("local"), Some(CoreSource::Local));
        assert_eq!(CoreSource::parse("app"), Some(CoreSource::App));
        assert_eq!(CoreSource::parse("other"), None);
        assert_eq!(CoreSource::Local.as_str(), "local");
        assert_eq!(CoreSource::App.as_str(), "app");
    }

    #[test]
    fn read_package_version_parses_manifest() {
        let dir = std::env::temp_dir().join(format!("dsh-core-ver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh","version":"0.1.0-rc.8"}"#,
        )
        .unwrap();
        assert_eq!(
            read_package_version(&dir).as_deref(),
            Some("0.1.0-rc.8")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_dedupes_versions_keeping_last_tag() {
        // 模拟 pkg 仓库的测试打包：同一版本打多个 tag（最新在前），去重后每个版本
        // 只保留最后一个 tag，且顺序保持首次出现顺序（版本新→旧）。
        let tags = vec![
            ("dsh-0.1.1-rc.1-32342588166".to_string(), "c1".to_string()),
            ("dsh-0.1.0-rc.8-32331963388".to_string(), "c2".to_string()),
            // 同版本重复：后续 tag（测试打包）应被去重掉，保留最后一个
            ("dsh-0.1.0-rc.8-32342588166".to_string(), "c3".to_string()),
            ("dsh-0.1.0-rc.8-32342588167".to_string(), "c4".to_string()),
            ("dsh-0.1.0-rc.7-31773193667".to_string(), "c5".to_string()),
            ("dsh-0.1.0-rc.7-31773193668".to_string(), "c6".to_string()),
        ];
        let mut version_tags: Vec<(String, String)> = Vec::new();
        for (tag, _commit) in &tags {
            let Some(version) = download::parse_version_from_tag(tag) else {
                continue;
            };
            if let Some(entry) = version_tags.iter_mut().find(|(v, _)| v == &version) {
                entry.1 = tag.clone();
            } else {
                version_tags.push((version, tag.clone()));
            }
        }
        let versions: Vec<&str> = version_tags.iter().map(|(v, _)| v.as_str()).collect();
        let kept_tags: Vec<&str> = version_tags.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(versions, vec!["0.1.1-rc.1", "0.1.0-rc.8", "0.1.0-rc.7"]);
        // rc.8 / rc.7 都保留了最后一个 tag
        assert_eq!(kept_tags[1], "dsh-0.1.0-rc.8-32342588167");
        assert_eq!(kept_tags[2], "dsh-0.1.0-rc.7-31773193668");
    }
}
