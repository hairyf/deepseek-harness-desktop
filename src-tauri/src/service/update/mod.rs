//! 桌面应用自更新模块。
//!
//! 与 `dsh` 内核更新（`download` 模块）不同，这里负责「DeepSeek Harness 桌面端」
//! 自身的更新：查询 GitHub Release 的最新版本、下载安装包、并交给系统打开安装器。
//!
//! 设计考量：
//! - 每次「检查更新」都实时向 GitHub 查询最新 Release（不做缓存），保证看到的
//!   永远是最新发布，不会因上传期间的旧结果而误判「已是最新」。
//! - 通过 GitHub 的 **HTML/atom 页面**（releases.atom、expanded_assets）而非
//!   api.github.com 查询，绕开未认证 API 60 次/小时/IP 的限流。
//! - 安装包下载到 AppData/updates 目录；已存在则视为「已下载」，不再重复拉取。
//! - 打开安装器（exe/msi/dmg 等）交给系统默认处理器（ShellExecute/LaunchServices）。

use std::path::PathBuf;
use std::time::Duration;

use futures_util::StreamExt;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_opener::OpenerExt;

use crate::config;
use crate::service::download::verify_sha256;

/// 仓库主页（同时用于构造 atom / expanded_assets / 下载地址）
const REPO_URL: &str = "https://github.com/hairyf/deepseek-harness-desktop";
/// 版权信息（与 tauri.conf.json bundle.copyright 保持一致）
const COPYRIGHT: &str = "Copyright © 2026 Deepseek Harness Desktop contributors";
/// About 对话框的 "Powered by" 文案
const POWERED_BY: &str = "DeepSeek Harness";
/// AppData 下安装包存放目录名
const UPDATES_DIR: &str = "updates";
/// 安装包下载总时长上限（秒）：超过即视为失败，防止 ghfast.top 等慢源拖死界面
const DOWNLOAD_TIMEOUT_SECS: u64 = 120;

/// 最新可用发布信息（仅在有更新且匹配到当前平台安装包时才有意义）
#[derive(Debug, Clone)]
struct LatestRelease {
    version: String,
    tag: String,
    published_at: String,
    url: String,
    asset_name: String,
    /// release 资产页（expanded_assets）中作者填写的 SHA-256 摘要
    /// （`sha256:<64hex>`）。`None` 表示无法取得可信摘要——此时镜像源
    /// 不可用作下载（无完整性凭据），仅官方直连可按旧行为继续。
    digest: Option<String>,
}

/// 当前桌面端版本号（来自 Cargo.toml / tauri.conf.json）
fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 解析版本号为数字段序列：`v0.5.2` / `0.5.2` → [0, 5, 2]
fn parse_version(v: &str) -> Option<Vec<u64>> {
    let s = v.trim().trim_start_matches('v');
    s.split('.')
        .map(|p| p.parse().ok())
        .collect::<Option<Vec<_>>>()
}

/// 判断 `latest` 是否严格高于 `current`（逐段比较，段数多者视作更新）
fn is_newer(latest: &str, current: &str) -> bool {
    let Some(a) = parse_version(latest) else {
        return false;
    };
    let Some(b) = parse_version(current) else {
        return false;
    };
    for (x, y) in a.iter().zip(b.iter()) {
        if x != y {
            return x > y;
        }
    }
    a.len() > b.len()
}

/// 根据资产文件名判断其架构匹配度，用于同扩展名下挑选正确架构的安装包：
/// - `2`：与当前运行架构完全匹配（如 `_x64.dmg` / `_aarch64.dmg` / `_amd64.deb`）
/// - `1`：通用包（`universal`），任何架构都可用
/// - `0`：不匹配或文件名未携带架构信息（作为兜底仍可尝试）
fn arch_rank(name: &str) -> i8 {
    let lower = name.to_lowercase();
    if lower.contains("universal") {
        return 1;
    }
    #[cfg(target_arch = "aarch64")]
    let markers = ["aarch64", "arm64", "apple-silicon", "-arm", "_arm"];
    #[cfg(target_arch = "x86_64")]
    let markers = ["x86_64", "amd64", "x64", "intel", "-x86", "_x86"];
    if markers.iter().any(|k| lower.contains(k)) {
        2
    } else {
        0
    }
}

/// 选择当前平台对应的安装包资产文件名。
///
/// 选择规则分两层：先按平台偏好扩展名排序，同扩展名下再按架构匹配度挑选。
/// - Windows 优先 NSIS setup.exe（其次 msi）：NSIS 不会像 MSI 那样由
///   RestartManager 强杀旧进程并在安装完成后自动重开应用，避免应用在旧进程
///   被强杀、运行文件瞬时缺失的窗口被自动拉起，从而误触发核心重下载。
/// - macOS 选 dmg，并按架构区分，避免 Intel 芯片 Mac 下载到 M 芯片
///   （aarch64）的安装包（issue #33）。
/// - Linux 选 AppImage（其次 deb/rpm），同样按架构匹配。
fn pick_asset(assets: &[String]) -> Option<String> {
    #[cfg(target_os = "windows")]
    let prefs = [".exe", ".msi"];
    #[cfg(target_os = "macos")]
    let prefs = [".dmg"];
    #[cfg(target_os = "linux")]
    let prefs = [".AppImage", ".deb", ".rpm"];

    let mut best: Option<(usize, i8, String)> = None;
    for name in assets {
        let Some(idx) = prefs.iter().position(|p| name.ends_with(p)) else {
            continue;
        };
        let rank = prefs.len() - idx; // 扩展名优先级：越靠前越高
        let ar = arch_rank(name); // 架构匹配度：同扩展名下优先选匹配架构
        if best
            .as_ref()
            .is_none_or(|(r, a, _)| rank > *r || (rank == *r && ar > *a))
        {
            best = Some((rank, ar, name.clone()));
        }
    }
    best.map(|(_, _, name)| name)
}

/// 构造带统一 UA 的 HTTP 客户端（并发小、超时短）。
fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("deepseek-harness-desktop")
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("UPDATE_CLIENT: {e}"))
}

/// 定位 `marker` 之后到 `end_marker` 之间的内容（用于轻量解析 atom/HTML）。
fn find_token<'a>(s: &'a str, marker: &str, end_marker: &str) -> Option<&'a str> {
    let start = s.find(marker)? + marker.len();
    let end = s[start..].find(end_marker).map(|e| start + e)?;
    Some(&s[start..end])
}

/// 从 releases.atom 解析最新 release 的 (tag, 发布时间)。
///
/// 不走 api.github.com，故不受未认证限流约束。
async fn fetch_latest_meta() -> Result<(String, String), String> {
    let body = http_client()?
        .get(format!("{REPO_URL}/releases.atom"))
        .send()
        .await
        .map_err(|e| format!("UPDATE_ATOM: {e}"))?
        .error_for_status()
        .map_err(|e| format!("UPDATE_ATOM: {e}"))?
        .text()
        .await
        .map_err(|e| format!("UPDATE_ATOM: {e}"))?;

    // 取第一条 <entry> 作为最新 release
    let entry = body
        .find("<entry>")
        .and_then(|p| body[p..].find("</entry>").map(|e| &body[p..p + e]))
        .unwrap_or(&body);
    let tag = find_token(entry, "releases/tag/", "\"")
        .ok_or_else(|| "UPDATE_PARSE: missing tag in atom feed".to_string())?
        .to_string();
    let published_at = find_token(entry, "<updated>", "</updated>")
        .unwrap_or_default()
        .to_string();
    Ok((tag, published_at))
}

/// 从 expanded_assets 页面 HTML 中提取给定 tag 的全部资产文件名（纯函数，便于测试）。
fn extract_asset_names(html: &str, tag: &str) -> Vec<String> {
    let needle = format!("releases/download/{tag}/");
    let mut names = Vec::new();
    let mut start = 0;
    while let Some(pos) = html[start..].find(&needle) {
        let after = start + pos + needle.len();
        let end = html[after..].find('"').map(|e| after + e).unwrap_or(html.len());
        names.push(html[after..end].to_string());
        start = end;
    }
    names
}

/// 从 expanded_assets HTML 片段中解析指定资产文件名后的 `sha256:<64hex>` 摘要。
///
/// 与 `download::core` 中 dsh 包的解析算法保持一致（非签名，仅页面元数据兜底，
/// 不能替代独立信任根）；解析失败/缺失返回 `None`。
fn parse_digest_from_expanded_assets(body: &str, expected_name: &str) -> Option<String> {
    let pos = body.find(expected_name)?;
    // 4096 字节窗口的终点回退到 UTF-8 字符边界，避免切片落在多字节字符中间 panic
    let mut end = (pos + 4096).min(body.len());
    while end > pos && !body.is_char_boundary(end) {
        end -= 1;
    }
    let window = &body[pos..end];
    const START: &str = "sha256:";
    let hash_start = window.find(START)?;
    let hash = &window[hash_start + START.len()..];
    let hex_end = hash
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(hash.len());
    if hex_end != 64 {
        return None;
    }
    Some(format!("sha256:{}", &hash[..64]))
}

/// 一次性拉取 expanded_assets 页面，同时提取资产名列表与该 release 的摘要哈希映射。
///
/// 返回值：(资产名列表, 任一资产名的摘要，若存在)。
async fn fetch_expanded_assets(tag: &str) -> Result<(Vec<String>, Option<String>), String> {
    let body = http_client()?
        .get(format!("{REPO_URL}/releases/expanded_assets/{tag}"))
        .send()
        .await
        .map_err(|e| format!("UPDATE_ASSETS: {e}"))?
        .error_for_status()
        .map_err(|e| format!("UPDATE_ASSETS: {e}"))?
        .text()
        .await
        .map_err(|e| format!("UPDATE_ASSETS: {e}"))?;
    let names = extract_asset_names(&body, tag);
    let digest = digest_for(&body, &names);
    Ok((names, digest))
}

/// 从 expanded_assets 页面中，为「任一资产名」解析 SHA-256 摘要。
/// 传入资产名列表，返回其中任一资产的摘要（release 通常只有目标平台一个安装包）。
fn digest_for(body: &str, names: &[String]) -> Option<String> {
    names
        .iter()
        .find_map(|name| parse_digest_from_expanded_assets(body, name))
}

/// 查询最新 Release（无缓存，每次实时检查，走 HTML/atom 而非 api.github.com）。
///
/// 返回 `Ok(Some(LatestRelease))` 表示有更新且匹配到当前平台安装包；
/// `Ok(None)` 表示无更新（或未匹配到资产）。网络失败返回 Err。
async fn fetch_latest_release() -> Result<Option<LatestRelease>, String> {
    let (tag, published_at) = fetch_latest_meta().await?;
    let version = tag.trim_start_matches('v').to_string();
    if !is_newer(&version, &current_version()) {
        return Ok(None);
    }

    // 一次拉取 expanded_assets 页面，同时得到资产名与摘要（避免两次请求）
    let (names, digest) = fetch_expanded_assets(&tag).await?;
    let Some(asset_name) = pick_asset(&names) else {
        return Ok(None);
    };

    // 摘要缺失不阻断：官方直连仍可按旧行为下载（兼容早期未填摘要的发布），
    // 但镜像兜底需要可信摘要（见 `download`）防止投毒。
    log::debug!(
        "Release {tag} digest for picked asset {}: {}",
        asset_name,
        digest.as_deref().map(|d| &d[..12]).unwrap_or("<none>")
    );

    // 下载地址由 tag + 资产名直接构造，无需 API
    let url = format!("{REPO_URL}/releases/download/{tag}/{asset_name}");
    Ok(Some(LatestRelease {
        version,
        tag,
        published_at,
        url,
        asset_name,
        digest,
    }))
}

/// 安装包存放路径（AppData/updates/<asset_name>）
fn installer_path(app_handle: &AppHandle, asset_name: &str) -> Result<PathBuf, String> {
    let dir = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| format!("UPDATE_DIR: {e}"))?
        .join(UPDATES_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("UPDATE_DIR: {e}"))?;
    Ok(dir.join(asset_name))
}

/// 检查是否有桌面端新版本。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopUpdateInfo {
    /// 最新可用版本号（无 `v` 前缀）
    pub version: String,
    /// 当前已安装版本号（无 `v` 前缀）
    pub current_version: String,
    pub tag: String,
    pub published_at: String,
    pub url: String,
    pub asset_name: String,
    pub path: String,
    pub downloaded: bool,
}

/// 检查是否有新版本可用（含安装包是否已下载）
pub async fn check(app_handle: &AppHandle) -> Result<Option<DesktopUpdateInfo>, String> {
    match fetch_latest_release().await? {
        None => Ok(None),
        Some(r) => {
            let path = installer_path(app_handle, &r.asset_name)?;
            let downloaded = path.exists();
            Ok(Some(DesktopUpdateInfo {
                version: r.version,
                current_version: current_version(),
                tag: r.tag,
                published_at: r.published_at,
                url: r.url,
                asset_name: r.asset_name,
                path: path.to_string_lossy().into_owned(),
                downloaded,
            }))
        }
    }
}

/// 下载进度载荷（前端进度条展示）
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopDownloadProgress {
    pub percentage: f64,
    pub downloaded: u64,
    pub total: u64,
    /// 附加提示（如切换下载源），无提示时为 None
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// 从单个下载源流式下载安装包到临时文件；失败时清理半成品（避免残留
/// 部分字节被误判为「已下载」）。
async fn download_from_source(
    client: &reqwest::Client,
    url: &str,
    tmp: &std::path::Path,
    app_handle: &AppHandle,
) -> Result<(), String> {
    log::info!("Downloading desktop installer from {}", url);
    let res = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("UPDATE_DOWNLOAD: {e}"))?
        .error_for_status()
        .map_err(|e| format!("UPDATE_DOWNLOAD: {e}"))?;

    let total = res.content_length().unwrap_or(0);
    let mut file = std::fs::File::create(tmp).map_err(|e| format!("UPDATE_FILE: {e}"))?;
    use std::io::Write;
    let mut downloaded: u64 = 0;
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("UPDATE_DOWNLOAD: {e}"))?;
        file.write_all(&chunk).map_err(|e| format!("UPDATE_FILE: {e}"))?;
        downloaded += chunk.len() as u64;
        let pct = if total > 0 {
            (downloaded as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        let _ = app_handle.emit(
            "desktop-update-progress",
            DesktopDownloadProgress {
                percentage: pct,
                downloaded,
                total,
                message: None,
            },
        );
    }
    drop(file);
    Ok(())
}

/// 安装包下载客户端：长超时（安装包可达数百 MB，慢镜像需要更久），
/// 与检查更新用的 5s `http_client()` 区分。
fn download_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("deepseek-harness-desktop")
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("UPDATE_CLIENT: {e}"))
}

/// 组装安装包下载源列表：官方 GitHub 直连 + （存在可信摘要时）ghfast.top 镜像。
///
/// 安全策略：第三方镜像没有独立信任根，仅在其内容可被 SHA-256 校验（摘要已取得）
/// 时才提供兜底；否则只允许官方直连，宁可在官方不可用时失败，也不冒投毒风险。
fn download_sources(release: &LatestRelease) -> Vec<String> {
    let mut urls = vec![release.url.clone()];
    if release.digest.is_some() {
        urls.push(config::mirror_download_url(&release.url));
    }
    urls
}

/// 下载桌面端安装包；已下载则直接返回。
///
/// 下载期间通过 `desktop-update-progress` 事件推送进度；完成后返回
/// `DesktopUpdateInfo`（path/downloaded 已更新）。
///
/// 下载源策略：先取 `expanded_assets` 页面的 SHA-256 摘要作为完整性凭据，再
/// 选择下载源——**镜像兜底（ghfast.top）仅在已取得可信摘要时才可使用**，否则
/// 宁可失败，防止第三方镜像投毒未被察觉；官方 GitHub 直连在摘要缺失时仍可
/// 按旧行为下载（兼容早期未填摘要的发布），下载后若有摘要则强制校验。
pub async fn download(app_handle: &AppHandle) -> Result<DesktopUpdateInfo, String> {
    let release = fetch_latest_release()
        .await?
        .ok_or_else(|| "UPDATE_NONE".to_string())?;
    let path = installer_path(app_handle, &release.asset_name)?;

    if path.exists() {
        log::info!("Installer already downloaded: {}", path.display());
        return check(app_handle)
            .await?
            .ok_or_else(|| "UPDATE_NONE".to_string());
    }

    let client = download_client()?;

    // 官方直连 → （可选）ghfast.top 镜像兜底。安装包无 SHA-256 元数据，切换源时
    // 丢弃上一源的部分字节从头下载，避免混用两个源的字节流。
    // 安全策略：镜像兜底要求已有可信摘要，否则不提供镜像（宁可失败）。
    let urls = download_sources(&release);
    if urls.len() == 1 {
        log::warn!(
            "No SHA-256 digest available for {}, mirror fallback disabled",
            release.asset_name
        );
    }
    let tmp = path.with_extension("part");
    let mut last_err = String::new();
    for (index, url) in urls.iter().enumerate() {
        if index > 0 {
            // 走镜像仅在存在可信摘要时发生（见上方 urls 组装）
            let host = reqwest::Url::parse(url)
                .ok()
                .and_then(|parsed| parsed.host_str().map(|h| h.to_string()))
                .unwrap_or_else(|| url.clone());
            log::warn!(
                "Primary desktop update source failed, switching to fallback: {}",
                url
            );
            let _ = app_handle.emit(
                "desktop-update-progress",
                DesktopDownloadProgress {
                    percentage: 0.0,
                    downloaded: 0,
                    total: 0,
                    message: Some(format!("主下载源不可用，已切换镜像源重试（{host}）")),
                },
            );
        }
        // 先写临时文件再原子改名，避免下载中断残留半成品被误判为「已下载」
        let _ = std::fs::remove_file(&tmp);
        match download_from_source(&client, url, &tmp, app_handle).await {
            Ok(()) => {
                last_err.clear();
                break;
            }
            Err(e) => last_err = e,
        }
    }
    if !last_err.is_empty() {
        return Err(format!(
            "UPDATE_DOWNLOAD: {last_err}（已尝试 {} 个下载源）",
            urls.len()
        ));
    }

    // 完整性校验：摘要存在（镜像路径必有）则强制校验，校验失败即拒绝，
    // 不保留为可安装文件，也不能被 open_installer 打开。
    if let Some(digest) = &release.digest {
        let bytes = std::fs::read(&tmp).map_err(|e| format!("UPDATE_FILE: {e}"))?;
        if let Err(e) = verify_sha256(&bytes, digest) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("UPDATE_DOWNLOAD: {e}"));
        }
        log::info!("Installer SHA-256 verified for {}", release.asset_name);
    }

    std::fs::rename(&tmp, &path).map_err(|e| format!("UPDATE_FILE: {e}"))?;

    check(app_handle)
        .await?
        .ok_or_else(|| "UPDATE_NONE".to_string())
}

/// 打开安装包：交给系统默认处理器（Windows 会触发 UAC 执行安装器）。
///
/// 安全边界：仅允许打开 `AppData/updates/` 目录内、且文件名与资产名一致的
/// 安装包——任意路径、绝对/相对遍历、`..` 都会拒绝，避免被伪装的 frame 或
/// 插件利用去执行任意文件。
pub async fn open_installer(app_handle: &AppHandle, path: String) -> Result<(), String> {
    let updates_dir = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| format!("UPDATE_DIR: {e}"))?
        .join(UPDATES_DIR);
    let p = std::path::Path::new(&path);
    if !p.exists() || !p.is_file() {
        return Err(format!("UPDATE_NOT_FOUND: {path}"));
    }
    // 规范化后必须仍在 updates 目录内（防 `..`、符号链接、路径穿越）
    let canonical = p.canonicalize().map_err(|e| format!("UPDATE_OPEN: {e}"))?;
    let updates_real = updates_dir
        .canonicalize()
        .map_err(|e| format!("UPDATE_DIR: {e}"))?;
    if !canonical.starts_with(&updates_real) {
        log::error!(
            "Rejecting open_installer outside updates dir: {} (root {})",
            canonical.display(),
            updates_real.display()
        );
        return Err("UPDATE_PATH_REJECTED: installer path is outside updates directory".to_string());
    }
    log::info!("Opening desktop installer: {}", p.display());
    app_handle
        .opener()
        .open_path(path, None::<&str>)
        .map_err(|e| format!("UPDATE_OPEN: {e}"))
}

/// 关于对话框信息。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAboutInfo {
    pub version: String,
    pub published_at: String,
    pub copyright: String,
    pub repo: String,
    pub powered_by: String,
}

/// 关于信息：版本来自编译常量，发布时间每次实时查询最新 Release（不缓存），
/// 查询失败则留空、不影响展示。
pub async fn about() -> DesktopAboutInfo {
    let published_at = fetch_latest_meta().await.map(|(_, p)| p).unwrap_or_default();
    DesktopAboutInfo {
        version: current_version(),
        published_at,
        copyright: COPYRIGHT.to_string(),
        repo: REPO_URL.to_string(),
        powered_by: POWERED_BY.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_strips_v_prefix() {
        assert_eq!(parse_version("v0.5.2").as_deref(), Some(&[0u64, 5, 2][..]));
        assert_eq!(parse_version("0.5.2").as_deref(), Some(&[0u64, 5, 2][..]));
        assert_eq!(parse_version("0.5").as_deref(), Some(&[0u64, 5][..]));
        assert_eq!(parse_version("abc"), None);
    }

    #[test]
    fn is_newer_compares_segments() {
        assert!(is_newer("0.5.2", "0.5.1"));
        assert!(is_newer("1.0.0", "0.9.0"));
        assert!(is_newer("0.5.0", "0.5"));
        assert!(!is_newer("0.5.1", "0.5.2"));
        assert!(!is_newer("0.5.1", "0.5.1"));
        assert!(!is_newer("0.5.1", "1.0.0"));
    }

    #[test]
    fn is_newer_ignores_unparseable() {
        assert!(!is_newer("abc", "0.5.1"));
        assert!(!is_newer("0.5.1", "abc"));
    }

    #[test]
    fn pick_asset_prefers_matching_suffix() {
        let mk = |name: &str| name.to_string();
        #[cfg(target_os = "windows")]
        {
            // NSIS setup.exe 优先于 msi（避免 MSI 的 RestartManager 强杀+自动重开）
            let assets: Vec<String> = vec![mk("app-x86_64-setup.exe"), mk("app-x64_en-US.msi")];
            assert_eq!(pick_asset(&assets).as_deref(), Some("app-x86_64-setup.exe"));
        }
        #[cfg(target_os = "macos")]
        {
            let assets: Vec<String> = vec![mk("app.dmg"), mk("app-x86_64.tar.gz")];
            assert_eq!(pick_asset(&assets).as_deref(), Some("app.dmg"));
        }
        let no_match: Vec<String> = vec![mk("README.md")];
        assert!(pick_asset(&no_match).is_none());
        assert!(pick_asset(&[]).is_none());
    }

    #[test]
    fn arch_rank_matches_host_and_universal() {
        // 通用包任何架构都可用
        assert_eq!(arch_rank("Deepseek.Harness.Desktop-universal.dmg"), 1);
        // 按编译目标分支断言，保证 CI 在任意架构上都能通过
        #[cfg(target_arch = "aarch64")]
        {
            assert_eq!(arch_rank("Deepseek.Harness.Desktop_0.6.6_aarch64.dmg"), 2);
            assert_eq!(arch_rank("Deepseek.Harness.Desktop_0.6.6_x64.dmg"), 0);
        }
        #[cfg(target_arch = "x86_64")]
        {
            assert_eq!(arch_rank("Deepseek.Harness.Desktop_0.6.6_x64.dmg"), 2);
            assert_eq!(arch_rank("Deepseek.Harness.Desktop_0.6.6_amd64.AppImage"), 2);
            assert_eq!(arch_rank("Deepseek.Harness.Desktop-0.6.6-1.x86_64.rpm"), 2);
            assert_eq!(arch_rank("Deepseek.Harness.Desktop_0.6.6_aarch64.dmg"), 0);
        }
        // 未携带架构信息的文件名作为兜底（0）
        assert_eq!(arch_rank("app.dmg"), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pick_asset_prefers_host_arch_dmg() {
        let mk = |name: &str| name.to_string();
        // aarch64 与 x64 并存（与真实发布资产命名一致）：选当前架构匹配的包
        let assets: Vec<String> = vec![
            mk("Deepseek.Harness.Desktop_0.6.6_aarch64.dmg"),
            mk("Deepseek.Harness.Desktop_0.6.6_x64.dmg"),
        ];
        let picked = pick_asset(&assets).unwrap();
        #[cfg(target_arch = "aarch64")]
        assert_eq!(picked, "Deepseek.Harness.Desktop_0.6.6_aarch64.dmg");
        #[cfg(target_arch = "x86_64")]
        assert_eq!(picked, "Deepseek.Harness.Desktop_0.6.6_x64.dmg");
        // 通用包优于与本机架构不匹配的包（用「非本机架构」的名字构造，任意架构成立）
        #[cfg(target_arch = "aarch64")]
        let wrong = "Deepseek.Harness.Desktop_0.6.6_x64.dmg";
        #[cfg(target_arch = "x86_64")]
        let wrong = "Deepseek.Harness.Desktop_0.6.6_aarch64.dmg";
        let assets: Vec<String> = vec![
            wrong.to_string(),
            "Deepseek.Harness.Desktop_0.6.6-universal.dmg".to_string(),
        ];
        let picked = pick_asset(&assets).unwrap();
        assert_eq!(picked, "Deepseek.Harness.Desktop_0.6.6-universal.dmg");
    }

    #[test]
    fn find_token_extracts_between_markers() {
        let s = r#"<link rel="alternate" href="https://github.com/x/releases/tag/v0.6.6"/>"#;
        assert_eq!(find_token(s, "releases/tag/", "\""), Some("v0.6.6"));
        let s2 = "<updated>2026-08-19T09:27:38Z</updated>";
        assert_eq!(find_token(s2, "<updated>", "</updated>"), Some("2026-08-19T09:27:38Z"));
        assert_eq!(find_token("no marker", "releases/tag/", "\""), None);
    }

    #[test]
    fn extract_asset_names_parses_download_links() {
        let tag = "v0.6.6";
        let html = r#"
            <a href="/hairyf/deepseek-harness-desktop/releases/download/v0.6.6/x64-setup.exe">x</a>
            <a href="/hairyf/deepseek-harness-desktop/releases/download/v0.6.6/x64_en-US.msi">y</a>
            <a href="/hairyf/deepseek-harness-desktop/releases/download/v0.6.5/old.dmg">z</a>
        "#;
        let names = extract_asset_names(html, tag);
        assert_eq!(names, vec!["x64-setup.exe", "x64_en-US.msi"]);
        assert!(extract_asset_names(html, "v9.9.9").is_empty());
        assert!(extract_asset_names("", tag).is_empty());
    }

    /// 摘要解析回归：识别 `sha256:<64hex>`（含中文/多字节前缀），拒绝非法摘要。
    #[test]
    fn parse_digest_from_expanded_assets_extracts_sha256() {
        let hex = format!("sha256:{}", "a".repeat(64));
        let html = format!(
            r#"<td>设置包</td><td class="d-block">app.dmg</td><td>下载</td><td>{hex}</td>"#
        );
        let digest = parse_digest_from_expanded_assets(&html, "app.dmg");
        let expected = format!("sha256:{}", "a".repeat(64));
        assert_eq!(digest.as_deref(), Some(expected.as_str()));

        // 无匹配资产 → None
        assert!(parse_digest_from_expanded_assets(&html, "app-x86_64.dmg").is_none());
        // 摘要长度/字符不合法 → None
        let bad = r#"<td>app.dmg sha256:zz"#;
        assert!(parse_digest_from_expanded_assets(bad, "app.dmg").is_none());
        // 多字节内容前移后仍能解析（切片边界安全）
        let unicode = format!(
            "中文说明app.dmg{}更多内容",
            hex
        );
        assert!(parse_digest_from_expanded_assets(&unicode, "app.dmg").is_some());
    }

    /// 镜像兜底策略回归：无摘要时只有官方源；有摘要时才加入镜像。
    #[test]
    fn download_sources_only_mirrors_with_digest() {
        let url = "https://github.com/x/y/releases/download/v0.7.4/x.dmg";
        let base = LatestRelease {
            version: "0.7.4".into(),
            tag: "v0.7.4".into(),
            published_at: String::new(),
            url: url.into(),
            asset_name: "x.dmg".into(),
            digest: None,
        };
        // 无摘要 → 仅官方直连
        let without = download_sources(&base);
        assert_eq!(without.len(), 1);
        assert_eq!(without[0], url);
        // 有摘要 → 官方 + 镜像
        let with_digest = LatestRelease {
            digest: Some(format!("sha256:{}", "b".repeat(64))),
            ..base.clone()
        };
        let sources = download_sources(&with_digest);
        assert_eq!(sources.len(), 2);
        assert!(sources[1].contains("ghfast.top"), "镜像应为 ghfast.top 前缀: {}", sources[1]);
        assert!(sources[1].ends_with("/releases/download/v0.7.4/x.dmg"), "镜像保留完整资产路径: {}", sources[1]);
    }
}
