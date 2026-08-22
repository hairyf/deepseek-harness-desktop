import type { RefObject } from 'react'
import {
  ArrowLeft,
  ArrowRight,
  LayoutSideContent,
  LayoutSideContentLeft,
  Minus,
  Square,
  Xmark,
} from '@gravity-ui/icons'
import { Button, Chip, Description, Dropdown, Label } from '@heroui/react'
import { useOverlay } from '@overlastic/react'
import { invoke } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { If } from 'react-if-lite'
import { cn } from 'tailwind-variants'
import { useStore } from 'valtio-define'
import { ConfigDialog } from '@/components/config-dialog'
import { DesktopAboutDialog } from '@/components/desktop-about-dialog'
import { DesktopUpdateDialog } from '@/components/desktop-update-dialog'
import { useDshPlugins } from '@/hooks/use-dsh-plugins'
import { useIframeTauri } from '@/hooks/use-iframe-tauri'
import { store } from '@/store'
import { toast } from '@/utils'
import { useMacOSAppMenu } from './use-macos-app-menu'

/**
 * 壳层窗口顶部导航栏（44px，常驻）：
 *
 *   [侧边栏(展开/收起)] [后退] [前进] [  空白拖拽区  ] [最小化][最大化][后台化(X)]
 *
 * - 侧边栏/后退/前进：经 postMessage 操控 iframe 内的 dsh 应用
 *   （`dsh://sidebar:toggle` / `dsh://page:prev` / `dsh://page:next`，
 *   由 dsh-tauri 插件或桌面端注入的导航桥脚本 NAV_SHIM_JS 执行）；
 *   折叠图标与按钮禁用态由 iframe 回报的
 *   `dsh://sidebar:collapsed` / `dsh://page:firsted` / `dsh://page:lasted` 同步。
 *   左侧控件只在「dsh-tauri 插件已启用（已安装）」且存在 iframe 时渲染：
 *   原生桥缺席时控件没有可靠接收方，避免出现点了没反应的死按钮。
 * - 空白拖拽区：Tauri 原生 `data-tauri-drag-region`（顶层文档直接生效），
 *   Windows/Linux 上双击切换最大化，macOS 上交由系统标题栏偏好。
 * - macOS：使用原生交通灯，红键后台化、黄键最小化、绿键进入原生全屏；
 *   普通窗口下导航栏左侧留出交通灯区域，原生全屏时整条导航栏收起。
 * - Windows/Linux：右侧窗口按钮直接调用 Tauri API；
 *   后台化 = 隐藏到托盘（服务保持运行）。
 *
 * 未传入 iframeRef（安装/错误/预装引导页，无 iframe 可操控）时
 * 只渲染窗口控制，不渲染左侧导航控制。
 */

/**
 * dsh-tauri 插件 id：安装后 iframe 内提供 `window.__dsh_tauri_bridge__`
 *  原生导航桥（`useDshPlugins` 实时同步其安装状态，插件增删即时生效）
 */
const TAURI_PLUGIN_ID = 'dsh-tauri'

/** WKWebView 的 macOS UA 稳定包含 Macintosh，用于切换平台原生窗口 chrome。 */
function detectMacOS() {
  return navigator.userAgent.includes('Macintosh')
}

const IS_MACOS = detectMacOS()

/** macOS 原生全屏时收起整条壳层导航栏。 */
function useMacOSFullscreen() {
  const [isFullscreen, setIsFullscreen] = useState(false)

  useEffect(() => {
    if (!IS_MACOS)
      return

    const appWindow = getCurrentWindow()
    let mounted = true
    let unlisten: (() => void) | undefined

    async function syncFullscreen() {
      try {
        const fullscreen = await appWindow.isFullscreen()
        if (mounted)
          setIsFullscreen(fullscreen)
      }
      catch (error) {
        console.error('[Navbar] failed to sync fullscreen state:', error)
      }
    }

    async function setupListener() {
      try {
        await syncFullscreen()
        const stopListening = await appWindow.onResized(() => {
          void syncFullscreen()
        })
        if (mounted)
          unlisten = stopListening
        else
          stopListening()
      }
      catch (error) {
        console.error('[Navbar] failed to listen for fullscreen state:', error)
      }
    }

    void setupListener()
    return () => {
      mounted = false
      unlisten?.()
    }
  }, [])

  return isFullscreen
}

export interface NavbarProps {
  /** 就绪态 iframe；传入时启用左侧导航控制 */
  iframeRef?: RefObject<HTMLIFrameElement | null>
}

export function Navbar({ iframeRef }: NavbarProps) {
  const { t } = useTranslation()
  const isFullscreen = useMacOSFullscreen()
  const { plugins } = useDshPlugins()
  const { sidebarCollapsed, canGoBack, canGoForward, sendNav } = useIframeTauri(iframeRef)
  const { updateInfo } = useStore(store.desktopUpdater)

  const openConfigDialog = useOverlay(ConfigDialog)
  const openAboutDialog = useOverlay(DesktopAboutDialog)
  const openUpdateDialog = useOverlay(DesktopUpdateDialog)
  // 仅当 dsh-tauri 插件启用（已安装）时显示左侧导航控件
  const tauriEnabled = plugins.some(plugin => plugin.id === TAURI_PLUGIN_ID)
  function handleWindowAction(action: 'minimize' | 'maximize' | 'background') {
    const appWindow = getCurrentWindow()
    switch (action) {
      case 'minimize':
        void appWindow.minimize()
        break
      case 'maximize':
        void appWindow.toggleMaximize()
        break
      case 'background':
        // 后台化：隐藏窗口到托盘（与关闭按钮行为一致，服务保持运行）
        void appWindow.hide()
        break
    }
  }

  function handleDragRegionDoubleClick() {
    // macOS 的双击标题栏行为由系统偏好决定，不用网页强制覆盖。
    if (!IS_MACOS)
      void getCurrentWindow().toggleMaximize()
  }

  function handleHelpAction(key: string) {
    if (key === 'check-update')
      void handleCheckUpdate()
    else if (key === 'about')
      void openAboutDialog().catch(() => {})
    else if (key === 'copy-run-logs')
      void copyRunLogs()
  }

  function handleOpenConfig() {
    void openConfigDialog().catch(() => {})
  }

  function handleOpenAbout() {
    void openAboutDialog().catch(() => {})
  }

  /** 「检查更新」：先检查，有更新才弹框；检查失败提示错误而非「已是最新」 */
  async function handleCheckUpdate() {
    try {
      const info = await store.desktopUpdater.check()
      if (info)
        void openUpdateDialog().catch(() => {})
      else
        toast(t('update.up_to_date'), {})
    }
    catch (err) {
      console.warn('[Navbar] check update failed:', err)
      toast(t('update.check_failed'), { variant: 'danger' })
    }
  }

  async function copyRunLogs() {
    try {
      const logs = await invoke<string>('read_run_logs')
      await navigator.clipboard.writeText(logs)
      toast(t('messages.logs_copied'), {})
    }
    catch (err) {
      console.error('[Navbar] failed to copy run logs:', err)
      toast(t('messages.copy_failed'), {})
    }
  }

  useMacOSAppMenu({
    enabled: IS_MACOS,
    openConfig: handleOpenConfig,
    openAbout: handleOpenAbout,
    copyRunLogs: () => { void copyRunLogs() },
    checkUpdate: () => { void handleCheckUpdate() },
  })

  return (
    <div
      className={cn(
        'relative flex h-11 w-full flex-none select-none items-center gap-0.5 border-b border-line bg-panel',
        {
          'hidden': IS_MACOS && isFullscreen,
          'pl-20 pr-1.5': IS_MACOS && !isFullscreen,
          'px-1.5': !IS_MACOS || isFullscreen,
        },
      )}
    >
      <If cond={iframeRef != null && tauriEnabled}>
        <Button
          className="rounded-lg size-7"
          isIconOnly
          size="sm"
          variant="ghost"
          aria-label={t(sidebarCollapsed ? 'nav.sidebar_expand' : 'nav.sidebar_collapse')}
          onPress={() => { sendNav('sidebar:toggle') }}
        >
          <If
            cond={sidebarCollapsed}
            then={<LayoutSideContentLeft />}
            else={<LayoutSideContent />}
          />
        </Button>

        <Button
          className="rounded-lg size-7"
          isIconOnly
          size="sm"
          variant="ghost"
          aria-label={t('nav.back')}
          isDisabled={!canGoBack}
          onPress={() => { sendNav('page:prev') }}
        >
          <ArrowLeft />
        </Button>
        <Button
          className="rounded-lg size-7"
          isIconOnly
          size="sm"
          variant="ghost"
          aria-label={t('nav.forward')}
          isDisabled={!canGoForward}
          onPress={() => { sendNav('page:next') }}
        >
          <ArrowRight />
        </Button>
      </If>
      <If cond={!IS_MACOS || !isFullscreen}>
        <div className="ml-1">
          <Button
            className="rounded-lg h-6 text-xs px-1.5"
            size="sm"
            variant="ghost"
            onPress={handleOpenConfig}
          >
            {t('app.config')}
          </Button>
          <Dropdown>
            <Button
              className="rounded-lg h-6 text-xs px-1.5"
              size="sm"
              variant="ghost"
              aria-label={t('app.expand_sidebar')}
            >
              {t('app.help')}
            </Button>
            <Dropdown.Popover className="rounded-md w-5!">
              <Dropdown.Menu>
                <Dropdown.Item
                  className="rounded-md"
                  id="copy-run-logs"
                  textValue={t('menu.run_logs')}
                  onAction={() => handleHelpAction('copy-run-logs')}
                >
                  <Label>{t('menu.run_logs')}</Label>
                </Dropdown.Item>
                <Dropdown.Item
                  className="rounded-md"
                  id="check-update"
                  textValue={t('menu.check_update')}
                  onAction={() => handleHelpAction('check-update')}
                >
                  <span className="flex w-full items-center justify-between gap-3">
                    <Label>{t('menu.check_update')}</Label>
                    <If cond={updateInfo != null}>
                      <Description>{t('menu.new_version')}</Description>
                    </If>
                  </span>
                </Dropdown.Item>
                <Dropdown.Item
                  className="rounded-md"
                  id="about"
                  textValue={t('menu.about')}
                  onAction={() => handleHelpAction('about')}
                >
                  <Label>{t('menu.about')}</Label>
                </Dropdown.Item>
              </Dropdown.Menu>
            </Dropdown.Popover>
          </Dropdown>
        </div>
      </If>
      <If cond={import.meta.env.DEV}>
        <Chip size="sm" variant="primary" color="warning" className="text-xs text-background ml-1">
          {t('app.dev_env')}
        </Chip>
      </If>

      {/* 拖拽区：Tauri 原生拖拽（仅此元素带 data-tauri-drag-region，按钮不受影响） */}
      <div
        className="min-w-0 flex-1 self-stretch"
        data-tauri-drag-region
        onDoubleClick={handleDragRegionDoubleClick}
      />

      <If cond={!IS_MACOS}>
        <Button
          className="rounded-lg size-7"
          isIconOnly
          size="sm"
          variant="ghost"
          aria-label={t('nav.minimize')}
          onPress={() => { handleWindowAction('minimize') }}
        >
          <Minus />
        </Button>

        <Button
          className="rounded-lg size-7"
          isIconOnly
          size="sm"
          variant="ghost"
          aria-label={t('nav.maximize')}
          onPress={() => { handleWindowAction('maximize') }}
        >
          <Square style={{ width: 14, height: 14 }} />
        </Button>

        <Button
          className="rounded-lg size-7 transition-colors enabled:hover:bg-danger/16 enabled:hover:text-danger"
          isIconOnly
          size="sm"
          variant="ghost"
          aria-label={t('nav.background')}
          onPress={() => { handleWindowAction('background') }}
        >
          <Xmark />
        </Button>
      </If>
    </div>
  )
}
