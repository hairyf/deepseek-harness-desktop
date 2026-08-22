#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::sync::{Mutex, OnceLock};

use tauri::{
    ipc::Invoke,
    menu::{Menu, MenuEvent, MenuItem},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
    Emitter, Runtime, WebviewUrl, WebviewWindowBuilder, Wry,
};

#[cfg(target_os = "macos")]
use tauri::{
    menu::{PredefinedMenuItem, Submenu},
    Manager,
};

#[cfg(target_os = "macos")]
static MACOS_FULLSCREEN_MENU_ITEM: OnceLock<Mutex<Option<PredefinedMenuItem<Wry>>>> =
    OnceLock::new();

#[cfg(windows)]
use crate::desktop::window::on_page_load;
use crate::desktop::window::{on_download, on_new_window};
use crate::utils::show_main_window;

/// setup app
pub fn setup(app_handle: tauri::AppHandle) {
    // 启动前清扫上次崩溃残留的孤儿 Harness（端口/PID 双重确认，见
    // workflow::sweep_orphan_harness），避免新实例一路漂移端口
    crate::service::workflow::sweep_orphan_harness(&app_handle);

    // 旧版 AppData data/dsh → 官方 $DSH_HOME（~/.dsh）数据迁移。
    // 必须在 sweep 之后（先杀掉占用文件句柄的残留 dsh 进程）、scheduler/
    // auto_start 之前（迁移完成前不启动 dsh）。失败仅告警不阻断：旧数据
    // 原地保留，下次启动重试。
    if let Err(e) = crate::service::migrate::migrate(&app_handle) {
        log::warn!("dsh home migration deferred (old data kept): {e}");
    }

    // 启动进程监控（tick 检测 dsh 服务状态）
    crate::service::scheduler::start(&app_handle);

    // 开机自启动：已安装且开启 auto_start 时拉起服务
    let app_for_start = app_handle.clone();
    tauri::async_runtime::spawn(async move {
        let setting = crate::config::get_store_dat_setting(&app_for_start);
        if !setting.auto_start {
            log::debug!("auto_start disabled, skipping startup");
            return;
        }
        if let Err(e) = crate::service::workflow::start(app_for_start).await {
            log::error!("start failed: {}", e);
        }
    });

    // 命令行集成自愈：已安装且开启时，确保 shim 与 PATH 注册完整
    // （shim 被删除、PATH 条目丢失等情况下自动重建）
    tauri::async_runtime::spawn(async move {
        let setting = crate::config::get_store_dat_setting(&app_handle);
        if !setting.installed || !setting.cli_link_enabled {
            return;
        }
        if let Err(e) = crate::service::cli::ensure(&app_handle) {
            log::warn!("cli link self-heal failed: {e}");
        }
    });
}

/// setup tray
pub fn tray<R: Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<()> {
    // 使用默认窗口图标
    let icon = app.default_window_icon().unwrap().clone();

    // 构建菜单
    let menu = Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, "open", "打开面板", true, None::<&str>)?,
            &MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?,
        ],
    )?;

    fn handle_menu_event<R: Runtime>(app: &tauri::AppHandle<R>, event: &MenuEvent) {
        match event.id().as_ref() {
            "open" => show_main_window(app),
            "quit" => {
                app.exit(0);
            }
            _ => {}
        }
    }

    fn handle_tray_icon_event<R: Runtime>(tray: &tauri::tray::TrayIcon<R>, event: &TrayIconEvent) {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            ..
        } = event
        {
            show_main_window(tray.app_handle());
        }
    }

    // 构建托盘图标
    let _ = TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("Deepseek Harness Desktop")
        .on_menu_event(move |app, event| handle_menu_event(app, &event))
        .on_tray_icon_event(move |tray, event| handle_tray_icon_event(tray, &event))
        .build(app)?;

    Ok(())
}

/// 安装 macOS 全局菜单栏操作。
///
/// 菜单由原生层在窗口启动前后始终持有，避免 WebView 重载或进入独立全屏 Space
/// 时丢失；点击后只发送动作 id，由前端复用现有对话框和更新流程。
#[cfg(target_os = "macos")]
pub fn install_macos_menu(app: &tauri::AppHandle<Wry>) -> tauri::Result<()> {
    let setting = crate::config::get_store_dat_setting(app);
    crate::config::i18n::set_language(match setting.language.as_str() {
        "en" | "en-US" => crate::config::i18n::Lang::En,
        _ => crate::config::i18n::Lang::Zh,
    });

    let config = MenuItem::with_id(
        app,
        "desktop-config",
        crate::config::i18n::t("menu.settings"),
        true,
        Some("CmdOrCtrl+,"),
    )?;
    let application_separator = PredefinedMenuItem::separator(app)?;
    let is_fullscreen = app
        .get_webview_window("main")
        .and_then(|window| window.is_fullscreen().ok())
        .unwrap_or(false);
    let fullscreen_label = crate::config::i18n::t(if is_fullscreen {
        "menu.exit_fullscreen"
    } else {
        "menu.enter_fullscreen"
    });
    let fullscreen = PredefinedMenuItem::fullscreen(app, Some(&fullscreen_label))?;
    let application_menu = Submenu::with_id_and_items(
        app,
        "desktop-application-menu",
        crate::config::i18n::t("menu.application"),
        true,
        &[&config, &application_separator, &fullscreen],
    )?;

    let hide = PredefinedMenuItem::hide(app, None)?;
    let hide_others = PredefinedMenuItem::hide_others(app, None)?;
    let show_all = PredefinedMenuItem::show_all(app, None)?;
    let quit_separator = PredefinedMenuItem::separator(app)?;
    let quit = PredefinedMenuItem::quit(app, None)?;
    // macOS 会把首个菜单标题强制显示为应用名称；这里只承载必要的系统动作，
    // 真正可见的“应用”功能菜单放在其后，避免再次被系统改名。
    let system_application_menu = Submenu::with_id_and_items(
        app,
        "desktop-system-application-menu",
        app.package_info().name.clone(),
        true,
        &[&hide, &hide_others, &show_all, &quit_separator, &quit],
    )?;

    let run_logs = MenuItem::with_id(
        app,
        "desktop-copy-run-logs",
        crate::config::i18n::t("menu.run_logs"),
        true,
        None::<&str>,
    )?;
    let check_update = MenuItem::with_id(
        app,
        "desktop-check-update",
        crate::config::i18n::t("menu.check_update"),
        true,
        None::<&str>,
    )?;
    let help_separator = PredefinedMenuItem::separator(app)?;
    let about = MenuItem::with_id(
        app,
        "desktop-about",
        crate::config::i18n::t("menu.about"),
        true,
        None::<&str>,
    )?;
    let help_menu = Submenu::with_id_and_items(
        app,
        "desktop-help-menu",
        crate::config::i18n::t("menu.help"),
        true,
        &[&run_logs, &check_update, &help_separator, &about],
    )?;

    let menu = Menu::with_items(
        app,
        &[&system_application_menu, &application_menu, &help_menu],
    )?;
    let _ = app.set_menu(menu)?;
    *MACOS_FULLSCREEN_MENU_ITEM
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(fullscreen);
    Ok(())
}

/// 原生全屏动画会连续触发 Resize；只在状态真正变化时刷新菜单文案。
#[cfg(target_os = "macos")]
fn sync_macos_fullscreen_menu(window: &tauri::Window<Wry>) {
    let Ok(is_fullscreen) = window.is_fullscreen() else {
        return;
    };
    let label = crate::config::i18n::t(if is_fullscreen {
        "menu.exit_fullscreen"
    } else {
        "menu.enter_fullscreen"
    });
    let item = MACOS_FULLSCREEN_MENU_ITEM
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let Some(item) = item else {
        return;
    };
    if item.text().ok().as_deref() == Some(label.as_str()) {
        return;
    }
    if let Err(error) = item.set_text(label) {
        log::warn!("[menu] failed to update macOS fullscreen label: {error}");
    }
}

/// 构建主窗口。
///
/// 主窗口在这里手动创建（不再从 tauri.conf.json 声明）：
/// config 声明的窗口无法挂载 on_download，而内嵌 iframe 的 dsh 页面
/// 触发下载时 WebView2 静默保存、用户零感知，需要接管下载以给出反馈。
pub fn build_main_window(app: &tauri::AppHandle<Wry>) -> tauri::Result<tauri::WebviewWindow<Wry>> {
    let app_handle = app.clone();

    #[cfg(windows)]
    let _notification_handlers_registered = Arc::new(AtomicBool::new(false));
    #[cfg(windows)]
    let notification_handlers_registered_for_page = _notification_handlers_registered.clone();

    let webview_builder =
        WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
            .title("Deepseek Harness Desktop")
            .inner_size(1280.0, 840.0)
            .min_inner_size(860.0, 620.0)
            .resizable(true);

    // macOS 保留原生交通灯：绿色按钮由 AppKit 进入独立 Space 的原生全屏，
    // 同时用 Overlay 让 44px 壳层导航栏继续与窗口 chrome 融合。其他平台
    // 仍由 ShellNavBar 的右侧按钮提供窗口控制。
    #[cfg(target_os = "macos")]
    let webview_builder = webview_builder
        .decorations(true)
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        // Wry 保留了 AppKit 原生按钮的纵向 frame 偏移；24px 在 44px
        // 壳层导航栏内的实测视觉圆心为 22px，而非 API 直觉上的 24px。
        .traffic_light_position(tauri::LogicalPosition::new(14.0, 24.0));

    #[cfg(not(target_os = "macos"))]
    let webview_builder = webview_builder.decorations(false);

    let webview_builder = webview_builder
        // 恢复 iframe 内 HTML5 拖拽（拖入图片/拖动元素）：
        // Tauri 默认注册 wry drag_drop_handler → WebView2 SetAllowExternalDrop(false)
        // 并注入 IDropTarget 接管拖放，iframe 内拖拽被禁用。
        // 注意不能用 .drag_and_drop(false)：它只设置 tao 窗口层的拖放开关
        // （tauri issue #13761），不影响 webview 层，拖拽依旧失效；
        // disable_drag_drop_handler 才能关掉 wry 的接管（等价于旧配置 dragDropEnabled: false）。
        .disable_drag_drop_handler()
        // 接管内嵌 iframe 的 window.open() / target=_blank 新窗口请求：
        // WebView2 里这类请求走 NewWindowRequested，wry 在没有 handler 时
        // 直接 SetHandled(true) 吞掉（点了没反应）——dshmarket 等预设插件的
        // “源码”按钮在桌面端因此无法跳转（浏览器里正常）。
        // 这里把 http(s) 链接交给系统浏览器打开，其余协议一律拒绝。
        .on_new_window(move |url, features| on_new_window(app_handle.clone(), url, features))
        .on_download(|webview, event| on_download(webview, event));

    #[cfg(windows)]
    let webview_builder = webview_builder.on_page_load(move |webview_window, payload| {
        on_page_load(
            webview_window,
            payload,
            notification_handlers_registered_for_page.clone(),
        )
    });

    // 非 Windows（macOS/Linux）没有 WebView2 的 FrameCreated/ContentLoading 流程，
    // 直接用 Tauri 的 initialization_script_for_all_frames 把兼容桥、通知桥、导航桥
    // 与样式桥注入所有 frame（脚本均带 window.__dsh_*_bridge__ 幂等守卫，重复注入安全）。
    #[cfg(not(windows))]
    let webview_builder = webview_builder
        .initialization_script_for_all_frames(crate::desktop::compat::ABORT_SIGNAL_ANY_SHIM_JS)
        .initialization_script_for_all_frames(crate::desktop::notification::NOTIFICATION_SHIM_JS)
        .initialization_script_for_all_frames(crate::desktop::nav::NAV_SHIM_JS)
        .initialization_script_for_all_frames(crate::desktop::style::IFRAME_STYLES_JS)
        .initialization_script_for_all_frames(crate::desktop::paste::PASTE_SHIM_JS);

    let webview_window = webview_builder.build()?;

    // 恢复上次的窗口大小/位置/最大化状态（无历史时保持 builder 默认的 1280×840，
    // 由 Tauri 自动居中；见 config::window_state）。
    crate::config::restore_main_window(app, &webview_window);

    #[cfg(windows)]
    {
        if !_notification_handlers_registered.swap(true, Ordering::SeqCst) {
            log::info!("[notification] scheduling handler registration from setup");
            let webview_for_dialog = webview_window.clone();
            if let Err(e) = webview_window.with_webview(move |webview| {
                if let Err(e) = crate::desktop::notification::enable_notification_permissions(
                    webview,
                    webview_for_dialog,
                ) {
                    log::warn!("[webview] failed to enable notification permission: {e}");
                }
            }) {
                log::warn!("[webview] failed to schedule notification permission setup: {e}");
            }
        }
    }

    Ok(webview_window)
}

// configure invoke handler
pub fn handler() -> impl Fn(Invoke<Wry>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        crate::bridge::install_dependencies,
        crate::bridge::check_dsh_update,
        crate::bridge::launch_harness,
        crate::bridge::shutdown_harness,
        crate::bridge::restart_harness,
        crate::bridge::get_dsh_status,
        crate::bridge::get_preinstall_plugins,
        crate::bridge::get_preinstall_pending,
        crate::bridge::install_preinstall_plugins,
        crate::bridge::cancel_preinstall_plugins,
        crate::bridge::skip_preinstall_plugins,
        crate::bridge::open_preinstall_repo,
        crate::bridge::get_dsh_plugins,
        crate::bridge::update_dsh_plugin,
        crate::bridge::remove_dsh_plugin,
        crate::bridge::report_plugin_error,
        crate::bridge::detect_plugin_recovery,
        crate::bridge::recover_plugin,
        crate::bridge::get_profiles,
        crate::bridge::create_profile,
        crate::bridge::set_active_profile,
        crate::bridge::remove_profile,
        crate::bridge::get_cores,
        crate::bridge::set_active_core,
        crate::bridge::download_core,
        crate::bridge::remove_core,
        crate::bridge::update_local_core,
        crate::bridge::proxy_health_check,
        crate::bridge::get_runtime_info,
        crate::bridge::runtime_ready,
        crate::bridge::get_app_config,
        crate::bridge::update_app_config,
        crate::bridge::get_cli_link_status,
        crate::bridge::open_in_browser,
        crate::bridge::copy_service_url,
        crate::bridge::reveal_data_dir,
        crate::bridge::reveal_in_folder,
        crate::bridge::open_dir,
        crate::bridge::read_service_logs,
        crate::bridge::read_run_logs,
        crate::bridge::clear_service_logs,
        crate::bridge::set_language,
        crate::bridge::toggle_sidebar,
        crate::bridge::get_dsh_theme,
        crate::bridge::check_desktop_update,
        crate::bridge::download_desktop_update,
        crate::bridge::open_desktop_installer,
        crate::bridge::get_desktop_about,
        crate::bridge::open_external_url,
        crate::bridge::read_clipboard_image,
        crate::desktop::notification::show_native_notification,
        crate::bridge::log_frontend,
    ]
}

// configure tauri builder
pub fn builder() -> tauri::Builder<tauri::Wry> {
    let builder = tauri::Builder::default()
        .setup(|app| {
            let app_handle = app.handle().clone();
            build_main_window(&app_handle)?;
            #[cfg(target_os = "macos")]
            install_macos_menu(&app_handle)?;
            tray(&app_handle)?;
            setup(app_handle.clone());
            Ok(())
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "desktop-config"
            | "desktop-about"
            | "desktop-copy-run-logs"
            | "desktop-check-update" => {
                if let Err(error) = app.emit("macos-menu-action", event.id().as_ref()) {
                    log::warn!("[menu] failed to emit macOS menu action: {error}");
                }
            }
            _ => {}
        })
        // 点击关闭按钮时隐藏到托盘而不是退出程序
        .on_window_event(|window, event| match event {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
            }
            // 移动/缩放主窗口时记录几何，重启后据此恢复（见 config::window_state）
            tauri::WindowEvent::Moved(_) => {
                crate::config::save_geometry(window);
            }
            tauri::WindowEvent::Resized(_) => {
                crate::config::save_geometry(window);
                #[cfg(target_os = "macos")]
                sync_macos_fullscreen_menu(window);
            }
            _ => {}
        });

    // 单例模式：多次双击图标（或重复启动）时不会新开窗口，而是把
    // 已存在的（可能已隐藏到托盘）主窗口调到前台，实现“单例 + 复用后台窗口”。
    // 该回调在首次启动时也会以当前进程的参数触发一次（幂等，仅 show/focus），
    // 之后每次二次启动都会派发到这里，重新展示后台运行的主窗口。
    // 仅在生产环境（release）启用：debug 开发调试时若启用单例，
    // 二次启动的调试进程会被吞掉（例如 tauri dev 多实例调试），
    // 因此开发环境跳过该插件。
    #[cfg(not(debug_assertions))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        crate::utils::show_main_window(app);
    }));

    builder
        // Opener plugin
        .plugin(tauri_plugin_opener::init())
        // Notification plugin（Windows 上以 tauri-winrt-notification 实现点击回调，
        // 注册官方插件保留跨平台回退能力）
        .plugin(tauri_plugin_notification::init())
        // FS plugin
        .plugin(tauri_plugin_fs::init())
        // Simple Store plugin
        .plugin(tauri_plugin_store::Builder::new().build())
        // Clipboard plugin
        .plugin(tauri_plugin_clipboard_manager::init())
}
