//! 主窗口几何持久化：记住窗口大小/位置/最大化状态，重启后恢复。
//!
//! 为什么不直接用官方的 tauri-plugin-window-state：本项目主窗口是程序化创建
//! （见 `desktop::builder::build_main_window`）、自定义壳层标题栏（macOS 上保留原生交通灯），
//! 且关闭时是「隐藏到托盘」而非销毁（见 builder 的 on_window_event），并叠加
//! release 的单例复用（二次启动 show/focus 同一窗口）。插件默认把状态写进独立的
//! 配置文件、恢复时机与这套「隐藏 + 单例」流程存在耦合，且与本项目「所有应用数据
//! 落进 store 文件（.store.dat/.store.dev.dat）」的约定不一致。因此这里基于已有的
//! `tauri-plugin-store` 手动读写，几何记录的时机与恢复流程完全可控。

use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Manager, PhysicalPosition, PhysicalSize, Position, Runtime, Size, WebviewWindow, Window,
};
use tauri_plugin_store::StoreExt;

use super::constants::{STORE_DAT_DEV_FILE, STORE_DAT_FILE, STORE_WINDOW_STATE_KEY};

/// 主窗口默认尺寸（逻辑像素，首次启动/无历史时由 builder 采用）
pub const DEFAULT_WINDOW_WIDTH: f64 = 1280.0;
pub const DEFAULT_WINDOW_HEIGHT: f64 = 840.0;
/// 主窗口最小尺寸（与 build_main_window 的 min_inner_size 对齐）
pub const MIN_WINDOW_WIDTH: f64 = 860.0;
pub const MIN_WINDOW_HEIGHT: f64 = 620.0;

/// 记录一帧主窗口几何。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowState {
    /// 上次是否为最大化状态
    pub maximized: bool,
    /// 非最大化时的物理位置（outer_position，屏幕左上角为原点）
    pub x: Option<i32>,
    pub y: Option<i32>,
    /// 物理尺寸（outer_size）
    pub width: u32,
    pub height: u32,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            maximized: false,
            x: None,
            y: None,
            width: DEFAULT_WINDOW_WIDTH as u32,
            height: DEFAULT_WINDOW_HEIGHT as u32,
        }
    }
}

/// Store 持久化文件名：debug 构建与生产隔离（各自独立文件），语义同
/// `config::setting` 的 store 文件选择，保证开发版与发布版窗口几何不互相污染。
fn store_dat_file_name() -> &'static str {
    if cfg!(debug_assertions) {
        STORE_DAT_DEV_FILE
    } else {
        STORE_DAT_FILE
    }
}

/// 读取上次保存的窗口状态；无记录时返回默认值（首次启动）。
pub fn get_window_state<R: Runtime>(app_handle: &AppHandle<R>) -> WindowState {
    let store = app_handle
        .store(store_dat_file_name())
        .expect("Failed to load store for window state");
    let raw = store.get(STORE_WINDOW_STATE_KEY);
    raw.and_then(|v| {
        v.as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .or_else(|| Some(v.clone()))
    })
    .and_then(|v| serde_json::from_value(v).ok())
    .unwrap_or_default()
}

/// 把窗口状态写回 store 并落盘（store 基于 AppData，不随窗口生命周期丢失）。
fn save_window_state<R: Runtime>(app_handle: &AppHandle<R>, state: &WindowState) {
    let store = app_handle
        .store(store_dat_file_name())
        .expect("Failed to load store for window state");
    let serialized = serde_json::to_value(state).expect("Failed to serialize window state");
    store.set(STORE_WINDOW_STATE_KEY, serialized);
    store.save().expect("Failed to save window state");
}

/// 采样当前窗口并保存（主窗口移动/缩放时由 builder 调用）。
pub fn save_geometry<R: Runtime>(window: &Window<R>) {
    // 原生全屏切换会触发 Resized，但此时的屏幕尺寸不是可恢复的
    // 普通窗口几何；保留进入全屏前的最后一帧，退出全屏后会再次正常采样。
    if window.is_fullscreen().unwrap_or(false) {
        return;
    }

    let pos = window.outer_position().ok();
    let size = window.outer_size().ok();
    let state = WindowState {
        maximized: window.is_maximized().unwrap_or(false),
        x: pos.map(|p| p.x),
        y: pos.map(|p| p.y),
        width: size.map(|s| s.width).unwrap_or(DEFAULT_WINDOW_WIDTH as u32),
        height: size.map(|s| s.height).unwrap_or(DEFAULT_WINDOW_HEIGHT as u32),
    };
    save_window_state(window.app_handle(), &state);
}

/// 把保存的几何解析成「实际可用的矩形」，并夹紧到当前可见屏幕。
///
/// 返回 `(物理尺寸, 物理坐标)`：
/// - 无位置记录（首次启动）→ `None`，调用方保持 builder 的默认尺寸（1280×840，
///   由 Tauri 自动居中）。
/// - 保存的位置完全不可见（例如保存时的外接屏已被拔出）→ 回落到主屏居中，
///   尺寸仍按可见屏幕夹紧，避免窗口被放到屏幕外「找不到」。
fn resolve_geometry<R: Runtime>(
    app: &AppHandle<R>,
    saved: &WindowState,
) -> Option<(PhysicalSize<u32>, PhysicalPosition<i32>)> {
    // 取当前所有监视器，计算可见屏幕的并集矩形
    let monitors = app.available_monitors().ok()?;
    if monitors.is_empty() {
        return None;
    }
    let min_x = monitors.iter().map(|m| m.position().x).min()?;
    let min_y = monitors.iter().map(|m| m.position().y).min()?;
    let max_x = monitors
        .iter()
        .map(|m| m.position().x + m.size().width as i32)
        .max()?;
    let max_y = monitors
        .iter()
        .map(|m| m.position().y + m.size().height as i32)
        .max()?;

    // 尺寸：不小于最小尺寸，且不超过可见屏幕并集（防止拔掉外接大屏后窗口过大）
    let mut w = saved.width.max(MIN_WINDOW_WIDTH as u32);
    let mut h = saved.height.max(MIN_WINDOW_HEIGHT as u32);
    let union_w = (max_x - min_x) as u32;
    let union_h = (max_y - min_y) as u32;
    w = w.min(union_w.max(MIN_WINDOW_WIDTH as u32));
    h = h.min(union_h.max(MIN_WINDOW_HEIGHT as u32));

    let (x, y) = match (saved.x, saved.y) {
        (Some(sx), Some(sy)) => {
            // 窗口矩形是否与可见屏幕并集有交集
            let intersects = sx < max_x && sx + w as i32 > min_x
                && sy < max_y && sy + h as i32 > min_y;
            if intersects {
                // 夹紧坐标到并集内，保证窗口至少部分可见
                let nx = (sx as i64).clamp(min_x as i64, (max_x as i64 - w as i64).max(min_x as i64));
                let ny = (sy as i64).clamp(min_y as i64, (max_y as i64 - h as i64).max(min_y as i64));
                (nx as i32, ny as i32)
            } else {
                // 保存的位置不可见：回落到主屏居中
                match app.primary_monitor().ok().flatten() {
                    Some(m) => (
                        m.position().x + (m.size().width as i32 - w as i32) / 2,
                        m.position().y + (m.size().height as i32 - h as i32) / 2,
                    ),
                    None => return None,
                }
            }
        }
        _ => return None,
    };

    Some((PhysicalSize::new(w, h), PhysicalPosition::new(x, y)))
}

/// 恢复主窗口的大小与位置。在 `build_main_window` 成功 build() 之后调用。
///
/// 内部读取上次保存的 `WindowState`；顺序说明：先设置物理尺寸/位置，再按需
/// `maximize()`——最大化会覆盖窗口当前尺寸，因此尺寸/位置要在最大化之前设置。
/// 无历史状态时不改动窗口，由 builder 的默认 `inner_size(1280, 840)`
/// （逻辑像素，居中）生效。
pub fn restore_main_window<R: Runtime>(app: &AppHandle<R>, window: &WebviewWindow<R>) {
    let saved = get_window_state(app);
    let Some((size, pos)) = resolve_geometry(app, &saved) else {
        return;
    };
    let _ = window.set_size(Size::Physical(size));
    let _ = window.set_position(Position::Physical(pos));
    if saved.maximized {
        let _ = window.maximize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_state_default_matches_builder_default() {
        let state = WindowState::default();
        assert!(!state.maximized);
        assert!(state.x.is_none());
        assert!(state.y.is_none());
        assert_eq!(state.width, DEFAULT_WINDOW_WIDTH as u32);
        assert_eq!(state.height, DEFAULT_WINDOW_HEIGHT as u32);
    }

    #[test]
    fn window_state_serde_roundtrip() {
        let state = WindowState {
            maximized: true,
            x: Some(100),
            y: Some(-200),
            width: 1920,
            height: 1080,
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let parsed: WindowState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.maximized, state.maximized);
        assert_eq!(parsed.x, state.x);
        assert_eq!(parsed.y, state.y);
        assert_eq!(parsed.width, state.width);
        assert_eq!(parsed.height, state.height);
    }

    #[test]
    fn window_state_ignores_missing_fields() {
        // 老版本/缺失字段时也应能反序列化并回落到默认值
        let parsed: WindowState = serde_json::from_str("{}").expect("deserialize");
        assert_eq!(parsed, WindowState::default());
    }
}
