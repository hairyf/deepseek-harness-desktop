import { useEffect } from 'react'
import { If } from 'react-if-lite'
import { useStore } from 'valtio-define'
import { PluginRecovery } from '../components/plugin-recovery'
import { useDshTheme } from '../hooks/use-dsh-theme'
import { store } from '../store'
import { DesktopUpdater } from './components/desktop-updater'
import { DownloadToast } from './components/download-toast-trigger'
import { HarnessUpdater } from './components/harness-updater'
import { Webview } from './components/webview'
import '../i18n'
/**
 * 应用根布局：只负责首次启动与整体壳层结构。
 * 业务状态与操作方法全部收敛到 valtio-define store，
 * 各子组件自行订阅 store，不再通过 props 透传回调与状态。
 * 弹出层（关于 / 检查更新 / 应用配置 / 插件异常修复）统一由 overlastic 命令式打开，
 * 仅在需要时挂载，不常驻渲染。
 */
export function App() {
  useDshTheme()
  const { status } = useStore(store.harness)
  // 首次挂载自动启动 harness（store 内部对 StrictMode 重复挂载去重）
  useEffect(() => {
    store.harness.startup()
  }, [])

  // 仅开发模式：快捷键预览「插件异常修复界面」，便于快速看到实际 UI（不影响生产构建）。
  //   Ctrl+Shift+1 → 运行期异常对话框（应用仍在运行）
  //   Ctrl+Shift+2 → 启动崩溃全屏恢复页
  useEffect(() => {
    if (!import.meta.env.DEV)
      return
    function onKeyDown(e: KeyboardEvent) {
      if (!e.ctrlKey || !e.shiftKey)
        return
      if (e.code === 'Digit1') {
        e.preventDefault()
        store.harness.setRuntimeRecovery({
          plugins: ['dsh-better-sidebar'],
          reason: 'slot_conflict',
          detail: 'sidebar',
          raw_error: 'Preview: dsh-better-sidebar reported a UI slot conflict.',
        })
      }
      else if (e.code === 'Digit2') {
        e.preventDefault()
        store.harness.fail('Preview: plugin startup failure')
        store.harness.setRuntimeRecovery({
          plugins: ['dsh-better-sidebar'],
          reason: 'duplicate_loader_entry',
          detail: 'dshSidebarApi',
          raw_error: 'Preview: duplicate loader entry id: dshSidebarApi',
        })
      }
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [])

  return (
    <div className="flex h-screen w-screen">
      <Webview />
      <If cond={status === 'ready'}>
        <HarnessUpdater />
      </If>
      <If cond={status === 'ready'}>
        <DownloadToast />
      </If>
      {/* 运行期插件异常：应用仍在运行，弹醒目对话框（启动崩溃走 webview 的全屏恢复页） */}
      <If cond={status === 'ready'}>
        <PluginRecovery />
      </If>
      <DesktopUpdater />
    </div>
  )
}
