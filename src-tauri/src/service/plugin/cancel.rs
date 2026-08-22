//! 取消正在进行的预装插件安装。
//!
//! 跨平台结束由本应用拉起的 `dsh plugin` 安装进程树：
//! - Unix（macOS/Linux）：进程以独立进程组启动（`process_group(0)`，见
//!   `process.rs`），此处对注册的 PID 发 `kill -<pid> SIGTERM` 一次结束整组
//!   （含 pnpm / git 等子进程）；随后向前端推送 `preinstall-cancelled` 事件；
//! - Windows：按命令行特征（`plugin --profile <档案> add`）查找由本应用安装
//!   目录下 node 拉起的进程树并强制结束（`taskkill /T /F`）。

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

#[cfg(windows)]
use std::process::{Command, Stdio};

#[cfg(windows)]
use crate::config;

/// 前端监听“安装已取消”事件名
const PREINSTALL_CANCEL_EVENT: &str = "preinstall-cancelled";

/// 取消事件载荷（预留扩展字段）
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreinstallCancelPayload {}

/// 取消正在进行的预装插件安装
pub async fn cancel(app_handle: &AppHandle) {
    // Unix：结束注册 PID 对应的安装进程组；Windows：按命令行特征 taskkill。
    #[cfg(not(windows))]
    {
        if let Some(pid) = super::process::active_plugin_pid() {
            log::info!("Cancelling dsh plugin install process group led by pid {pid}");
            // 负 PID 语义：对整个进程组发 SIGTERM（子进程 pnpm/git 一并结束）。
            // 注意 `--` 之后的负数才被 kill 当进程组号，直接 `kill -<pid>` 会被
            // 误解析为信号编号。
            let ok = std::process::Command::new("kill")
                .args(["-TERM", "--", &format!("-{pid}")])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                // 兜底：kill 命令不可用/失败时，至少结束直接子进程
                let _ = std::process::Command::new("pkill")
                    .arg("-TERM")
                    .arg("-P")
                    .arg(pid.to_string())
                    .status();
            }
            log::warn!("preinstall cancel: kill(-{pid}) success={ok}");
        } else {
            log::debug!("preinstall cancel: no active install process");
        }
    }

    #[cfg(windows)]
    {
        let Some(window) = app_handle.get_webview_window("main") else {
            return;
        };

        // 按当前档案匹配命令行：`dsh plugin --profile <当前档案> add`（不再写死 web）
        let profile = crate::service::profile::active_profile(app_handle);
        let base = config::get_dsh_install_path(app_handle)
            .to_string_lossy()
            .replace('\\', "\\\\");
        // PowerShell 单引号转义：`'` → `''`（防断串/注入）
        let profile_escaped = profile.replace('\'', "''");
        let base_escaped = base.replace('\'', "''");
        let ps_cmd = format!(
            "Get-CimInstance Win32_Process -Filter \"Name='node.exe'\" | Where-Object {{ ($_.CommandLine -like '*plugin*--profile*{profile_escaped}*add*') -and ($_.ExecutablePath -like '{base_escaped}\\*') }} | ForEach-Object {{ taskkill /PID $_.ProcessId /T /F 2>$null }}"
        );

        let mut cmd = Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &ps_cmd,
        ]);
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());

        if let Err(e) = cmd.output() {
            log::warn!("failed to run preinstall cancel: {e}");
        }
    }

    if let Some(window) = app_handle.get_webview_window("main") {
        let _ = window.emit(PREINSTALL_CANCEL_EVENT, PreinstallCancelPayload {});
    }
}