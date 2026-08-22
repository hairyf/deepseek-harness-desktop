use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config;
use crate::service::download::ProgressTracker;
use tauri::Runtime;

/// 下载文件到内存
///
/// # 参数
/// - `tracker`: 进度追踪器
/// - `url`: 要下载的文件 URL
///
/// # 返回
/// 成功返回文件内容 `Ok(Vec<u8>)`，失败返回错误信息
pub async fn download_file<'a, R: Runtime>(
    tracker: &'a ProgressTracker<'a, R>,
    url: String,
) -> Result<Vec<u8>, String> {
    download_file_from_sources(tracker, vec![url]).await
}

/// 按顺序依次尝试多个下载源（如 GitHub 官方直连 → ghfast.top 镜像兜底），
/// 某个源全部重试失败后自动切换下一个源，并通过 `tracker` 在界面上告知用户
/// 当前使用的下载源；全部源均失败时返回最后一个源的错误并注明尝试过的源数。
///
/// 每个源内部仍走 `download_with_retry` 的断点续传重试；切换源时保留已下载
/// 的字节续传（镜像透传同一文件，内容一致，且最终有 SHA-256 完整性校验兜底；
/// 服务端不支持 Range 时 `download_attempt` 会自动清空从头下载）。
pub async fn download_file_from_sources<'a, R: Runtime>(
    tracker: &'a ProgressTracker<'a, R>,
    urls: Vec<String>,
) -> Result<Vec<u8>, String> {
    if urls.is_empty() {
        return Err("DOWNLOAD_URL_EMPTY: no download source provided".to_string());
    }
    let mut last_err = String::new();
    for (index, url) in urls.iter().enumerate() {
        // 切换下载源时告知用户（detail 展示在进度面板，log 进入日志流）
        if index > 0 {
            let host = reqwest::Url::parse(url)
                .ok()
                .and_then(|parsed| parsed.host_str().map(|h| h.to_string()))
                .unwrap_or_else(|| url.clone());
            log::warn!(
                "Primary download source failed, switching to fallback source: {}",
                url
            );
            tracker.update(
                0.0,
                format!("主下载源不可用，已切换镜像源重试（{host}）"),
                format!("Switch to fallback download source: {url}"),
            );
        }
        match download_with_retry(tracker, url).await {
            Ok(buffer) => return Ok(buffer),
            Err(e) => last_err = e,
        }
    }
    Err(if urls.len() > 1 {
        format!("{last_err}（已尝试 {} 个下载源）", urls.len())
    } else {
        last_err
    })
}

/// 对单个 URL 执行断点续传重试，全部尝试失败返回中文可读错误。
async fn download_with_retry<'a, R: Runtime>(
    tracker: &'a ProgressTracker<'a, R>,
    url: &str,
) -> Result<Vec<u8>, String> {
    log::info!("Starting file download: {}", url);
    validate_download_url(url)?;

    // 只在整个下载开始时写一次 "Download <url>" 日志，之后进度由 detail/percentage
    // 驱动：同一 URL 若随每个 50ms 进度 tick 重复写入，日志面板会把同一文件刷成
    // 多条完全相同行（历史 issue：一个文件的下载任务却出现多条日志）。
    tracker.update(0.0, String::new(), format!("Download {}", url));

    // 创建具备 User-Agent 的客户端
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (deepseek-harness-desktop)")
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| {
            log::error!("Failed to create HTTP client: {}", e);
            e.to_string()
        })?;

    // GitHub CDN（objects/release-assets.githubusercontent.com）的大文件传输
    // 偶发中途断流（连接被重置、chunked body 提前结束，表现为"error decoding
    // response body"），这类瞬时错误重试通常即可成功。重试时带 Range 头从
    // 上次断点续传（CDN 支持 206），避免每次都从头下载 38MB。
    // 最多 MAX_DOWNLOAD_ATTEMPTS 次，失败退避递增（2s/4s/8s/8s）。
    const MAX_DOWNLOAD_ATTEMPTS: usize = 5;
    // buffer 由外层持有：每次尝试在已有字节基础上续传/追加，成功后整体返回
    let mut buffer: Vec<u8> = Vec::new();
    for attempt in 1..=MAX_DOWNLOAD_ATTEMPTS {
        if attempt > 1 {
            let delay = Duration::from_secs((1 << (attempt - 1)).min(8));
            log::warn!(
                "Download attempt {}/{} failed, retrying in {}s (resume from {} bytes)",
                attempt - 1,
                MAX_DOWNLOAD_ATTEMPTS,
                delay.as_secs(),
                buffer.len()
            );
            tokio::time::sleep(delay).await;
        }
        match download_attempt(&client, tracker, url, attempt, MAX_DOWNLOAD_ATTEMPTS, &mut buffer)
            .await
        {
            Ok(()) => {
                log::info!("Download completed, {} bytes total", buffer.len());
                return Ok(buffer);
            }
            Err(e) => {
                log::warn!(
                    "Download attempt {}/{} failed: {}",
                    attempt,
                    MAX_DOWNLOAD_ATTEMPTS,
                    e
                );
            }
        }
    }
    // 全部尝试失败：只把可读的中文提示暴露给界面（原始传输错误保留在日志，
    // 避免 "error decoding response body" 这类底层信息直接糊在界面上）。
    Err(format!(
        "DOWNLOAD_INTERRUPTED: 下载中断（网络传输被重置），已自动重试 {MAX_DOWNLOAD_ATTEMPTS} 次仍失败，已下载约 {:.1} MB，请检查网络后重试",
        buffer.len() as f64 / 1_000_000.0
    ))
}

/// 下载进度百分比（0.0–100.0）；总长未知（0）时返回 -1 表示「不确定进度」，
/// 避免除零产生 +inf/NaN 污染前端进度条。
fn download_progress_percent(received_total: u64, total_size: u64) -> f64 {
    if total_size > 0 {
        (received_total as f64 / total_size as f64) * 100.0
    } else {
        -1.0
    }
}

/// 单次下载尝试：发起 GET 请求并把响应体流式读入 `buffer`。
///
/// 已有部分数据时自动带 `Range: bytes=<已有>-` 续传；服务端返回 200（不支持
/// Range）则清空从头下载。续传成功不代表整体完成——调用方必须比对摘要校验，
/// 任何中途断流都会以 Err 返回并触发外层重试。
async fn download_attempt<'a, R: Runtime>(
    client: &reqwest::Client,
    tracker: &'a ProgressTracker<'a, R>,
    url: &str,
    attempt: usize,
    max_attempts: usize,
    buffer: &mut Vec<u8>,
) -> Result<(), String> {
    let resume_from = buffer.len() as u64;
    log::debug!(
        "Download attempt {}/{}: {} (resume from {})",
        attempt,
        max_attempts,
        url,
        resume_from
    );

    let mut req = client.get(url);
    if resume_from > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={resume_from}-"));
    }
    let res = req.send().await.map_err(|e| {
        log::error!("Download request failed: {}", e);
        e.to_string()
    })?;
    validate_download_url(res.url().as_str())?;

    // 416 属防御分支：理论上不会发生（断流说明还没收完，resume_from 必然
    // 小于文件总长），若出现则清空 buffer 让下一次尝试从头下载，避免死循环。
    if res.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        log::warn!("Server returned 416 for range request, restarting from zero");
        buffer.clear();
        return Err(format!("Download failed: HTTP {}", res.status()));
    }
    if !res.status().is_success() {
        log::error!("Download failed with HTTP status: {}", res.status());
        return Err(format!("Download failed: HTTP {}", res.status()));
    }

    // 206 = 续传成功，保留已有字节只追加后续分片；200 = 服务端不支持 Range
    // （或首次下载），整体从头开始。
    let range_accepted = res.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if !range_accepted && resume_from > 0 {
        log::warn!("Server ignored Range request, restarting download from zero");
        buffer.clear();
    }

    // 进度按"已收字节 + 本次分片"计算；总长 = 断点偏移 + 本次 Content-Length
    let total_size = if range_accepted {
        resume_from + res.content_length().unwrap_or(0)
    } else {
        res.content_length().unwrap_or(0)
    };
    log::debug!("File size: {} bytes", total_size);
    let mut downloaded: u64 = 0;
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            log::error!("Download stream read error: {}", e);
            e.to_string()
        })?;
        buffer.extend_from_slice(&chunk);
        downloaded += chunk.len() as u64;
        let received_total = resume_from + downloaded;
        // 未知总长（服务器未给 Content-Length/分块传输）时进度取 -1，
        // 避免 `received / 0 = +inf` 或 NaN 进度传给前端
        let progress_pct = download_progress_percent(received_total, total_size);
        tracker.update(
            progress_pct,
            format!(
                "已下载 {:.1} MB / {:.1} MB",
                received_total as f64 / 1_000_000.0,
                total_size as f64 / 1_000_000.0
            ),
            // 进度阶段不再重复写 URL：仅由上面的 "Download <url>" 一行标识本次
            // 下载，避免随进度 tick 产生大量重复日志（前端对空 log 直接忽略）。
            String::new(),
        );
    }

    log::info!("Download attempt {}/{} succeeded, {} bytes in buffer", attempt, max_attempts, buffer.len());
    Ok(())
}

fn validate_download_url(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("DOWNLOAD_URL_INVALID: {e}"))?;
    let trusted_host = matches!(
        parsed.host_str(),
        Some(
            "nodejs.org"
                | "registry.npmjs.org"
                | "github.com"
                | "release-assets.githubusercontent.com"
                | "objects.githubusercontent.com"
                // 国内镜像：npmmirror 系列（node dist 会 302 到 cdn.npmmirror.com）
                // 与 ghfast.top 中转（GitHub Release 内容原样透传）
                | "npmmirror.com"
                | "cdn.npmmirror.com"
                | "registry.npmmirror.com"
                | "ghfast.top"
        )
    );
    if parsed.scheme() != "https" || !trusted_host {
        return Err(format!("DOWNLOAD_SOURCE_UNTRUSTED: {url}"));
    }
    Ok(())
}

/// 校验下载内容的 SHA-256，拒绝未通过完整性校验的运行时与核心包。
pub fn verify_sha256(buffer: &[u8], expected: &str) -> Result<(), String> {
    let expected = expected
        .strip_prefix("sha256:")
        .unwrap_or(expected)
        .trim()
        .to_ascii_lowercase();
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("INTEGRITY_METADATA_INVALID: expected SHA-256 is invalid".to_string());
    }
    let actual = format!("{:x}", Sha256::digest(buffer));
    if actual != expected {
        return Err(format!(
            "INTEGRITY_CHECK_FAILED: SHA-256 mismatch, expected {expected}, got {actual}"
        ));
    }
    Ok(())
}

/// 从 Node.js 官方同版本 SHASUMS256.txt 中读取当前平台包的摘要。
pub async fn fetch_node_sha256(download_url: &str) -> Result<String, String> {
    let (base, filename) = download_url.rsplit_once('/').ok_or_else(|| {
        "INTEGRITY_METADATA_INVALID: Node.js download URL has no filename".to_string()
    })?;
    let checksums_url = format!("{base}/SHASUMS256.txt");
    let checksums = reqwest::Client::builder()
        .user_agent("deepseek-harness-desktop")
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("INTEGRITY_METADATA_FAILED: {e}"))?
        .get(&checksums_url)
        .send()
        .await
        .map_err(|e| format!("INTEGRITY_METADATA_FAILED: {e}"))?
        .error_for_status()
        .map_err(|e| format!("INTEGRITY_METADATA_FAILED: {e}"))?
        .text()
        .await
        .map_err(|e| format!("INTEGRITY_METADATA_FAILED: {e}"))?;

    checksums
        .lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .find_map(|(digest, name)| {
            (name.trim_start_matches([' ', '*']) == filename).then(|| digest.to_string())
        })
        .ok_or_else(|| format!("INTEGRITY_METADATA_MISSING: no checksum for {filename}"))
}

/// 删除目录并等待 Windows 文件锁释放。
///
/// 结束 dsh/node 进程后，加载进内存的 DLL 句柄不会立即释放，删除目录可能
/// 短暂失败（os error 32）。这里轮询等待，最长约 10 秒。
///
/// # 性能
/// 锁等待期间用 `tokio::time::sleep` 让出异步运行时，而不是阻塞占用一个
/// Tokio worker：安装流程的进度事件与其它异步任务（健康检查、日志轮转）
/// 不会因一次长锁等待而被一并冻结。
pub(crate) async fn remove_dir_with_retry(dest: &Path) -> bool {
    const MAX_ATTEMPTS: u32 = 40;
    const RETRY_DELAY: Duration = Duration::from_millis(250);

    for attempt in 1..=MAX_ATTEMPTS {
        match fs::remove_dir_all(dest) {
            Ok(()) => return true,
            Err(e) => {
                if attempt < MAX_ATTEMPTS {
                    log::warn!(
                        "Failed to clean {:?} (attempt {}/{}), file may be locked: {}",
                        dest,
                        attempt,
                        MAX_ATTEMPTS,
                        e
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                } else {
                    log::error!(
                        "Failed to clean {:?} after {} attempts: {}",
                        dest,
                        MAX_ATTEMPTS,
                        e
                    );
                }
            }
        }
    }
    false
}

async fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_dir() {
        if remove_dir_with_retry(path).await {
            Ok(())
        } else {
            Err(format!(
                "INSTALL_PATH_LOCKED: cannot remove {}",
                path.display()
            ))
        }
    } else {
        fs::remove_file(path)
            .map_err(|e| format!("INSTALL_PATH_REMOVE_FAILED: {}: {e}", path.display()))
    }
}

/// 重试式重命名：等待 Windows 文件锁释放（os error 32 随进程句柄释放而消失）。
///
/// 结束 dsh/node 进程树后，子进程加载进内存的 DLL 句柄释放存在短暂滞后
/// （与 remove_dir_with_retry 相同的场景），紧跟其后的目录重命名可能一次失败。
/// 这里轮询重试，最长约 30 秒；除进程句柄外，Windows 杀毒/索引服务也可能
/// 在大规模解压后短暂持有目录句柄，10 秒窗口不足以等待其释放。
/// 重试仍失败才返回底层错误交由调用方映射。
pub(crate) async fn rename_with_retry(from: &Path, to: &Path) -> Result<(), std::io::Error> {
    const MAX_ATTEMPTS: u32 = 60;
    const RETRY_DELAY: Duration = Duration::from_millis(500);

    for attempt in 1..=MAX_ATTEMPTS {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if attempt < MAX_ATTEMPTS => {
                log::warn!(
                    "Rename {:?} -> {:?} failed (attempt {attempt}/{MAX_ATTEMPTS}), file may be locked: {e}",
                    from,
                    to
                );
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("rename_with_retry loop always returns")
}

/// 将已经完整解压并验证结构的临时目录切换为正式目录；切换失败时恢复旧版本。
async fn commit_staged_install(staging: &Path, dest: &Path, backup: &Path) -> Result<(), String> {
    // 上次若恰好在“旧目录改名为备份”后崩溃，先恢复旧版本再进行本次切换。
    if !dest.exists() && backup.exists() {
        rename_with_retry(backup, dest).await.map_err(|e| {
            format!(
                "INSTALL_RECOVERY_FAILED: {} -> {}: {e}",
                backup.display(),
                dest.display()
            )
        })?;
    }
    remove_path_if_exists(backup).await?;
    let had_previous = dest.exists();
    if had_previous {
        rename_with_retry(dest, backup).await.map_err(|e| {
            format!(
                "INSTALL_BACKUP_FAILED: {} -> {}: {e}",
                dest.display(),
                backup.display()
            )
        })?;
    }
    if let Err(e) = rename_with_retry(staging, dest).await {
        if had_previous {
            let _ = fs::rename(backup, dest);
        }
        return Err(format!(
            "INSTALL_COMMIT_FAILED: {} -> {}: {e}",
            staging.display(),
            dest.display()
        ));
    }
    if had_previous {
        if let Err(e) = remove_path_if_exists(backup).await {
            // 新版本已经切换成功，备份清理失败不应把成功安装误报为失败。
            log::warn!("Failed to remove previous installation backup: {e}");
        }
    }
    Ok(())
}

/// 确保解压文件到指定目录
///
/// # 参数
/// - `tracker`: 进度追踪器
/// - `name`: 文件名
/// - `buffer`: 压缩文件内容
/// - `dest`: 解压目标目录
///
/// # 返回
/// 成功返回 `Ok(())`，失败返回错误信息
pub async fn ensure_extract<'a, R: Runtime>(
    tracker: &'a ProgressTracker<'a, R>,
    name: String,
    buffer: Vec<u8>,
    dest: PathBuf,
) -> Result<(), String> {
    log::info!("Starting file extraction: {} -> {:?}", name, dest);
    use super::extractor::{extract_tgz, extract_zip};
    use super::utils::flatten_directory;

    // 始终先落到同盘临时路径，全部成功后再原子切换，避免更新失败破坏旧版本。
    let parent = dest.parent().unwrap_or(Path::new("."));
    let leaf = dest
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("package");
    let staging = parent.join(format!(".{leaf}.installing-{}", std::process::id()));
    let backup = parent.join(format!(".{leaf}.backup"));
    remove_path_if_exists(&staging).await?;

    // 判断文件类型
    let pure_name = name.split('?').next().unwrap_or(&name).to_lowercase();
    let is_tgz = pure_name.ends_with(".tar.gz") || pure_name.ends_with(".tgz");
    let is_zip = pure_name.ends_with(".zip");
    log::debug!("File type: tgz={}, zip={}", is_tgz, is_zip);

    // 目标是文件，跳过，直接写入文件
    if !is_tgz && !is_zip {
        log::debug!("Non-compressed file, writing directly");
        if let Some(parent) = staging.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                log::error!("Failed to create parent directory: {}", e);
                e.to_string()
            })?;
        }
        fs::write(&staging, &buffer).map_err(|e| {
            log::error!("Failed to write file: {}", e);
            e.to_string()
        })?;
        tracker.update(
            100.0,
            format!("已写入: {}", "100%"),
            format!("File written: {}", staging.display()),
        );
        commit_staged_install(&staging, &dest, &backup).await?;
        log::info!("File write completed: {}", dest.display());
        return Ok(());
    }

    fs::create_dir_all(&staging).map_err(|e| {
        log::error!("Failed to create destination directory: {}", e);
        e.to_string()
    })?;

    // 根据文件类型解压
    if is_tgz {
        log::debug!("Using tgz extractor");
        extract_tgz(tracker, &buffer, &staging)?;
    } else {
        log::debug!("Using zip extractor");
        extract_zip(tracker, &buffer, &staging)?;
    }

    // 处理解压后的"套娃"文件夹
    log::debug!("Flattening directory structure");
    flatten_directory(&staging).map_err(|e| {
        log::error!("Failed to flatten directory: {}", e);
        e.to_string()
    })?;

    // 权限修复与隔离属性移除 (仅限 Unix/macOS)
    #[cfg(unix)]
    {
        use super::utils::fix_recursive_permissions;
        // 递归赋予可执行权限 (755)
        log::debug!("Fixing file permissions");
        fix_recursive_permissions(&staging).map_err(|e| {
            log::error!("Failed to fix permissions: {}", e);
            format!("Failed to fix permissions: {}", e)
        })?;

        // macOS 移除 quarantine 属性
        #[cfg(target_os = "macos")]
        {
            use std::process::Command;
            log::debug!("Removing macOS quarantine attribute");
            if let Some(path_str) = staging.to_str() {
                let _ = Command::new("xattr").args(["-cr", path_str]).output();
            }
        }
    }

    commit_staged_install(&staging, &dest, &backup).await?;
    Ok(())
}

/// GitHub API 地址（未认证限流 60 次/小时/IP，仅供每次启动检查一次）
const DSH_PKG_GITHUB_API: &str = "https://api.github.com/repos/hairyf/deepseek-harness-pkg";
/// pkg 仓库 HTML 来源；`releases.atom` 走 github.com 而非 api.github.com，不受未认证限流约束。
const DSH_PKG_REPO: &str = "https://github.com/hairyf/deepseek-harness-pkg";

/// 最新 Harness 发行版信息（版本 tag + 对应 commit hash）
#[derive(Debug, Clone, serde::Serialize)]
pub struct LatestDshPkg {
    pub tag: String,
    pub commit: String,
    pub asset_url: String,
    /// 可信 SHA-256 摘要。为 `None` 表示 GitHub API 限流/不可用未能取得可信摘要，
    /// 此时该信息**仅可作更新提示**（tag/commit 仍有效），不可用于自动重装——
    /// 重装路径会因完整性校验缺失而中止（沿用 DSH_INTEGRITY_UNAVAILABLE 安全设计）。
    pub digest: Option<String>,
}

/// 构造带 User-Agent 与超时的 GitHub 请求客户端。
fn github_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("deepseek-harness-desktop")
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))
}

/// 访问 api.github.com 的 GET 请求（所有 API 调用统一走这里）。
///
/// 带 GitHub 未认证限流（403）冷却：冷却期内直接返回 Err（由调用方走兜底来源），
/// 命中 403 时记录冷却开始。返回的 `reqwest::Response` 已通过 `error_for_status`，
/// 403 已被拦截并标记冷却，不会作为普通错误继续向下传输。
async fn github_api_get(
    client: &reqwest::Client,
    url: &str,
) -> Result<reqwest::Response, String> {
    if crate::service::download::github_api::rate_limited() {
        return Err("GitHub API rate limited (cached, using fallback sources)".to_string());
    }
    let res = client.get(url).send().await.map_err(|e| e.to_string())?;
    if res.status() == reqwest::StatusCode::FORBIDDEN {
        crate::service::download::github_api::mark_rate_limited();
        return Err(format!("GitHub API rate limited: HTTP {}", res.status()));
    }
    res.error_for_status().map_err(|e| e.to_string())
}

/// 拉取最新 release 的 JSON（含 tag、资产、摘要）。
async fn fetch_releases_latest(client: &reqwest::Client) -> Result<serde_json::Value, String> {
    github_api_get(client, &format!("{DSH_PKG_GITHUB_API}/releases/latest"))
        .await
        .map_err(|e| format!("Latest release request failed: {e}"))?
        .json()
        .await
        .map_err(|e| format!("Failed to parse latest release response: {e}"))
}

/// 通过 commits 端点把 release tag 解析为完整 commit hash。
async fn fetch_tag_commit(client: &reqwest::Client, tag: &str) -> Result<String, String> {
    let commit: serde_json::Value = github_api_get(client, &format!("{DSH_PKG_GITHUB_API}/commits/{tag}"))
        .await
        .map_err(|e| format!("Release commit request failed: {e}"))?
        .json()
        .await
        .map_err(|e| format!("Failed to parse release commit response: {e}"))?;
    commit
        .get("sha")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "Missing sha in release commit response".to_string())
}

/// 从 tag 内嵌的 build-id 提取 commit 标识：`dsh-0.1.0-rc.8-32331963388` → `32331963388`。
///
/// 当 `/commits/{tag}` 因 api.github.com 限流/网络失败时，用 build-id 兜底作为 commit，
/// 保证「版本升级」判定不因这一次要调用而整体中断（issue：rc.8 发布后无更新提示）。
fn commit_fallback_from_tag(tag: &str) -> String {
    tag.rsplit('-').next().unwrap_or(tag).to_string()
}

/// 从 releases.atom（github.com，非 api.github.com）解析最新 release tag。
///
/// 用作 API 限流/不可用时的兜底来源，仅在 API 完全不可达时调用。
async fn fetch_latest_dsh_tag_from_atom() -> Result<String, String> {
    let client = github_client()?;
    let body = client
        .get(format!("{DSH_PKG_REPO}/releases.atom"))
        .send()
        .await
        .map_err(|e| format!("DSH_ATOM: {e}"))?
        .error_for_status()
        .map_err(|e| format!("DSH_ATOM: {e}"))?
        .text()
        .await
        .map_err(|e| format!("DSH_ATOM: {e}"))?;
    // 取第一条 <entry> 作为最新 release，从中提取 releases/tag/<TAG>
    let entry = body
        .find("<entry>")
        .and_then(|p| body[p..].find("</entry>").map(|e| &body[p..p + e]))
        .unwrap_or(&body);
    entry
        .split("releases/tag/")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .ok_or_else(|| "DSH_ATOM: missing tag in atom feed".to_string())
}

/// 从 expanded_assets HTML 片段中解析指定资产文件名后的 `sha256:<64hex>` 摘要。
///
/// 纯函数，便于对真实页面片段做离线单元测试；解析失败返回 `None`。
fn parse_digest_from_expanded_assets(body: &str, expected_name: &str) -> Option<String> {
    let pos = body.find(expected_name)?;
    let window = &body[pos..(pos + 4096).min(body.len())];
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

/// 从 release 的 expanded_assets HTML（github.com，非 api.github.com，不受未认证
/// 限流 403 约束）解析指定资产的 SHA-256 摘要。
///
/// GitHub release 资产的 `digest` 字段默认只由 api.github.com 的 JSON 返回，一旦
/// API 被限流就拿不到可信摘要，更新会因完整性校验缺失被 `DSH_INTEGRITY_UNAVAILABLE`
/// 卡死。但 GitHub 的 `expanded_assets` 页面片段同样呈现作者填写的 `sha256:<hex>`
/// 摘要（发行版页面资产区展开时的 HTML），且走 github.com 普通请求、不受 API 配额
/// 限制——以它作为 API 限流时的非限流兜底来源，保证完整性校验不因 403 而失效。
async fn fetch_dsh_digest_from_expanded_assets(
    client: &reqwest::Client,
    tag: &str,
    expected_name: &str,
) -> Result<Option<String>, String> {
    let body = client
        .get(format!("{DSH_PKG_REPO}/releases/expanded_assets/{tag}"))
        .send()
        .await
        .map_err(|e| format!("DSH_EXPANDED: {e}"))?
        .error_for_status()
        .map_err(|e| format!("DSH_EXPANDED: {e}"))?
        .text()
        .await
        .map_err(|e| format!("DSH_EXPANDED: {e}"))?;
    Ok(parse_digest_from_expanded_assets(&body, expected_name))
}

/// 查询 GitHub 上最新 Harness 发行版信息。
///
/// 优先走 api.github.com（`/releases/latest` + `/commits/{tag}`），拿到可用的 tag、
/// 资产地址与可信 SHA-256 摘要。API 限流/网络失败时**不整体中断**：
/// - tag 兜底用 releases.atom（github.com，不受未认证限流约束）；
/// - commit 兜底用 tag 内嵌 build-id；
/// - 资产 URL 由平台确定性推导；
/// - digest 置 `None`（仅可提示、不可自动重装，重装时重取摘要或安全中止）。
///
/// 修复前：api.github.com 一限流 `fetch_latest_dsh_pkg_info` 直接返回 Err，
/// `check_dsh_update` 静默跳过，导致上游 rc.8 发布后桌面端迟迟不出现更新提示。
pub async fn fetch_latest_dsh_pkg_info() -> Result<LatestDshPkg, String> {
    let client = github_client()?;
    let expected_name = config::get_dsh_download_url()?
        .rsplit('/')
        .next()
        .ok_or_else(|| "Missing DSH asset filename".to_string())?
        .to_string();

    // 1. 首选 GitHub API 拉最新 release（含 tag + 资产 + 可信摘要）
    let api_release = match fetch_releases_latest(&client).await {
        Ok(release) => Some(release),
        Err(e) => {
            // 限流冷却期内不重复打 403 警告（进入冷却时已提示一次），静默走兜底
            if crate::service::download::github_api::rate_limited() {
                log::debug!(
                    "GitHub API latest release unavailable (rate-limited), using fallback sources: {e}"
                );
            } else {
                log::warn!(
                    "GitHub API latest release unavailable ({}), falling back to atom feed",
                    e
                );
            }
            None
        }
    };

    // 2. tag：优先 API，失败则从 releases.atom 兜底
    let tag_name = match &api_release {
        Some(release) => release
            .get("tag_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing tag_name in latest release response".to_string())?
            .to_string(),
        None => fetch_latest_dsh_tag_from_atom().await?,
    };

    // 3. commit：优先 API /commits/{tag}，失败用 tag 内嵌 build-id 兜底
    let commit = match fetch_tag_commit(&client, &tag_name).await {
        Ok(sha) => sha,
        Err(e) => {
            if crate::service::download::github_api::rate_limited() {
                log::debug!(
                    "GitHub API commit resolution rate-limited, using build-id fallback"
                );
            } else {
                log::warn!(
                    "Failed to resolve commit for tag {} ({}), using build-id fallback",
                    tag_name,
                    e
                );
            }
            commit_fallback_from_tag(&tag_name)
        }
    };

    // 4. 资产 URL 与摘要：仅 API 可达时资产/摘要可信；否则 URL 平台确定性回退、digest=None
    let (asset_url, mut digest) = match api_release.as_ref() {
        Some(release) => {
            let asset = release.get("assets").and_then(|value| value.as_array()).and_then(
                |assets| {
                    assets.iter().find(|asset| {
                        asset.get("name").and_then(|value| value.as_str())
                            == Some(expected_name.as_str())
                    })
                },
            );
            let asset_url = asset
                .and_then(|a| a.get("browser_download_url").and_then(|v| v.as_str()))
                .map(|u| u.to_string())
                .unwrap_or_else(|| config::get_dsh_download_url().unwrap_or_default());
            let digest = asset
                .and_then(|a| a.get("digest").and_then(|v| v.as_str()))
                .filter(|v| v.starts_with("sha256:"))
                .map(|v| v.to_string());
            (asset_url, digest)
        }
        None => (config::get_dsh_download_url()?, None),
    };

    // 4b. API 限流/不可用导致取不到可信摘要时，改从 expanded_assets HTML
    // （github.com，非 api.github.com，不受 403 限流）解析作者填写的 sha256，
    // 保证完整性校验不因 api.github.com 限流而失效、更新不被卡死。
    if digest.is_none() {
        match fetch_dsh_digest_from_expanded_assets(&client, &tag_name, &expected_name).await {
            Ok(Some(d)) => {
                log::info!(
                    "Trusted digest unavailable from GitHub API, recovered from release HTML for {}",
                    expected_name
                );
                digest = Some(d);
            }
            Ok(None) => {
                log::warn!(
                    "No digest found in release HTML for {} (tag {})",
                    expected_name,
                    tag_name
                );
            }
            Err(e) => {
                log::warn!("Failed to fetch digest from release HTML: {}", e);
            }
        }
    }

    Ok(LatestDshPkg {
        tag: tag_name,
        commit,
        asset_url,
        digest,
    })
}

/// 拉取指定 tag 的发行版信息（资产 URL + 可信摘要），供核心面板按版本下载。
///
/// 与 `fetch_latest_dsh_pkg_info` 同源策略：优先走 api.github.com
/// （`/releases/tags/{tag}` 拿资产与摘要），失败时资产 URL 平台确定性推导
/// （latest 地址的 tag 位替换）、摘要走 expanded_assets HTML；digest 仍取不到
/// 则置 `None`，调用方据此安全中止下载（沿用 DSH_INTEGRITY_UNAVAILABLE 设计）。
pub async fn fetch_dsh_pkg_asset(tag: &str) -> Result<LatestDshPkg, String> {
    let client = github_client()?;
    let expected_name = config::get_dsh_download_url()?
        .rsplit('/')
        .next()
        .ok_or_else(|| "Missing DSH asset filename".to_string())?
        .to_string();

    // 1. 优先 API 拉该 tag 的 release（含资产 + 可信摘要）
    let release = github_api_get(&client, &format!("{DSH_PKG_GITHUB_API}/releases/tags/{tag}"))
        .await
        .map_err(|e| format!("Release {tag} request failed: {e}"))?;
    let json: serde_json::Value = release
        .json()
        .await
        .map_err(|e| format!("Failed to parse release {tag} response: {e}"))?;

    let asset = json.get("assets").and_then(|value| value.as_array()).and_then(
        |assets| {
            assets.iter().find(|asset| {
                asset.get("name").and_then(|value| value.as_str())
                    == Some(expected_name.as_str())
            })
        },
    );
    let asset_url = asset
        .and_then(|a| a.get("browser_download_url").and_then(|v| v.as_str()))
        .map(|u| u.to_string())
        .unwrap_or_else(|| config::get_dsh_download_url_for_tag(tag).unwrap_or_default());
    let mut digest = asset
        .and_then(|a| a.get("digest").and_then(|v| v.as_str()))
        .filter(|v| v.starts_with("sha256:"))
        .map(|v| v.to_string());

    // 2. 摘要兜底：expanded_assets HTML（github.com，非 api.github.com）
    if digest.is_none() {
        match fetch_dsh_digest_from_expanded_assets(&client, tag, &expected_name).await {
            Ok(Some(d)) => {
                log::info!(
                    "Trusted digest unavailable from GitHub API, recovered from release HTML for {}",
                    expected_name
                );
                digest = Some(d);
            }
            Ok(None) => {
                log::warn!(
                    "No digest found in release HTML for {} (tag {})",
                    expected_name,
                    tag
                );
            }
            Err(e) => {
                log::warn!("Failed to fetch digest from release HTML: {}", e);
            }
        }
    }

    Ok(LatestDshPkg {
        tag: tag.to_string(),
        commit: commit_fallback_from_tag(tag),
        asset_url,
        digest,
    })
}

/// 从 release tag 中解析版本号：`dsh-0.1.0-rc.7-32054485373` → `0.1.0-rc.7`。
///
/// tag 约定为 `dsh-<version>-<commit 后缀>`；格式不符时返回 `None`，
/// 调用方据此回退到仅 commit 比对的旧行为，避免误判。
pub fn parse_version_from_tag(tag: &str) -> Option<String> {
    let version = tag.strip_prefix("dsh-")?.rsplit_once('-')?.0;
    (!version.is_empty()).then(|| version.to_string())
}

/// 更新判定结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateCheck {
    /// 无更新
    UpToDate,
    /// 无更新，但本地记录滞后于实际安装文件，需要修正记录
    HealUpToDate,
    /// 有更新
    UpdateAvailable,
}

/// 结合本地记录与实际安装文件判定是否有新版 Harness 可用。
///
/// 本地记录（release commit + tag）由安装流程写入；但当安装文件被外围途径
/// 更新、或安装时 GitHub API 失败未落盘，记录会滞后于文件，造成每次都误报
/// 更新。这里以磁盘上实际的 `@deepseek-ai/dsh` 版本为准核对：
/// - 最新 release 版本号与已装版本不同 → 有更新（不论 commit）；
/// - 版本号相同且 commit 一致 → 无更新；
/// - 版本号相同：记录 tag 版本也相同 → 同版本热修 → 有更新；
///   记录 tag 版本更旧（或记录无 tag，经 `legacy_tags` 反查）→ 记录滞后 → 修正记录。
///
/// 注意必须先比版本、再比 commit：dsh 仓库的 rc 发布会重打同一 git commit，
/// 因此「最新 release 的 commit 等于已装记录的 commit」并不代表没有更新，
/// 只有 tag 里的版本号才能正确区分（如 rc.8 之于 rc.7）。
///
/// `legacy_tags` 是 pkg 仓库的 tags 列表（tag, commit），仅用于反查历史安装
/// 记录的版本；反查不到时以实际文件为准（视为记录滞后）。
pub fn resolve_update(
    record_commit: Option<&str>,
    record_tag: Option<&str>,
    installed_version: Option<&str>,
    latest: &LatestDshPkg,
    legacy_tags: &[(String, String)],
) -> UpdateCheck {
    let (Some(installed), Some(latest_version)) =
        (installed_version, parse_version_from_tag(&latest.tag))
    else {
        // 版本信息不可解析时回退旧行为：记录不一致即视为有更新
        return if record_commit == Some(latest.commit.as_str()) {
            UpdateCheck::UpToDate
        } else {
            UpdateCheck::UpdateAvailable
        };
    };

    // 先按“最新 release 的版本号”判定：与已装版本不同 → 有更新。
    // 不能先看 commit 相等就跳过：dsh 仓库的 rc 发布可能重打同一 git commit
    // （build-id 不同但 underlying commit 相同），此时 commit 不是可分辨信号，
    // 只有 tag 里的版本号才能正确识别 rc.8 之于 rc.7 是更新。
    if installed != latest_version {
        return UpdateCheck::UpdateAvailable;
    }
    // 版本相同且 commit 一致 → 安装文件即最新 release，无更新。
    if record_commit == Some(latest.commit.as_str()) {
        return UpdateCheck::UpToDate;
    }
    // 文件已经是“最新版本”，此时需要甄别记录是否滞后
    match record_tag.and_then(parse_version_from_tag) {
        Some(record_version) if record_version < latest_version => UpdateCheck::HealUpToDate,
        Some(_) => UpdateCheck::UpdateAvailable,
        None => match legacy_tags
            .iter()
            .find(|(_, commit)| Some(commit.as_str()) == record_commit)
        {
            Some((tag, _)) => match parse_version_from_tag(tag) {
                Some(record_version) if record_version < latest_version => {
                    UpdateCheck::HealUpToDate
                }
                // 反查到的版本与最新版本相同（或解析失败）→ 视为同版本热修
                _ => UpdateCheck::UpdateAvailable,
            },
            // 无法考证记录对应的版本 → 以实际安装文件为准，修正记录
            None => UpdateCheck::HealUpToDate,
        },
    }
}

/// 拉取 pkg 仓库的 release tag 列表（tag, commit），用于反查历史记录对应的版本。
///
/// 仅在更新判定需要反查“无 tag 的老记录”时调用，失败时由调用方回退到
/// “以实际文件为准”的保守分支。
pub async fn fetch_dsh_pkg_tags() -> Result<Vec<(String, String)>, String> {
    let client = github_client()?;
    let tags: serde_json::Value = github_api_get(
        &client,
        &format!("{}/tags?per_page=100", DSH_PKG_GITHUB_API),
    )
    .await
    .map_err(|e| format!("Release tags request failed: {e}"))?
    .json()
    .await
    .map_err(|e| format!("Failed to parse release tags response: {e}"))?;

    Ok(tags
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?.to_string();
            let sha = entry.get("commit")?.get("sha")?.as_str()?.to_string();
            Some((name, sha))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_percent_never_emits_nan_or_inf() {
        // 总长已知：常规百分比
        assert_eq!(download_progress_percent(50, 100), 50.0);
        assert_eq!(download_progress_percent(0, 100), 0.0);
        // 总长未知（0）：返回 -1 而非 inf/NaN（除零保护）
        assert_eq!(download_progress_percent(50, 0), -1.0);
        // 尚未开始且总长未知也不产生 NaN
        assert_eq!(download_progress_percent(0, 0), -1.0);
        // 数值绝对不出现非有限值
        for pct in [download_progress_percent(50, 0), download_progress_percent(0, 0)] {
            assert!(pct.is_finite(), "progress must be finite, got {pct}");
        }
    }

    #[test]
    fn sha256_verification_accepts_only_matching_digest() {
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_sha256(b"abc", expected).is_ok());
        assert!(verify_sha256(b"changed", expected).is_err());
        assert!(verify_sha256(b"abc", "not-a-digest").is_err());
    }

    #[test]
    fn download_sources_are_https_and_allowlisted() {
        assert!(validate_download_url("https://nodejs.org/dist/v22/file.zip").is_ok());
        assert!(validate_download_url("https://registry.npmjs.org/pnpm/-/pnpm.tgz").is_ok());
        assert!(validate_download_url("http://nodejs.org/dist/file.zip").is_err());
        assert!(validate_download_url("https://example.com/file.zip").is_err());
        // 国内镜像源（含 npmmirror 302 后的最终落地域名）
        assert!(validate_download_url("https://npmmirror.com/mirrors/node/v22/file.zip").is_ok());
        assert!(validate_download_url("https://cdn.npmmirror.com/binaries/node/v22/file.zip").is_ok());
        assert!(validate_download_url("https://registry.npmmirror.com/pnpm/-/pnpm.tgz").is_ok());
        assert!(validate_download_url(
            "https://ghfast.top/https://github.com/hairyf/deepseek-harness-pkg/releases/latest/download/deepseek-harness-pkg-windows.zip"
        )
        .is_ok());
    }

    #[tokio::test]
    async fn staged_install_replaces_previous_version_and_cleans_backup() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("dsh-atomic-install-{unique}"));
        let dest = root.join("package");
        let staging = root.join("package.installing");
        let backup = root.join("package.backup");
        fs::create_dir_all(&dest).expect("create previous install");
        fs::create_dir_all(&staging).expect("create staged install");
        fs::write(dest.join("version.txt"), "old").expect("write previous version");
        fs::write(staging.join("version.txt"), "new").expect("write staged version");

        commit_staged_install(&staging, &dest, &backup)
            .await
            .expect("commit staged install");

        assert_eq!(fs::read_to_string(dest.join("version.txt")).unwrap(), "new");
        assert!(!staging.exists());
        assert!(!backup.exists());

        // 模拟上次切换在 dest -> backup 后崩溃，本次应先恢复再安全切换。
        fs::rename(&dest, &backup).expect("simulate interrupted switch");
        fs::create_dir_all(&staging).expect("create next staged install");
        fs::write(staging.join("version.txt"), "next").expect("write next version");
        commit_staged_install(&staging, &dest, &backup)
            .await
            .expect("recover and commit");
        assert_eq!(fs::read_to_string(dest.join("version.txt")).unwrap(), "next");
        assert!(!backup.exists());
        fs::remove_dir_all(root).ok();
    }

    fn latest(tag: &str, commit: &str) -> LatestDshPkg {
        LatestDshPkg {
            tag: tag.to_string(),
            commit: commit.to_string(),
            asset_url: "https://example.invalid/dsh.zip".to_string(),
            digest: Some(format!("sha256:{}", "0".repeat(64))),
        }
    }

    #[test]
    fn commit_fallback_extracts_build_id_from_tag() {
        assert_eq!(
            commit_fallback_from_tag("dsh-0.1.0-rc.8-32331963388"),
            "32331963388"
        );
        // 无 build-id 的 tag 兜底取最后一段，仍为非空稳定标识，避免空 commit 破坏比对
        assert_eq!(commit_fallback_from_tag("dsh-0.2.0"), "0.2.0");
    }

    #[test]
    fn parses_digest_from_expanded_assets_html() {
        // 模拟 expanded_assets 片段：资产文件名之后紧跟作者填写的 sha256:<64hex>。
        // 来自真实 rc.8 页面：windows 资产摘要为 4d541676...
        let html = concat!(
            "…/deepseek-harness-pkg-windows.zip…<span>sha256:4d5416766eb4a66e81b83532abeea64de7e7e2e0bac69a4f0c0508e1d91936c0</span>",
        );
        let got =
            parse_digest_from_expanded_assets(html, "deepseek-harness-pkg-windows.zip");
        assert_eq!(
            got.as_deref(),
            Some("sha256:4d5416766eb4a66e81b83532abeea64de7e7e2e0bac69a4f0c0508e1d91936c0")
        );
        // 文件名不存在 → None
        assert_eq!(
            parse_digest_from_expanded_assets(html, "deepseek-harness-pkg-linux.zip"),
            None
        );
        // 摘要缺失 → None
        assert_eq!(
            parse_digest_from_expanded_assets("<p>no digest here</p>", "deepseek-harness-pkg-windows.zip"),
            None
        );
    }

    #[test]
    fn parse_version_from_tag_formats() {
        assert_eq!(
            parse_version_from_tag("dsh-0.1.0-rc.7-32054485373").as_deref(),
            Some("0.1.0-rc.7")
        );
        assert_eq!(
            parse_version_from_tag("dsh-0.1.0-rc.6-31773193667").as_deref(),
            Some("0.1.0-rc.6")
        );
        assert_eq!(parse_version_from_tag("dsh-0.2.0"), None);
        assert_eq!(parse_version_from_tag("0.1.0-rc.7-abc"), None);
        assert_eq!(parse_version_from_tag(""), None);
    }

    #[test]
    fn resolve_matching_commit_is_up_to_date() {
        let latest = latest(
            "dsh-0.1.0-rc.7-32054485373",
            "6c659bb2636b3ad396a204c4c6ff110276fa3a09",
        );
        let decision = resolve_update(
            Some("6c659bb2636b3ad396a204c4c6ff110276fa3a09"),
            Some("dsh-0.1.0-rc.7-32054485373"),
            Some("0.1.0-rc.7"),
            &latest,
            &[],
        );
        assert_eq!(decision, UpdateCheck::UpToDate);
    }

    #[test]
    fn resolve_different_installed_version_is_update() {
        let latest = latest(
            "dsh-0.1.0-rc.7-32054485373",
            "6c659bb2636b3ad396a204c4c6ff110276fa3a09",
        );
        let decision = resolve_update(
            Some("564019027fd9469991aef6e57bb0a96325491c4e"),
            Some("dsh-0.1.0-rc.6-31773193667"),
            Some("0.1.0-rc.6"),
            &latest,
            &[],
        );
        assert_eq!(decision, UpdateCheck::UpdateAvailable);
    }

    #[test]
    fn resolve_same_version_hotfix_is_update() {
        // 记录正确（与文件一致），最新 release 是同版本热修：应提示更新
        let latest = latest(
            "dsh-0.1.0-rc.6-31773193667",
            "564019027fd9469991aef6e57bb0a96325491c4e",
        );
        let decision = resolve_update(
            Some("995e261e117617780dc50db16c70d445255978fd"),
            Some("dsh-0.1.0-rc.6-31762761461"),
            Some("0.1.0-rc.6"),
            &latest,
            &[],
        );
        assert_eq!(decision, UpdateCheck::UpdateAvailable);
    }

    #[test]
    fn resolve_stale_record_behind_files_heals() {
        // 用户现场：记录停留在 rc.6，文件已是 rc.7 → 修正记录、免打扰
        let latest = latest(
            "dsh-0.1.0-rc.7-32054485373",
            "6c659bb2636b3ad396a204c4c6ff110276fa3a09",
        );
        let decision = resolve_update(
            Some("564019027fd9469991aef6e57bb0a96325491c4e"),
            Some("dsh-0.1.0-rc.6-31773193667"),
            Some("0.1.0-rc.7"),
            &latest,
            &[],
        );
        assert_eq!(decision, UpdateCheck::HealUpToDate);
    }

    #[test]
    fn resolve_legacy_record_without_tag_heals_via_tags_lookup() {
        // 老记录没有 tag：反查 tags 列表发现记录版本低于文件版本 → 修正
        let latest = latest(
            "dsh-0.1.0-rc.7-32054485373",
            "6c659bb2636b3ad396a204c4c6ff110276fa3a09",
        );
        let tags = vec![(
            "dsh-0.1.0-rc.6-31773193667".to_string(),
            "564019027fd9469991aef6e57bb0a96325491c4e".to_string(),
        )];
        let decision = resolve_update(
            Some("564019027fd9469991aef6e57bb0a96325491c4e"),
            None,
            Some("0.1.0-rc.7"),
            &latest,
            &tags,
        );
        assert_eq!(decision, UpdateCheck::HealUpToDate);
    }

    #[test]
    fn resolve_legacy_same_version_still_updates() {
        // 老记录无 tag 但反查为同版本热修：仍应提示
        let latest = latest(
            "dsh-0.1.0-rc.6-31773193667",
            "564019027fd9469991aef6e57bb0a96325491c4e",
        );
        let tags = vec![(
            "dsh-0.1.0-rc.6-31762761461".to_string(),
            "995e261e117617780dc50db16c70d445255978fd".to_string(),
        )];
        let decision = resolve_update(
            Some("995e261e117617780dc50db16c70d445255978fd"),
            None,
            Some("0.1.0-rc.6"),
            &latest,
            &tags,
        );
        assert_eq!(decision, UpdateCheck::UpdateAvailable);
    }

    #[test]
    fn resolve_without_version_metadata_falls_back_to_update() {
        // 最新 tag 无法解析出版本时回退旧行为：记录不一致即提示
        let latest = latest("0.1.0-rc.7", "6c659bb2636b3ad396a204c4c6ff110276fa3a09");
        let decision = resolve_update(
            Some("564019027fd9469991aef6e57bb0a96325491c4e"),
            None,
            Some("0.1.0-rc.7"),
            &latest,
            &[],
        );
        assert_eq!(decision, UpdateCheck::UpdateAvailable);
    }

    #[test]
    fn resolve_rc7_installed_rc8_released_is_update() {
        // 用户现场（已实测真实网络）：已装 rc.7，上游发布 rc.8。仓库 rc 发布会
        // 重打同一 git commit（/commits/{tag} 解析出的 rc.8 commit 与 rc.7 完全相同
        // 6c659bb...），因此 commit 相等不能当作“无更新”——版本号 rc.8 > rc.7
        // 才是有更新。正是这个 commit 相等快路径导致更新提示被吞。
        let latest = latest(
            "dsh-0.1.0-rc.8-32331963388",
            "6c659bb2636b3ad396a204c4c6ff110276fa3a09", // 与已装记录相同 commit
        );
        let decision = resolve_update(
            Some("6c659bb2636b3ad396a204c4c6ff110276fa3a09"), // rc.7 记录
            Some("dsh-0.1.0-rc.7-32054485373"),
            Some("0.1.0-rc.7"),
            &latest,
            &[],
        );
        assert_eq!(decision, UpdateCheck::UpdateAvailable);
    }
}
