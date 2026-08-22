//! 浏览器/文件管理器唤起、日志捕获与健康检测。
//!
//! 对外部系统组件的交互：在系统浏览器打开链接、在文件管理器定位/打开目录与
//! 数据目录、复制服务地址到剪贴板；前端与后端日志的透传/读取/清空；以及通过
//! Rust 代理的服务健康检查与运行时环境诊断信息。

use crate::config;
use crate::logger;
use tauri::AppHandle;
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_opener::OpenerExt;

/// 健康检查（通过 Rust 代理，避免 WebView CORS 问题）
#[tauri::command]
pub async fn proxy_health_check(app_handle: AppHandle) -> Result<String, String> {
    let port = config::get_store_dat_setting(&app_handle).port;
    crate::service::workflow::proxy_health_check(port).await
}

/// 运行时/版本/诊断信息（侧边栏展示）
#[tauri::command]
pub async fn get_runtime_info(app_handle: AppHandle) -> Result<config::RuntimeInfo, String> {
    let port = config::get_store_dat_setting(&app_handle).port;
    Ok(config::runtime_info(&app_handle, port))
}

/// 在系统浏览器中打开 Harness 界面
#[tauri::command]
pub async fn open_in_browser(app_handle: AppHandle) -> Result<(), String> {
    let url = config::get_dsh_service_url(config::get_store_dat_setting(&app_handle).port);
    app_handle
        .opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// 复制 Harness 服务地址到剪贴板
#[tauri::command]
pub async fn copy_service_url(app_handle: AppHandle) -> Result<(), String> {
    let url = config::get_dsh_service_url(config::get_store_dat_setting(&app_handle).port);
    app_handle
        .clipboard()
        .write_text(url)
        .map_err(|e| e.to_string())
}

/// 在系统文件管理器中定位指定文件（Session 日志下载完成后的"在文件夹中显示"）
#[tauri::command]
pub fn reveal_in_folder(app_handle: AppHandle, path: String) -> Result<(), String> {
    // 安全边界：只允许定位允许根目录（下载目录/数据目录/$DSH_HOME）内的文件，
    // 防止第三方插件通过 IPC 驱动宿主打开任意路径。
    if !crate::bridge::guard::is_allowed_path(&app_handle, std::path::Path::new(&path)) {
        return Err(format!("REVEAL_PATH_REJECTED: {path}"));
    }
    tauri_plugin_opener::reveal_item_in_dir(&path)
        .map_err(|e| format!("REVEAL_FAILED: {e}"))
}

/// 在系统文件管理器中打开指定目录（核心版本「打开目录」按钮；目录用 open 而非
/// reveal——reveal 是定位父目录，open 是直接打开该目录本身）。
#[tauri::command]
pub fn open_dir(app_handle: AppHandle, path: String) -> Result<(), String> {
    // 安全边界同 reveal_in_folder：仅允许打开允许根目录内的目录
    if !crate::bridge::guard::is_allowed_path(&app_handle, std::path::Path::new(&path)) {
        return Err(format!("OPEN_DIR_REJECTED: {path}"));
    }
    tauri_plugin_opener::open_path(&path, None::<&str>)
        .map_err(|e| format!("OPEN_DIR_FAILED: {e}"))
}

/// 在系统文件管理器中打开数据目录（官方 $DSH_HOME，即 ~/.dsh）
#[tauri::command]
pub async fn reveal_data_dir(app_handle: AppHandle) -> Result<(), String> {
    let dsh_home = config::get_dsh_data_path(&app_handle);
    // 目录可能尚未创建（全新安装），先建好再打开，避免资源管理器报路径不存在
    std::fs::create_dir_all(&dsh_home).map_err(|e| e.to_string())?;

    if cfg!(windows) {
        std::process::Command::new("explorer")
            .arg(&dsh_home)
            .spawn()
            .map_err(|e| e.to_string())?;
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
            .arg(&dsh_home)
            .spawn()
            .map_err(|e| e.to_string())?;
    } else {
        std::process::Command::new("xdg-open")
            .arg(&dsh_home)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 前端日志透传：前端 `console.*` 劫持经此命令落盘到 `desktop.frontdesk.log`
/// （与持有后端 + `dsh` target 的 `desktop.log` 分离），见 `logger/mod.rs`。
#[tauri::command]
pub fn log_frontend(level: String, target: String, message: String) {
    let lvl = logger::FrontendLevel::from_str(&level);
    logger::log_frontend(lvl, &target, &message);
}

/// 读取 dsh 服务日志
#[tauri::command]
pub async fn read_service_logs(
    app_handle: AppHandle,
    max_bytes: Option<usize>,
) -> Result<String, String> {
    let log_path = config::get_service_log_path(&app_handle);
    if !log_path.exists() {
        return Ok(String::new());
    }

    let content = std::fs::read_to_string(&log_path).map_err(|e| e.to_string())?;
    let max_bytes = max_bytes.unwrap_or(64 * 1024);
    if content.len() <= max_bytes {
        Ok(content)
    } else {
        Ok(content[content.len() - max_bytes..].to_string())
    }
}

/// 清空 dsh 服务日志
#[tauri::command]
pub async fn clear_service_logs(app_handle: AppHandle) -> Result<(), String> {
    let log_path = config::get_service_log_path(&app_handle);
    std::fs::write(&log_path, "").map_err(|e| e.to_string())
}

/// 读取运行日志（DSH 服务日志 + 桌面端 Rust 运行日志），格式化为便于
/// 反馈/报障复制的纯文本块：`### 环境信息`、`### 服务日志`、`### 前台日志`
/// 与 `### 后台日志` 四段。
///
/// 服务日志来自 `logs/dsh-web.log`（debug 构建为 `logs/dsh-web.dev.log`）；
/// 运行日志来自 `logs/desktop.log`（桌面端自身 `logger::init` 每次启动落盘，
/// 见 logger/mod.rs）。前端 `console.*` 已在 logger 的文件层按 `target: "frontend"`
/// 跳过、不会写入 `desktop.log`（见 logger/mod.rs），因此「后台日志」取的是纯后端
/// `log::*`；仅对旧版本已落盘、尚未轮转掉的 `frontend:` 行做一次兜底剔除。据此把
/// 「运行日志」拆成：
/// - `### 前台日志`：取自前端独立文件 `logs/desktop.frontdesk.log`
///   （`logger::init` 单独落盘，见 logger/mod.rs），仅含前端 `console.*`；
/// - `### 后台日志`：取自 `logs/desktop.log`，剔除残余 `target: "frontend"` 行，
///   仅保留后端 `log::*`。
/// 每段取末尾最多 `MAX_LINES` 行（前端日志量大，仅取一半 `FRONTEND_MAX_LINES`），
/// 避免粘贴内容超出 GitHub issue 长度上限。
#[tauri::command]
pub async fn read_run_logs(app_handle: AppHandle) -> Result<String, String> {
    const MAX_LINES: usize = 100;
    // 前端日志量大，复制的行数减半（避免粘贴内容过长）；后端仍取满 MAX_LINES
    const FRONTEND_MAX_LINES: usize = MAX_LINES / 2;

    let base = config::get_base_dir(&app_handle);
    let service = config::get_service_log_path(&app_handle);
    let desktop = base.join("logs").join("desktop.log");
    let frontend = base.join("logs").join("desktop.frontdesk.log");

    let read_tail = |path: &std::path::Path, max_lines: usize| -> String {
        if !path.exists() {
            return String::new();
        }
        let content = std::fs::read_to_string(path).unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(max_lines);
        lines[start..].join("\n")
    };

    // 后端尾行：先剔除 `target: "frontend"` 的行，再取末尾，避免前端日志把后端日志挤没
    let read_backend_tail = |path: &std::path::Path, max_lines: usize| -> String {
        if !path.exists() {
            return String::new();
        }
        let content = std::fs::read_to_string(path).unwrap_or_default();
        let lines: Vec<&str> = content
            .lines()
            .filter(|line| !is_frontend_log_line(line))
            .collect();
        let start = lines.len().saturating_sub(max_lines);
        lines[start..].join("\n")
    };

    // 环境信息：桌面端应用版本、dsh 发行版本、Node 版本与系统平台/架构，便于报障时快速定位环境差异
    let dsh_version = config::get_dsh_version(&app_handle)
        .map(|v| format!("dsh: {v}\n"))
        .unwrap_or_default();
    let env_text = format!(
        "app: {}\n{}node: {}\nos: {} ({})",
        app_handle.package_info().version,
        dsh_version,
        config::get_active_node_version(),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );

    let service_text = read_tail(&service, MAX_LINES);
    let frontend_text = read_tail(&frontend, FRONTEND_MAX_LINES);
    let backend_text = read_backend_tail(&desktop, MAX_LINES);

    Ok(format!(
        "### 环境信息\n\n{}\n\n### 服务日志\n\n```\n{}\n```\n\n### 前台日志\n\n```\n{}\n```\n\n### 后台日志\n\n```\n{}\n```",
        env_text,
        service_text.trim_end(),
        frontend_text.trim_end(),
        backend_text.trim_end()
    ))
}

/// 判断某行是否为前端日志（`target: "frontend"`）。
/// 日志行格式见 logger/mod.rs：`[ts] LEVEL target: message`（时间戳可能含空格）。
/// 前端行的 target 恒为 `frontend`，紧跟 LEVEL 之后；用「LEVEL + frontend:」定位，
/// 避免把消息正文里出现的 "frontend" 误判为前端日志。
fn is_frontend_log_line(line: &str) -> bool {
    const LEVELS: [&str; 5] = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"];
    let trimmed = line.trim_start();
    LEVELS
        .iter()
        .any(|lvl| trimmed.contains(&format!("{lvl} frontend:")))
}

/// 在系统浏览器中打开任意 http(s) 链接（更新说明 / 关于对话框仓库链接等）
#[tauri::command]
pub async fn open_external_url(app_handle: AppHandle, url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("EXTERNAL_URL_INVALID: {url}"));
    }
    app_handle
        .opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::is_frontend_log_line;

    #[test]
    fn frontend_line_detected() {
        // tracing 文件层（desktop.log）与前端独立文件（desktop.frontdesk.log）两种时间戳格式都应命中
        assert!(is_frontend_log_line("2024-06-01 12:00:00.123Z INFO frontend: [tag] message"));
        assert!(is_frontend_log_line("[2024-06-01 12:00:00.123Z] INFO frontend: message"));
        assert!(is_frontend_log_line("2024-06-01 12:00:00.123Z WARN frontend: something"));
        assert!(is_frontend_log_line("2024-06-01 12:00:00.123Z ERROR frontend: boom"));
    }

    #[test]
    fn backend_line_not_detected() {
        // 后端（dsh 等 target）不应误判为前端；消息正文里出现 "frontend" 也不应命中
        assert!(!is_frontend_log_line("2024-06-01 12:00:00.123Z INFO dsh: starting server"));
        assert!(!is_frontend_log_line("[2024-06-01 12:00:00.123Z] INFO dsh: emit to frontend: 3"));
        assert!(!is_frontend_log_line("2024-06-01 12:00:00.123Z DEBUG reqwest: GET /ping"));
    }

    #[test]
    fn frontend_level_padding_and_extra_spaces() {
        // 级别可能带前导空格（`{:>5}` 或 tracing 层多空格），frontend 目标仍应命中
        assert!(is_frontend_log_line("2024-06-01 12:00:00.123Z  INFO frontend: padded"));
    }
}
