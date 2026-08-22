pub mod status;
pub mod utils;
pub(crate) mod win_inspector;
#[cfg(windows)]
pub(crate) mod win_spawn;

use crate::config;
use crate::service::download;
use crate::service::workflow::utils::{is_port_in_use, spawn_output_readers};
use std::collections::HashMap;

#[cfg(windows)]
use std::ffi::OsString;
use std::fs;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use tauri::Manager;

/// 启动守卫：并发调用 `launch` 时只允许一个真正拉起 dsh 进程
static LAUNCH_GUARD: AtomicBool = AtomicBool::new(false);

/// 当前进程内由桌面端创建的 Harness 根进程（PID + Windows 句柄）。
///
/// PID 与句柄装在同一把锁的可选值里：`take()` 一次性成对取出，保证
/// 「PID 清空」与「句柄关闭」之间不存在跨原子竞态（WARN-6）。历史上 PID/句柄
/// 分两个 `Atomic*` 存储，`stop` 读 PID 与监视线程清句柄之间有微窗口可能导致
/// 漏杀或重复 close。
#[derive(Clone, Copy)]
struct OwnedProcess {
    pid: u32,
    /// Windows 进程句柄（原始 HANDLE 转 usize 存储，避免 `*mut c_void` 非 Send）。
    /// 只在 Windows 存在；Unix 无句柄概念。
    #[cfg(windows)]
    handle: usize,
}

fn owned_process_lock() -> &'static Mutex<Option<OwnedProcess>> {
    static OWNED_PROCESS: OnceLock<Mutex<Option<OwnedProcess>>> = OnceLock::new();
    OWNED_PROCESS.get_or_init(|| Mutex::new(None))
}

/// 记录新持有的 Harness 根进程（Unix，启动成功后调用）。
#[cfg(not(windows))]
fn set_owned_process(pid: u32) {
    let mut guard = owned_process_lock().lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(OwnedProcess { pid });
}

/// 若调用方 owns 该进程（Windows 额外存句柄），记录之。
#[cfg(windows)]
fn set_owned_process_with_handle(pid: u32, handle: usize) {
    let mut guard = owned_process_lock().lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(OwnedProcess { pid, handle });
}

/// 原子取出持有的进程（PID+句柄一起）。Whoever takes it is responsible for
/// closing the Windows handle.
fn take_owned_process() -> Option<OwnedProcess> {
    owned_process_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

struct LaunchGuard;

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        LAUNCH_GUARD.store(false, Ordering::SeqCst);
    }
}

/// 从起始端口向上查找第一个空闲端口，绝不结束未知的端口占用进程。
fn find_available_port(start: u16) -> Result<u16, String> {
    let mut port = start;
    loop {
        if !is_port_in_use(port) {
            return Ok(port);
        }
        log::warn!("Port {port} is occupied, trying the next port");
        port = port.checked_add(1).ok_or_else(|| {
            "PORT_EXHAUSTED: no available TCP port after the configured port".to_string()
        })?;
    }
}

/// dsh 版本是否支持 `--no-open` 标志。
///
/// 0.1.0-rc.8 起 `dsh web` 默认在系统浏览器打开 UI（桌面端内嵌 WebView，
/// 不希望每次启动都弹浏览器），并新增 `--no-open` 关闭该行为。更早的 rc
/// 版本没有这个标志，commander 会把未知选项当作错误、导致 web profile
/// 启动失败，因此追加标志前必须按已装版本判定：0.1.0-rc.8 及以上传标志；
/// 更早不传（保持旧行为）。
///
/// 比较用 `semver` 库按完整语义化版本进行：只比 rc 序号会把基础版本更大的
/// 新版本误判为旧版——`0.1.1-rc.1` 的 rc 号（1）虽小于 8，但晚于
/// 0.1.0-rc.8，同样支持 `--no-open`（该误判是浏览器复弹的回归根因）。
/// 版本号非法（无法解析）时保守处理：不追加标志。
fn version_supports_no_open(version: &str) -> bool {
    // 首个支持 `--no-open` 的 dsh 版本（0.1.0-rc.8）
    const NO_OPEN_MIN_VERSION: &str = "0.1.0-rc.8";
    let Ok(min) = semver::Version::parse(NO_OPEN_MIN_VERSION) else {
        return false;
    };
    semver::Version::parse(version)
        .map(|v| v >= min)
        .unwrap_or(false)
}

/// 按当前活动核心的 dsh 版本决定是否追加 `--no-open`（见 [`version_supports_no_open`]）。
///
/// 版本以活动核心为准：本地核心（用户 CLI 安装）与预打包核心各自读自己的
/// 包清单；读不到时保守处理：不追加标志。
fn web_supports_no_open_flag(app_handle: &tauri::AppHandle) -> bool {
    match crate::service::core::active_version(app_handle) {
        Some(version) => version_supports_no_open(&version),
        None => false,
    }
}

/// 只结束本应用当前进程创建并仍持有的 Harness 进程树。
fn terminate_owned_process() {
    // 一次性取出 PID+句柄（成对），杜绝「PID 已清空/句柄未清」的漏杀窗口
    let Some(owned) = take_owned_process() else {
        return;
    };

    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        const WAIT_TIMEOUT_CODE: u32 = 0x0000_0102;
        let handle = owned.handle as windows_sys::Win32::Foundation::HANDLE;
        if handle.is_null() {
            return;
        }
        // 真实句柄已结束说明 PID 可能已复用，此时绝不调用 taskkill。
        if unsafe { WaitForSingleObject(handle, 0) } != WAIT_TIMEOUT_CODE {
            unsafe { CloseHandle(handle) };
            return;
        }
        kill_pid_tree(owned.pid);
        unsafe {
            WaitForSingleObject(handle, 5_000);
            CloseHandle(handle);
        }
    }

    #[cfg(unix)]
    {
        kill_pid_tree(owned.pid);
    }
}

/// 结束进程树（Windows `taskkill /PID <pid> /T /F`；Unix 负 PID 进程组，与
/// 启动时 `process_group(0)` 对应）。调用方需先确认 PID 确实指向目标进程。
fn kill_pid_tree(pid: u32) {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        if let Err(e) = cmd.output() {
            log::error!("Failed to stop Harness process tree {pid}: {e}");
        }
    }

    #[cfg(unix)]
    {
        // Harness 根进程启动在独立进程组中，负 PID 只作用于该进程树。
        let group = format!("-{pid}");
        let _ = Command::new("kill").args(["-TERM", "--", &group]).output();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = Command::new("kill").args(["-KILL", "--", &group]).output();
    }
}

pub fn has_owned_process() -> bool {
    owned_process_lock()
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false)
}

/// 处理「持有的 dsh 进程退出」这一事实（由退出监视线程与健康检查 tick 共用）：
///
/// - 清空持有的 PID/句柄（`take` 保证只会被清一次，PID/句柄作为整体成对
///   取出，PID 复用窗口最小化）；
/// - 若当前状态仍是 Running，回落到 Stopped——否则进程已经没了、状态却永远
///   显示「运行中」，前端按钮/横幅会长期处于错误语义（WARN-5）。
///
/// 返回被取出的进程记录（含 Windows 句柄），取到者负责 `CloseHandle`——保证
/// 「取走进程」与「关闭句柄」同属一个调用者，杜绝重复 close。幂等：多次调用
/// （tick 与监视线程并发）只会生效一次，后续调用返回 None。
fn on_owned_process_exit() -> Option<OwnedProcess> {
    let owned = take_owned_process()?;

    log::warn!(
        "Owned Harness process {} exited; resetting status to Stopped",
        owned.pid
    );

    if status::get_status() == status::Status::Running {
        status::set_status(status::Status::Stopped);
    }
    Some(owned)
}

/// 结束所有从本应用 dsh 安装目录启动的 Harness 服务进程（含历史崩溃残留的孤儿实例）。
///
/// 只停本应用当前持有的进程不够：`.harness.pid` 标记只记录最近一次会话的 PID，
/// 应用多次崩溃/强杀（任务管理器结束等）会遗留多个孤儿 dsh 进程、端口一路漂移
/// （3080→3081→…），`sweep_orphan_harness` 每次只能回收最近一个，更早的孤儿
/// 会持续占用 `dependencies/dsh` 目录的文件句柄（node 以该目录为 cwd 且模块
/// DLL 加载在内存），更新切换目录时触发 os error 32（INSTALL_BACKUP_FAILED）。
///
/// 命令行为本应用 dsh 入口路径（`...\dependencies\dsh\node_modules\...\bin.js`）
/// 的 node 进程可判定为本应用的服务实例——路径精确匹配不会误杀用户其它 node
/// 程序，因此可安全地全部结束（taskkill /T /F）。
pub fn terminate_stale_harness_processes(app_handle: &tauri::AppHandle) {
    // 开发（debug）构建不做按路径清扫：生产与开发共用同一个 `dependencies/dsh`
    // 安装目录（核心共用），按命令行路径匹配会把同时运行的 release 服务进程
    // 一并结束——`pnpm tauri dev` 每次后端重编译都会重启应用并触发清扫，导致
    // "release 版 DSH 被 dev 版热更新杀掉"。开发构建自身的崩溃残留仍由
    // `.harness.pid` 标记（位于独立数据目录 `.dsh.dev`，PID+端口双重确认）
    // 精确回收。
    if cfg!(debug_assertions) {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let dsh_bin_path = config::get_dsh_binary_path(app_handle);
        let Some(dsh_bin) = dsh_bin_path.to_str() else {
            return;
        };
        // 进程名过滤保证 PowerShell 自身（其命令行同样包含该路径）不被误杀；
        // 路径中的单引号按 PS 字符串字面量规则转义，避免用户目录含 `'` 时语法错误。
        let escaped = dsh_bin.replace('\'', "''");
        let script = format!(
            "Get-CimInstance Win32_Process -Filter \"Name = 'node.exe'\" | Where-Object {{ $_.CommandLine -like '*{escaped}*' }} | Select-Object -ExpandProperty ProcessId"
        );
        let Ok(output) = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .creation_flags(0x08000000)
            .output()
        else {
            log::error!("Failed to enumerate stale Harness service processes");
            return;
        };
        let mut found = 0;
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Ok(pid) = line.trim().parse::<u32>() else {
                continue;
            };
            found += 1;
            log::warn!("Terminating stale Harness service process {pid} (from dsh install dir)");
            kill_pid_tree(pid);
        }
        if found > 0 {
            // 与 stop() 同理：taskkill 返回后 DLL 句柄的释放还有短暂滞后，
            // 让出一点时间避免紧随其后的目录切换撞上残留锁。
            std::thread::sleep(std::time::Duration::from_millis(800));
        }
    }
    #[cfg(not(windows))]
    {
        // Unix 允许对打开中的文件重命名，孤儿进程不阻塞更新切换，无需处理。
        let _ = app_handle;
    }
}

// ---------------------------------------------------------------------------
// 孤儿 Harness 清扫：崩溃/强杀残留实例的识别与回收（issue #34 关联现象）
// ---------------------------------------------------------------------------

/// 孤儿清扫用的 PID/端口标记文件路径（$DSH_HOME/.harness.pid，两行：PID、端口）。
///
/// 应用被强杀（崩溃、任务管理器结束等）时无法执行退出清理，其 Harness 子进程
/// 会继续占用端口；下一次启动只能一路漂移端口（3080→3081→…）并触发服务端
/// "already running"，表现为应用"坏掉"。启动前据此文件识别并清理这类残留。
fn harness_pid_path(app_handle: &tauri::AppHandle) -> std::path::PathBuf {
    config::get_dsh_data_path(app_handle).join(".harness.pid")
}

/// 记录本次启动的 Harness PID 与端口，供下次启动清扫孤儿用。
fn persist_harness_pid(app_handle: &tauri::AppHandle, pid: u32, port: u16) {
    let path = harness_pid_path(app_handle);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(&path, format!("{pid}\n{port}\n"));
}

/// 启动前清扫上次崩溃残留的孤儿 Harness。端口与 PID 双重确认后才动手：
/// - 标记进程已死 → 仅清理陈旧标记；
/// - 端口占用者正是标记中的 PID → 本应用残留，结束其进程树并清标记；
/// - 其余情况（标记不可解析、端口被其他程序占用、无法探测占用者）一律不动，
///   绝不凭端口猜进程、绝不杀未知进程。
pub fn sweep_orphan_harness(app_handle: &tauri::AppHandle) {
    if has_owned_process() {
        return;
    }
    // 先按命令行路径清扫所有从本应用 dsh 安装目录启动的孤儿 Harness 实例：
    // 标记文件只记录最近一次会话的 PID，应用多次崩溃/强杀会遗留更早的孤儿
    // （端口一路漂移 3081/3082/…），它们持续占用 dependencies/dsh 目录的文件
    // 句柄，导致更新切换目录失败（INSTALL_BACKUP_FAILED, os error 32）。
    // 路径精确匹配不会误杀用户其它 node 程序；标记中的进程若在其中会被一并
    // 结束，随后的 PID/端口双重确认自然落空，仅清理陈旧标记。
    terminate_stale_harness_processes(app_handle);
    let pid_file = harness_pid_path(app_handle);
    let Ok(text) = fs::read_to_string(&pid_file) else { return; };
    let mut lines = text.lines();
    let (Some(pid), Some(port)) = (
        lines.next().and_then(|l| l.trim().parse::<u32>().ok()),
        lines.next().and_then(|l| l.trim().parse::<u16>().ok()),
    ) else {
        // 标记内容不可解析：陈旧垃圾，清掉即可
        let _ = fs::remove_file(&pid_file);
        return;
    };
    if !is_port_in_use(port) {
        // 端口已释放：残留实例早已自行退出，仅清理标记
        let _ = fs::remove_file(&pid_file);
        return;
    }
    if port_owner_pid(port) != Some(pid) {
        // 端口占用者不是我们落盘的进程（或探测不到）：可能是其他程序，不动
        return;
    }
    log::warn!("Sweeping orphaned Harness process {pid} (port {port}) left by a previous session");
    kill_pid_tree(pid);
    let _ = fs::remove_file(&pid_file);
}

/// 占用指定端口的进程 PID（LISTENING 状态）。
/// - Windows：`netstat -ano` 解析；
/// - Unix：`lsof -ti tcp:<port>`，不可用时返回 None。
/// 返回 None 视为"无法确认"，调用方不会因此杀任何进程。
fn port_owner_pid(port: u16) -> Option<u32> {
    #[cfg(windows)]
    {
        let output = Command::new("netstat").arg("-ano").output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let needle = format!(":{port} ");
        for line in text.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 5 || fields[0] != "TCP" {
                continue;
            }
            // 本地地址列（如 127.0.0.1:3080 / [::1]:3080）以 :<port> 结尾
            if !fields[1].ends_with(&needle) {
                continue;
            }
            if fields[3] == "LISTENING" {
                return fields[4].parse().ok();
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        // lsof 在 macOS 默认可用、Linux 常缺失；缺失时跳过清扫（返回 None）
        let output = Command::new("lsof")
            .args(["-ti", &format!("tcp:{port}")])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
    }
}

/// Windows RedirectionGuard（错误码 448 = ERROR_UNTRUSTED_MOUNT_POINT）逃逸重拉的标记路径。
fn relaunch_marker_path(app_handle: &tauri::AppHandle) -> std::path::PathBuf {
    config::get_base_dir(app_handle).join(".dsh-relaunch-448")
}

/// 探测 dsh 入口在当前进程上下文下的打开错误码（None=可打开）。
///
/// 448 只在「进程继承 RedirectionGuard 强制执行」时出现；干净上下文（父进程为
/// explorer 等普通进程）下 Level-1 符号链接可正常穿越。
fn dsh_bin_open_error(app_handle: &tauri::AppHandle) -> Option<i32> {
    std::fs::File::open(config::get_dsh_binary_path(app_handle))
        .err()
        .and_then(|e| e.raw_os_error())
}

/// 通过 explorer 转交启动请求，脱离 RedirectionGuard 强制执行上下文后退出本进程。
///
/// Windows 11 25H2 的 RedirectionGuard 对「非提权进程创建的符号链接/联接点」盖信任章，
/// 而安装器（msiexec/RestartManager 自动重开）会在自身进程启用强制执行并随进程树继承，
/// 导致新实例跨越 pnpm 符号链接链打开 bin.js 时持续报 448——实测与等待时长无关、
/// 永不自行恢复（issue #35）。应用无法在运行时关闭继承的策略，只能脱离被污染的进程树：
/// 把启动请求转交给 explorer（单实例壳进程，干净上下文），由 explorer 创建新实例，
/// 其父进程即 explorer，不再继承强制执行（实测：explorer.exe <exe> 的子进程父进程为
/// explorer.exe，而非转交发起者）。标记文件用于防死循环：若上次重拉未逃逸
/// （explorer 未运行等），本次回退到常规缺失处理（复位 installed 走安装流程）。
#[cfg(windows)]
fn relaunch_via_shell_escape(app_handle: &tauri::AppHandle) {
    let marker = relaunch_marker_path(app_handle);
    if marker.exists() {
        let _ = std::fs::remove_file(&marker);
        log::warn!("RedirectionGuard(448) relaunch did not escape, falling back to normal missing handling");
        return;
    }
    let _ = std::fs::write(&marker, b"1");
    let Ok(exe) = std::env::current_exe() else {
        log::warn!("RedirectionGuard(448) detected but current_exe unavailable, falling back");
        return;
    };
    match std::process::Command::new("explorer.exe").arg(&exe).spawn() {
        Ok(_) => {
            log::warn!(
                "RedirectionGuard(448) detected, relaunching via explorer to escape enforced context: {}",
                exe.display()
            );
            // 短暂让出后退出，避免与新实例产生单实例冲突
            std::thread::sleep(std::time::Duration::from_millis(300));
            std::process::exit(0);
        }
        Err(e) => {
            log::warn!("RedirectionGuard(448) detected but explorer spawn failed ({e}), falling back");
        }
    }
}

/// 检测并启动 Harness 服务
pub async fn start(app_handle: tauri::AppHandle) -> Result<(), String> {
    let setting = config::get_store_dat_setting(&app_handle);
    let node_binary_path = config::get_node_binary_path(&app_handle);
    // 活动核心的入口：本地核心存在时优先本地（需求 3），否则预打包
    let dsh_binary_path = crate::service::core::active_dsh_binary(&app_handle);

    if !setting.installed {
        log::debug!("Harness not installed, skipping startup");
        return Ok(());
    }
    if !node_binary_path.exists() || !dsh_binary_path.exists() {
        // Windows RedirectionGuard(448)：安装器继承的强制执行上下文永不自行恢复，
        // 先尝试通过 explorer 逃逸重拉（见 relaunch_via_shell_escape 注释），
        // 成功则本进程退出；未命中（重拉未逃逸/非 448）才走常规缺失处理。
        #[cfg(windows)]
        if dsh_bin_open_error(&app_handle) == Some(448) {
            relaunch_via_shell_escape(&app_handle);
        }
        let mut setting = config::get_store_dat_setting(&app_handle);
        setting.installed = false;
        config::set_store_dat_setting(&app_handle, setting);
        // 状态变更需要 info 级落盘：这是「store 显示未安装」的源头之一
        // （核心文件短暂缺失被复位），自更新后自动重开走进安装分支多由此触发。
        log::info!("Runtime files missing (node/dsh), resetting installed flag");
        return Ok(());
    }

    if has_owned_process() {
        log::info!("Owned Harness process is already running");
        status::set_status(status::Status::Running);
        status::emit_status(&app_handle);
        return Ok(());
    }

    // 清理 RedirectionGuard(448) 逃逸重拉标记：本进程正常走到启动说明处于干净上下文，
    // 移除标记保证下次自更新后仍能触发逃逸重拉。
    #[cfg(windows)]
    let _ = std::fs::remove_file(relaunch_marker_path(&app_handle));

    log::info!("Starting Harness service");
    status::set_status(status::Status::Starting);
    status::emit_status(&app_handle);
    launch(app_handle).await?;
    // 之后由 scheduler/task/tick_check_dsh_process/mod.rs 检测状态

    Ok(())
}

/// 重启 Harness 服务
pub async fn restart(app_handle: tauri::AppHandle) -> Result<(), String> {
    log::info!("Restarting Harness service");

    // 1. 停止现有服务
    stop(app_handle.clone()).await?;

    // 2. 重新启动
    start(app_handle).await?;

    Ok(())
}

/// 启动 Harness 服务进程
pub async fn launch(app_handle: tauri::AppHandle) -> Result<(), String> {
    let mut setting = config::get_store_dat_setting(&app_handle);
    let node_binary_path = config::get_node_binary_path(&app_handle);
    // 活动核心的 dsh 入口（本地核心优先，未检测到走预打包）
    let dsh_binary_path = crate::service::core::active_dsh_binary(&app_handle);

    log::debug!("Checking Node.js path: {:?}", node_binary_path);
    if !node_binary_path.exists() {
        log::error!("Node.js not installed");
        return Err("NODE_NOT_FOUND: Node.js not installed".to_string());
    }
    log::debug!("Checking Harness path: {:?}", dsh_binary_path);
    if !dsh_binary_path.exists() {
        log::error!("Harness not installed");
        return Err("HARNESS_NOT_FOUND: Harness not installed".to_string());
    }

    // 避免重复启动（配合启动守卫，确保并发调用只拉起一个进程）
    if has_owned_process() {
        log::info!("Owned Harness process is already running, skipping launch");
        return Ok(());
    }
    if LAUNCH_GUARD
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        log::info!("Harness launch already in progress, skipping");
        return Ok(());
    }
    let _launch_guard = LaunchGuard;

    // 端口冲突时从当前值开始逐个递增，并持久化最终选择供所有调用方复用。
    let available_port = find_available_port(setting.port)?;
    if available_port != setting.port {
        log::info!(
            "Harness port changed from {} to {} because the configured port is occupied",
            setting.port,
            available_port
        );
        setting.port = available_port;
        config::set_store_dat_setting(&app_handle, setting.clone());
    }

    // 构造环境变量：隔离的 $DSH_HOME + 隐私默认（关闭遥测）
    let dsh_home = config::get_dsh_data_path(&app_handle);
    fs::create_dir_all(&dsh_home).map_err(|e| format!("create dsh home failed: {e}"))?;

    // Windows 极简模式修复的自愈：插件已装入 profile 时确保 patch 挂载行与
    // minimal-win 用户 preset 落盘（幂等）。最佳努力：失败只告警，不阻断启动。
    if let Err(e) = win_inspector::apply(&app_handle) {
        log::warn!("win32 terminal support apply failed: {e}");
    }
    // 预防性处理：pnpm 在无 TTY 环境（dsh-market 等子进程）下重装/更新插件时，
    // 清理/重建 node_modules 会触发交互确认并因无 TTY 直接中止
    // （ERR_PNPM_ABORTED_REMOVE_MODULES_DIR_NO_TTY），表现为插件更新失败。
    // 启动时确保 profile 的 .npmrc 写入 confirmModulesPurge=false（幂等、保留
    // 已有配置）。最佳努力：失败只告警，不阻断启动。
    if let Err(e) = crate::service::plugin::ensure_profile_npmrc(&app_handle) {
        log::warn!("ensure profile .npmrc failed: {e}");
    }
    let mut envs: HashMap<String, String> = HashMap::new();
    envs.insert(
        "DSH_HOME".to_string(),
        dsh_home.to_string_lossy().into_owned(),
    );
    envs.insert("DSH_TELEMETRY_DISABLED".to_string(), "1".to_string());
    envs.insert("NO_COLOR".to_string(), "1".to_string());
    envs.insert("DSH_WEB_PORT".to_string(), setting.port.to_string());

    // 扩展 PATH，让 dsh 及其子进程能找到 node；Windows 上再注入 Git Bash 的
    // bin 目录：persistent bash（--noprofile --norc）不执行 profile 脚本、PATH
    // 完全继承服务进程，若不含 Git 的 usr/bin，ls/sed/find 等 coreutils 全会
    // `command not found`（MSYS 运行时在部分环境下不会自动补 /usr/bin）。
    if let Some(node_dir) = node_binary_path.parent() {
        if let Some(existing_path) = std::env::var_os("PATH") {
            let git_dirs = win_inspector::git_bash_bin_dirs();
            // 只打印注入的前缀目录，完整 PATH 太长会刷屏
            for dir in &git_dirs {
                log::debug!("harness service PATH prepend: {}", dir.to_string_lossy());
            }
            let mut paths = vec![node_dir.to_path_buf()];
            paths.extend(git_dirs);
            paths.extend(std::env::split_paths(&existing_path));
            if let Ok(new_path) = std::env::join_paths(paths) {
                envs.insert("PATH".to_string(), new_path.to_string_lossy().into_owned());
            }
        }
    }

    // 日志文件（前端日志面板读取）。
    // 每次真实启动前轮转：只保留最近 3 次启动的日志，旧文件后退为
    // `dsh-web.log.1` / `dsh-web.log.2`，避免单文件随多次启动无限增长。
    let log_path = config::get_service_log_path(&app_handle);
    fs::create_dir_all(log_path.parent().unwrap_or(std::path::Path::new(".")))
        .map_err(|e| format!("create log dir failed: {e}"))?;
    utils::rotate_service_log(&log_path, 3);

    // rc.8 起 `dsh web` 默认在系统浏览器打开 UI；桌面端内嵌 WebView，不需要
    // 浏览器，追加 `--no-open` 关闭（老版本无此标志时按版本判定不传）。
    let no_open = web_supports_no_open_flag(&app_handle);

    log::info!("Starting Harness process");

    // Windows 打包版是 GUI 进程（没有控制台）。直接以 CREATE_NO_WINDOW 启动
    // node 会让 dsh 派生的子进程各自新建可见控制台窗口（频繁闪烁 cmd 黑窗），
    // 因此 Windows 上改用“隐藏控制台”方式启动，见 win_spawn 模块。
    let active_profile = crate::service::profile::active_profile(&app_handle);
    let spawn_result = {
        #[cfg(windows)]
        {
            let mut args: Vec<OsString> = vec![
                dsh_binary_path.as_os_str().to_os_string(),
                OsString::from("--profile"),
                OsString::from(active_profile.as_str()),
                OsString::from("--host"),
                OsString::from("127.0.0.1"),
                OsString::from("--port"),
                OsString::from(setting.port.to_string()),
            ];
            if no_open {
                args.push(OsString::from("--no-open"));
            }
            win_spawn::spawn_with_hidden_console_owned(
                &node_binary_path,
                &args,
                Some(&config::get_dsh_install_path(&app_handle)),
                &envs,
            )
            .map(|(stdout, stderr, pid, handle)| {
                // PID 与句柄作为整体一次登记，与退出清理（take 一并取出）配对
                let handle_value = handle as usize;
                set_owned_process_with_handle(pid, handle_value);
                std::thread::spawn(move || unsafe {
                    use windows_sys::Win32::Foundation::CloseHandle;
                    use windows_sys::Win32::System::Threading::{
                        GetExitCodeProcess, WaitForSingleObject, INFINITE,
                    };
                    let process_handle = handle_value as windows_sys::Win32::Foundation::HANDLE;
                    WaitForSingleObject(process_handle, INFINITE);
                    // 记录退出码：启动即崩溃（插件冲突等）时前端据此快速失败，
                    // 退出码也便于诊断问题
                    let mut exit_code: u32 = 0;
                    if GetExitCodeProcess(process_handle, &mut exit_code) != 0 {
                        log::warn!("Owned Harness process {pid} exited with code {exit_code}");
                    } else {
                        log::warn!("Owned Harness process {pid} exited (exit code unavailable)");
                    }
                    // 进程确已退出：清空持有 PID 并把 Status 从 Running 回落为
                    // Stopped（原有实现只清 PID、状态永远停留在 Running）。
                    // take 返回值里的句柄由本线程负责关闭（不会与
                    // terminate_owned_process 重复 close——进程已 exit，
                    // 通常是本线程取走）。
                    let owned = on_owned_process_exit();
                    if let Some(owned) = owned {
                        let h = owned.handle as windows_sys::Win32::Foundation::HANDLE;
                        CloseHandle(h);
                    }
                });
                (Some(stdout), Some(stderr), pid)
            })
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::process::CommandExt;
            let mut cmd = Command::new(&node_binary_path);
            cmd.arg(&dsh_binary_path)
                .arg("--profile")
                .arg(active_profile.as_str())
                .arg("--host")
                .arg("127.0.0.1")
                .arg("--port")
                .arg(&setting.port.to_string());
            if no_open {
                cmd.arg("--no-open");
            }
            cmd.envs(&envs)
                .current_dir(config::get_dsh_install_path(&app_handle))
                // 核心修正：提供一个空的 stdin 防止 setRawMode 报错
                .stdin(Stdio::null())
                // 使用管道捕获输出，以便在子线程中读取
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // 独立进程组让停止操作只影响 Harness 及其后代。
                .process_group(0);
            cmd.spawn().map(|mut child| {
                let pid = child.id();
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                set_owned_process(pid);
                std::thread::spawn(move || {
                    let code = child.wait().ok().and_then(|status| status.code());
                    // 记录退出码：启动即崩溃（插件冲突等）时前端据此快速失败
                    if let Some(code) = code {
                        log::warn!("Owned Harness process {pid} exited with code {code}");
                    } else {
                        log::warn!("Owned Harness process {pid} exited (no exit code)");
                    }
                    // 进程确已退出：清空持有 PID 并把 Status 从 Running 回落为 Stopped
                    // （Unix 无进程句柄可关闭，忽略返回的进程记录）
                    let _ = on_owned_process_exit();
                });
                (stdout, stderr, pid)
            })
        }
    };

    match spawn_result {
        Ok((stdout, stderr, pid)) => {
            log::info!(
                "Harness process started successfully: pid={pid}, port={}",
                setting.port
            );
            // 记录 PID+端口供下次启动清扫崩溃残留的孤儿实例（见 sweep_orphan_harness）
            persist_harness_pid(&app_handle, pid, setting.port);
            spawn_output_readers(stdout, stderr, log_path);
            Ok(())
        }
        Err(e) => {
            log::error!("Failed to start process: {}", e);
            Err(format!("PROCESS_START_FAILED: {e}"))
        }
    }
}

/// 停止 Harness 服务
pub async fn stop(app_handle: tauri::AppHandle) -> Result<(), String> {
    log::info!("Stopping Harness service...");
    // 重置启动守卫，确保后续 launch 可以重新拉起；仅结束持有的根进程树。
    LAUNCH_GUARD.store(false, Ordering::SeqCst);
    terminate_owned_process();
    // 清理孤儿清扫标记：正常停止的实例不应被下次启动当作残留
    let _ = fs::remove_file(harness_pid_path(&app_handle));

    // 给系统一点时间释放端口 (重要！)
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    status::set_status(status::Status::Stopped);
    status::emit_status(&app_handle);
    Ok(())
}

/// 应用退出时同步回收 Harness 进程。
///
/// 退出路径上不更新状态、不做异步等待，只结束当前应用持有的 Harness 进程树。
pub fn stop_on_exit(app_handle: tauri::AppHandle, _port: u16) {
    terminate_owned_process();
    // 正常退出路径同样清理清扫标记（崩溃路径才需要下次启动清扫）
    let _ = fs::remove_file(harness_pid_path(&app_handle));
}

/// 安装环境（Node.js 运行时 + 打包的 Harness 发行版）。
///
/// 返回是否真正落盘更新了 Harness（dsh 任务实际下载并解压）；仅重装
/// Node/pnpm 或全部任务被跳过时返回 false，供调用方决定是否重启页面。
pub async fn install(
    app_handle: &tauri::AppHandle,
    mut dsh_latest: Option<download::LatestDshPkg>,
) -> Result<bool, String> {
    log::info!("Starting installation process");
    // dsh 任务（index==1）实际下载解压时置 true
    let mut dsh_updated = false;

    // 安装前先停止本应用持有的 Harness 服务：运行中的 node 进程会把
    // 原生模块 DLL（如 sharp 的 libvips-42.dll）加载进内存并锁住文件，
    // 不停止的话覆盖解压必然失败（Windows os error 32）。
    // 进程归属以启动时记录的 PID 为准，不根据端口结束未知程序。
    if has_owned_process() {
        log::info!("Stopping running Harness service before installation");
        stop(app_handle.clone()).await?;
    }
    // 只停本应用持有的进程还不够：历史崩溃/强杀残留的孤儿 Harness 实例
    // （不在 .harness.pid 标记中）同样从 dependencies/dsh 启动、占用目录文件
    // 句柄，会导致更新切换目录失败（INSTALL_BACKUP_FAILED, os error 32）。
    // 按命令行路径精确清扫所有本应用 dsh 安装目录启动的进程。
    terminate_stale_harness_processes(app_handle);

    let window = app_handle
        .get_webview_window("main")
        .ok_or("Failed to get main window")?;
    log::debug!("Main window obtained");
    // 3 个任务 × 下载/解压 2 个阶段
    let mut tracker = download::ProgressTracker::new(&window, 6);
    let tasks: Vec<Box<dyn download::Installable>> = vec![
        Box::new(download::Nodejs),
        Box::new(download::Dsh),
        Box::new(download::Pnpm),
    ];
    log::info!("Task list created, {} tasks total", tasks.len());

    for (index, task) in tasks.iter().enumerate() {
        log::debug!("Processing task {}/{}", index + 1, tasks.len());
        // 已安装但版本/commit 与最新 release 不一致时强制重新下载。
        // 版本优先（与 resolve_update 的判定完全一致）：dsh 的 rc 发布会复用
        // 同一 git commit（record_commit 不变），只比 commit 会把 rc.8 之于
        // rc.7 误判为"已最新"而跳过下载——日志表现为"All installation tasks
        // completed"但实际什么都没下载，重启后仍是旧版，且前端丢掉更新提示。
        let outdated = index == 1
            && dsh_latest.as_ref().is_some_and(|info| {
                let installed_version = config::get_dsh_version(app_handle);
                let latest_version = download::parse_version_from_tag(&info.tag);
                // 版本号可解析且不同 → 必须更新；版本不可解析时退回 commit 比对
                let version_differs =
                    match (installed_version.as_deref(), latest_version.as_deref()) {
                        (Some(a), Some(b)) => a != b,
                        _ => false,
                    };
                version_differs
                    || config::get_dsh_pkg_commit(app_handle).as_deref()
                        != Some(info.commit.as_str())
            });
        if task.check_installed(app_handle) && !outdated {
            log::debug!(
                "Task {} already installed and up to date, skipping",
                index + 1
            );
            tracker.skip_phases(2);
            continue;
        }

        log::info!("Task {} not installed, starting installation", index + 1);

        // 1. 下载
        tracker.start_phase(
            "download",
            &format!(
                "{} {}",
                config::i18n::t("install.downloading"),
                task.title()
            ),
        );
        // 下载 URL 对 dsh 也是完全确定可算的（DSH_CORE_URL + 平台文件名），
        // 无需依赖 GitHub API 元数据；api.github.com 限流/被代理拦截时
        // （mac 首次启动常见）仍能拿到真实下载地址，避免整次安装被瞬时失败卡死。
        // dsh 核心默认先走 GitHub 官方直连，失败自动切换 ghfast.top 镜像兜底
        // （下载层会在界面上告知用户）；其余任务保持单一官方源。
        let (urls, name) = if index == 1 {
            let urls = config::get_dsh_download_urls()?;
            let name = urls
                .first()
                .and_then(|u| u.rsplit('/').next())
                .unwrap_or("")
                .to_string();
            (urls, name)
        } else {
            let url = task.get_download_url()?;
            let name = url.rsplit('/').next().unwrap_or("").to_string();
            (vec![url], name)
        };
        // 取文件名用于解压类型判定；下载 URL 正常必含 '/'，但这里不 panic，
        // 防御性兜底为空串（后续 ensure_extract 会因无法判定类型而报错返回，
        // 不再让进程崩溃）。
        log::debug!("Download URL: {}", urls.join(" -> "));
        log::debug!("File name: {}", name);
        let buffer = download::download_file_from_sources(&tracker, urls).await?;
        log::info!("Download completed, file size: {} bytes", buffer.len());
        let expected_digest = match index {
            0 => download::fetch_node_sha256(task.get_download_url()?.as_str()).await?,
            1 => {
                // dsh 的 SHA-256 digest 只能来自 GitHub release asset 元数据
                // （安全设计，见 dsh_INTEGRITY_UNAVAILABLE）。首次安装时该元数据
                // 可能因 api.github.com 限流/网络抖动而缺失（mac 首次启动常见，
                // issue #31），这里带退避重取，避免启动被瞬时失败卡死。
                if dsh_latest.is_none() {
                    for attempt in 0..3 {
                        match download::fetch_latest_dsh_pkg_info().await {
                            Ok(info) => {
                                dsh_latest = Some(info);
                                break;
                            }
                            Err(e) if attempt < 2 => {
                                log::warn!(
                                    "Retrying dsh release metadata fetch ({}/3), will retry: {}",
                                    attempt + 1,
                                    e
                                );
                                tokio::time::sleep(std::time::Duration::from_millis(
                                    500 * (attempt as u64 + 1),
                                ))
                                .await;
                            }
                            Err(e) => {
                                return Err(format!(
                                    "DSH_INTEGRITY_UNAVAILABLE: 无法获取 Harness 发行版的完整性校验信息（{}），请检查网络后重试",
                                    e
                                ));
                            }
                        }
                    }
                }
                dsh_latest
                    .as_ref()
                    .and_then(|info| info.digest.clone())
                    .ok_or_else(|| {
                        "DSH_INTEGRITY_UNAVAILABLE: trusted release digest is required".to_string()
                    })?
            }
            2 => config::PNPM_SHA256.to_string(),
            _ => return Err("INSTALL_TASK_INVALID: unknown install task".to_string()),
        };
        download::verify_sha256(&buffer, &expected_digest)?;
        log::info!("Download integrity verified for task {}", index + 1);
        tracker.end_phase();

        // 2. 解压
        tracker.start_phase(
            "extract",
            &format!("{} {}", config::i18n::t("install.extracting"), task.title()),
        );
        let dest = task.get_install_path(app_handle);
        log::debug!("Installation path: {:?}", dest);
        download::ensure_extract(&tracker, name, buffer, dest).await?;
        log::info!("Extraction completed");
        tracker.end_phase();

        // 记录本次安装对应的 release tag 与 commit，供下次启动比对
        if index == 1 {
            dsh_updated = true;
            if let Some(info) = &dsh_latest {
                config::set_dsh_pkg_commit(app_handle, info.commit.clone());
                config::set_dsh_pkg_tag(app_handle, info.tag.clone());
            }
        }
    }

    log::info!("All installation tasks completed");
    tracker.update(
        100.0,
        config::i18n::t("install.done"),
        "All tasks completed".into(),
    );

    Ok(dsh_updated)
}

/// 健康检查（通过 Rust 代理，避免 WebView CORS 问题）
pub async fn proxy_health_check(port: u16) -> Result<String, String> {
    if !has_owned_process() {
        return Err("HARNESS_NOT_OWNED: no Harness process is owned by this app".to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(config::HEALTH_CHECK_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;

    for endpoint in [
        format!("http://127.0.0.1:{port}/"),
        format!("http://127.0.0.1:{port}/healthz"),
    ] {
        match client.get(&endpoint).send().await {
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                if status.is_success() {
                    return Ok(format!(
                        "healthy - {status} - {}",
                        body.chars().take(80).collect::<String>()
                    ));
                }
            }
            Err(err) => {
                log::debug!("Health check {endpoint}: {err}");
            }
        }
    }
    Err("HARNESS_NOT_READY: Harness service is not ready".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn occupied_port_advances_to_a_free_port() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind occupied test port");
        let occupied = listener.local_addr().expect("read occupied port").port();
        let selected = find_available_port(occupied).expect("find next free port");
        assert!(selected > occupied);
        assert!(!is_port_in_use(selected));
    }

    #[test]
    fn no_open_supported_on_rc8_and_later() {
        assert!(version_supports_no_open("0.1.0-rc.8"));
        assert!(version_supports_no_open("0.1.0-rc.9"));
        // 基础版本更大的新版本：0.1.1-rc.1 的 rc 号（1）虽小于 8，但晚于
        // 0.1.0-rc.8，同样支持 --no-open（只比 rc 号会把这里误判为旧版）
        assert!(version_supports_no_open("0.1.1-rc.1"));
        assert!(version_supports_no_open("0.1.2-rc.1"));
        // 稳定版必然晚于 rc.8
        assert!(version_supports_no_open("0.1.0"));
        assert!(version_supports_no_open("0.2.0"));
        assert!(version_supports_no_open("1.0.0"));
    }

    #[test]
    fn no_open_absent_before_rc8() {
        assert!(!version_supports_no_open("0.1.0-rc.7"));
        assert!(!version_supports_no_open("0.1.0-rc.0"));
        // 基础版本更早的 rc 系列一律不支持
        assert!(!version_supports_no_open("0.0.1-rc.5"));
        assert!(!version_supports_no_open("0.0.9-rc.99"));
    }

    #[test]
    fn no_open_unknown_version_is_conservative() {
        assert!(!version_supports_no_open(""));
        // rc 号缺失：`0.1.0-rc` 的预发布 [rc] 短于 [rc, 8]，判为早于 rc.8
        assert!(!version_supports_no_open("0.1.0-rc"));
        // 不完整/非法版本号（缺 patch、带 v 前缀、无 semver 结构）：无法解析
        assert!(!version_supports_no_open("0.1"));
        assert!(!version_supports_no_open("v0.1.0"));
        assert!(!version_supports_no_open("not-a-version"));
    }
}
