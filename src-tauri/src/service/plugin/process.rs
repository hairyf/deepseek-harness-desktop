//! dsh 子进程执行：启动 `dsh plugin` 进程并等待退出，输出逐行转发为事件。
//!
//! Windows 打包版是 GUI 进程（无控制台），直接以 CREATE_NO_WINDOW 启动会让
//! dsh 派生的子进程各建可见控制台窗口（黑窗闪烁），因此复用
//! `service/workflow/win_spawn` 的隐藏控制台方案并额外跟踪进程句柄以等待退出；
//! Unix 上直接以管道捕获标准输出/错误。

use serde::Serialize;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{Emitter, WebviewWindow};

#[cfg(windows)]
use crate::service::workflow;
#[cfg(not(windows))]
use std::process::{Command, Stdio};

/// 前端监听的控制台事件名（进程输出行）
pub(crate) const PREINSTALL_LOG_EVENT: &str = "preinstall-log";

/// 进程输出行事件载荷
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreinstallLogPayload {
    pub line: String,
}

/// 当前正在运行的 `dsh plugin` 子进程 PID（无进行中安装时为 None）。
///
/// `cancel`（跨平台）用它结束安装进程树；安装结束/失败后必须复位，
/// 防止把「下一个安装」或无关进程误杀。
static ACTIVE_PLUGIN_PID: OnceLock<Mutex<Option<u32>>> = OnceLock::new();

fn active_pid_lock() -> &'static Mutex<Option<u32>> {
    ACTIVE_PLUGIN_PID.get_or_init(|| Mutex::new(None))
}

/// 当前进行中安装的根进程 PID（取消安装用）。
pub(crate) fn active_plugin_pid() -> Option<u32> {
    *active_pid_lock().lock().unwrap_or_else(|e| e.into_inner())
}

/// 记录/清除当前安装进程 PID（guard-drop 模式，作用域结束自动复位）。
struct PidGuard;

impl PidGuard {
    fn set(pid: u32) {
        *active_pid_lock().lock().unwrap_or_else(|e| e.into_inner()) = Some(pid);
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        *active_pid_lock().lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Windows 进程句柄包装：原始句柄是 `*mut c_void`（非 Send），
/// 但 `WaitForSingleObject`/`GetExitCodeProcess` 均为线程安全的系统调用，
/// 包一层以安全地移入 `spawn_blocking` 等待进程退出。
#[cfg(windows)]
struct WaitableHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for WaitableHandle {}

/// 启动 `dsh plugin` 进程并等待结束，返回 `(退出码, 捕获的完整输出)`。
///
/// 输出仍然逐行实时转发为 `preinstall-log` 事件（供前端进度反馈），同时把
/// 全部行追加进共享缓冲区并返回——安装失败时 pnpm 会在错误里印出
/// `allowBuilds:` 允许键（git depPath / 被忽略的构建包名），调用方需要这段
/// 文本去解析并重试。
pub(crate) async fn run_plugin_process(
    node: &Path,
    args: &[OsString],
    cwd: &Path,
    envs: &HashMap<String, String>,
    window: &WebviewWindow,
) -> Result<(i32, String), String> {
    let captured = Arc::new(Mutex::new(String::new()));

    #[cfg(windows)]
    {
        let (stdout, stderr, handle) =
            workflow::win_spawn::spawn_with_hidden_console_tracked(node, args, Some(cwd), envs)
                .map_err(|e| format!("PREINSTALL_SPAWN: {e}"))?;

        spawn_line_emitter(stdout, window.clone(), captured.clone());
        spawn_line_emitter(stderr, window.clone(), captured.clone());

        let handle = WaitableHandle(handle);
        let exit_code = tauri::async_runtime::spawn_blocking(move || {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::{
                GetExitCodeProcess, WaitForSingleObject, INFINITE,
            };
            let handle = handle;
            unsafe {
                let wait = WaitForSingleObject(handle.0, INFINITE);
                let mut code: u32 = 0;
                if GetExitCodeProcess(handle.0, &mut code) == 0 {
                    code = wait;
                }
                CloseHandle(handle.0);
                code as i32
            }
        })
        .await
        .map_err(|e| format!("PREINSTALL_WAIT: {e}"))?;

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        Ok((exit_code, drain_captured(captured)))
    }

    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        let mut child = Command::new(node)
            .args(args)
            .envs(envs)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // 独立进程组：取消时可用 `kill(-pid, ...)` 一次结束整棵安装进程树
            .process_group(0)
            .spawn()
            .map_err(|e| format!("PREINSTALL_SPAWN: {e}"))?;

        let pid = child.id();
        PidGuard::set(pid);
        log::info!("dsh plugin install started, pid {pid}");

        if let Some(stdout) = child.stdout.take() {
            spawn_line_emitter(stdout, window.clone(), captured.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_line_emitter(stderr, window.clone(), captured.clone());
        }

        let exit_code = tauri::async_runtime::spawn_blocking(move || {
            child.wait().map(|s| s.code().unwrap_or(1)).unwrap_or(1)
        })
        .await
        .map_err(|e| format!("PREINSTALL_WAIT: {e}"))?;

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        Ok((exit_code, drain_captured(captured)))
    }
}

/// 取出（并清空）共享缓冲区中的全部捕获输出。
fn drain_captured(captured: Arc<Mutex<String>>) -> String {
    captured
        .lock()
        .map(|mut buf| std::mem::take(&mut *buf))
        .unwrap_or_default()
}

/// 在独立线程中逐行读取进程输出：实时通过 `preinstall-log` 事件转发，
/// 同时追加进共享缓冲区。
/// 使用静态泛型约束 `R: Read + Send + 'static` 避免动态派发（Box<dyn Read>）堆分配。
fn spawn_line_emitter<R: Read + Send + 'static>(
    reader: R,
    window: WebviewWindow,
    captured: Arc<Mutex<String>>,
) {
    std::thread::spawn(move || {
        let buf = BufReader::new(reader);
        for line in buf.lines().map_while(Result::ok) {
            let trimmed = line.trim_end().to_string();
            let _ = window.emit(
                PREINSTALL_LOG_EVENT,
                PreinstallLogPayload {
                    line: trimmed.clone(),
                },
            );
            if let Ok(mut acc) = captured.lock() {
                acc.push_str(&trimmed);
                acc.push('\n');
            }
        }
    });
}