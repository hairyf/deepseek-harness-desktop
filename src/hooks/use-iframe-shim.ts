import type { RefObject } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { useEffect } from 'react'
import { useEvent, useInterval, useMountedState } from 'react-use'
import { queryClient } from '@/config/client'
import { getIframeOrigin } from '@/utils/iframe-origin'

interface NativeNotificationMessage {
  source?: 'dsh-notification-bridge'
  type?: string
  title?: string
  body?: string
  tag?: string
  sessionId?: string
  requireInteraction?: boolean
}

/** iframe 插件异常上报消息（dsh-tauri 桥 / 页面脚本 postMessage 通道） */
interface PluginErrorMessage {
  source?: 'dsh-plugin-error-bridge'
  type?: string
  /** 插件 id（npm 包名） */
  id?: string
  /** 异常消息 */
  error?: string
  /** 记录动作：runtime / install / update（默认 runtime） */
  action?: string
}

/**
 * iframe 剪贴板图片回退请求（desktop/paste::PASTE_SHIM_JS 发来）。
 * Linux/WebKitGTK 下 dsh iframe 的 paste 事件拿不到图片，宿主侧据此调用
 * `read_clipboard_image` 读系统剪贴板，再把 PNG data URL 回传给 iframe 重新贴图。
 */
interface ClipboardImageRequest {
  source?: 'dsh-clipboard-image-bridge'
  type?: 'dsh://clipboard-image:read'
  id?: string
}

export function useIframeShim(iframeRef: RefObject<HTMLIFrameElement | null>) {
  const isMounted = useMountedState()

  // 接收 iframe 的「原生通知」请求，转发给 Tauri 命令弹出系统通知
  function handleMessage(event: MessageEvent<NativeNotificationMessage>) {
    const data = event.data
    if (!data || typeof data !== 'object' || data.source !== 'dsh-notification-bridge') {
      return
    }
    // 只接受 DSH 直接 iframe 发来的消息；不兼容多层嵌套 iframe。
    if (event.source !== iframeRef.current?.contentWindow) {
      return
    }
    const iframeOrigin = getIframeOrigin(iframeRef)
    if (!iframeOrigin || event.origin !== iframeOrigin) {
      return
    }
    if (data.type !== 'dsh://native-notification') {
      return
    }
    void invoke('show_native_notification', {
      payload: {
        title: data.title ?? '',
        body: data.body ?? '',
        tag: data.tag ?? null,
        sessionId: data.sessionId ?? null,
        requireInteraction: Boolean(data.requireInteraction),
      },
    }).catch(error => console.error('[notification] show_native_notification failed:', error))
  }

  // 接收 iframe 的「插件异常上报」：记录到后端错误注册表并刷新插件列表。
  // 内嵌页面（或 dsh-tauri 扩展）捕获插件运行期异常后，以
  // `{ source: 'dsh-plugin-error-bridge', type: 'dsh://plugin-error', id, error }`
  // postMessage 到宿主，宿主经 `report_plugin_error` 持久化并推送新列表，
  // 「插件」面板据此展示 danger 图标与更新/卸载入口。
  function handlePluginError(event: MessageEvent<PluginErrorMessage>) {
    const data = event.data
    if (!data || typeof data !== 'object' || data.source !== 'dsh-plugin-error-bridge') {
      return
    }
    if (event.source !== iframeRef.current?.contentWindow) {
      return
    }
    const iframeOrigin = getIframeOrigin(iframeRef)
    if (!iframeOrigin || event.origin !== iframeOrigin) {
      return
    }
    if (data.type !== 'dsh://plugin-error' || !data.id || !data.error) {
      return
    }
    void invoke('report_plugin_error', {
      id: data.id,
      error: data.error,
      action: data.action ?? 'runtime',
    })
      .then(() => {
        void queryClient.invalidateQueries({ queryKey: ['plugins'] })
      })
      .catch(error => console.error('[plugin-error] report_plugin_error failed:', error))
  }

  // 接收 iframe 的「剪贴板图片回退」请求：读取系统剪贴板图片并把 PNG data URL 回传，
  // 使 Linux/WebKitGTK 下 dsh iframe 的贴图（paste 事件拿不到图片）能走原生剪贴板通路。
  function handleClipboardImage(event: MessageEvent<ClipboardImageRequest>) {
    const data = event.data
    if (!data || typeof data !== 'object' || data.source !== 'dsh-clipboard-image-bridge') {
      return
    }
    // 只接受 DSH 直接 iframe 发来的消息；不兼容多层嵌套 iframe。
    if (event.source !== iframeRef.current?.contentWindow) {
      return
    }
    const iframeOrigin = getIframeOrigin(iframeRef)
    if (!iframeOrigin || event.origin !== iframeOrigin) {
      return
    }
    if (data.type !== 'dsh://clipboard-image:read' || !data.id) {
      return
    }
    // 在闭包外把收窄后的值固定到局部常量，避免 TS 在闭包内丢失控制流收窄
    const reqId = data.id
    const origin = iframeOrigin
    function reply(dataUrl: string | null) {
      iframeRef.current?.contentWindow?.postMessage(
        { source: 'dsh-desktop-clipboard', id: reqId, data_url: dataUrl },
        origin,
      )
    }
    void invoke<{ data_url?: string } | null>('read_clipboard_image')
      .then(result => reply(result?.data_url ?? null))
      .catch((error) => {
        console.error('[clipboard-image] read_clipboard_image failed:', error)
        reply(null)
      })
  }

  useEvent('message', handleMessage)
  useEvent('message', handlePluginError)
  useEvent('message', handleClipboardImage)

  // 系统通知点击 → 通知 iframe 聚焦对应会话
  useEffect(() => {
    let unlisten: (() => void) | undefined
    void listen<{ sessionId?: string | null, title?: string, tag?: string }>(
      'dsh-notification-clicked',
      (event) => {
        const payload = event.payload
        const iframeOrigin = getIframeOrigin(iframeRef)
        if (!iframeOrigin)
          return
        iframeRef.current?.contentWindow?.postMessage(
          {
            type: 'dsh://focus-session',
            sessionId: payload.sessionId || undefined,
            title: payload.title || undefined,
            tag: payload.tag || undefined,
          },
          iframeOrigin,
        )
      },
    ).then((unlistener) => {
      // 订阅完成前若已卸载则立即释放，避免回调泄漏
      if (isMounted()) {
        unlisten = unlistener
      }
      else {
        unlistener()
      }
    })
    return () => {
      unlisten?.()
    }
  // eslint-disable-next-line react/exhaustive-deps
  }, [])

  // 将窗口可见性（最小化/隐藏/失焦）同步给 iframe，便于其暂停渲染
  function syncVisibility() {
    void (async () => {
      try {
        const appWindow = getCurrentWindow()
        const [minimized, visible] = await Promise.all([
          appWindow.isMinimized(),
          appWindow.isVisible(),
        ])
        const hidden = minimized || !visible
        const iframeOrigin = getIframeOrigin(iframeRef)
        if (!iframeOrigin)
          return
        iframeRef.current?.contentWindow?.postMessage(
          { type: 'dsh://visibility-state', hidden },
          iframeOrigin,
        )
      }
      catch (error) {
        console.error('[notification] sync visibility failed:', error)
      }
    })()
  }

  // 窗口焦点/尺寸变化时即时同步可见性
  useEffect(() => {
    let unlisteners: Array<() => void> = []
    void (async () => {
      try {
        const appWindow = getCurrentWindow()
        syncVisibility()
        const unFocus = await appWindow.onFocusChanged(() => {
          void syncVisibility()
        })
        const unResized = await appWindow.onResized(() => {
          void syncVisibility()
        })
        if (isMounted()) {
          unlisteners = [unFocus, unResized]
        }
        else {
          unFocus()
          unResized()
        }
      }
      catch (error) {
        console.error('[notification] visibility listeners failed:', error)
      }
    })()
    return () => {
      unlisteners.forEach(fn => fn())
    }
  // eslint-disable-next-line react/exhaustive-deps
  }, [])

  // 兜底轮询：覆盖监听不到的状态变化（如任务栏切换）；主路径是文档可见性事件，5s 足够
  useInterval(syncVisibility, 5000)
}
