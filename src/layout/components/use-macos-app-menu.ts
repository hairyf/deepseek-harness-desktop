import { listen } from '@tauri-apps/api/event'
import { useEffect, useRef } from 'react'

interface MacOSAppMenuActions {
  openConfig: () => void
  openAbout: () => void
  copyRunLogs: () => void
  checkUpdate: () => void
}

interface UseMacOSAppMenuOptions extends MacOSAppMenuActions {
  enabled: boolean
}

/**
 * 接收 macOS 原生菜单事件并复用壳层已有操作。
 * 操作经 ref 转发，监听器无需因 React 渲染而反复注册。
 */
export function useMacOSAppMenu({
  enabled,
  openConfig,
  openAbout,
  copyRunLogs,
  checkUpdate,
}: UseMacOSAppMenuOptions) {
  const actionsRef = useRef<MacOSAppMenuActions>({
    openConfig,
    openAbout,
    copyRunLogs,
    checkUpdate,
  })
  actionsRef.current = { openConfig, openAbout, copyRunLogs, checkUpdate }

  useEffect(() => {
    if (!enabled)
      return

    let mounted = true
    let unlisten: (() => void) | undefined

    async function setupListener() {
      try {
        const stopListening = await listen<string>('macos-menu-action', (event) => {
          switch (event.payload) {
            case 'desktop-config':
              actionsRef.current.openConfig()
              break
            case 'desktop-about':
              actionsRef.current.openAbout()
              break
            case 'desktop-copy-run-logs':
              actionsRef.current.copyRunLogs()
              break
            case 'desktop-check-update':
              actionsRef.current.checkUpdate()
              break
          }
        })
        if (mounted)
          unlisten = stopListening
        else
          stopListening()
      }
      catch (error) {
        console.error('[Navbar] failed to listen for macOS application menu:', error)
      }
    }

    void setupListener()
    return () => {
      mounted = false
      unlisten?.()
    }
  }, [enabled])
}
