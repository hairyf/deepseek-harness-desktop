import type { PropsWithOverlays } from '@overlastic/react'
import type { HarnessCore } from '../hooks/use-dsh-cores'
import type { InstallProgress } from '@/store/modules/harness/types'
import { AlertDialog, Button, Spinner } from '@heroui/react'
import { useDisclosure } from '@overlastic/react'
import { listen } from '@tauri-apps/api/event'
import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { If } from 'react-if-lite'
import { PanelProgress } from './panel-progress'

/**
 * 核心版本下载对话框：复用首次安装（setup）的 `install-progress` 事件流，
 * 以进度条 + 日志面板展示下载/解压过程；成功自动关闭，失败展示错误与日志。
 *
 * 用法（overlastic holder）：
 * ```tsx
 * const [dialog, openDownload] = useOverlay(DownloadCoreDialog, { type: 'holder' })
 * await openDownload({ tag, version, runDownload: (tag) => downloadCore(tag) })
 * ```
 * `runDownload` 由调用方注入（通常是 useDshCores 的 downloadCore），对话框负责
 * 监听进度事件并在结束后 resolve/reject。
 */
export interface DownloadCoreDialogProps extends PropsWithOverlays {
  /** 要下载的 release tag（如 `dsh-0.1.0-rc.8-32331963388`） */
  tag: string
  /** 展示用版本号 */
  version: string
  /** 实际下载动作（返回下载后的核心行；成功与否决定对话框如何关闭） */
  runDownload: (tag: string) => Promise<HarnessCore>
}

export function DownloadCoreDialog(props: DownloadCoreDialogProps) {
  const disclosure = useDisclosure({ props, delay: 300 })

  const { t } = useTranslation()
  const [percentage, setPercentage] = useState(0)
  const [logs, setLogs] = useState<string[]>([])
  const [errorMsg, setErrorMsg] = useState<string | null>(null)
  const error = errorMsg != null

  // 对话框打开后：监听 install-progress 事件驱动进度，同时执行下载；
  // 下载成功 → confirm() 关闭并 resolve，失败 → 展示错误 + 关闭按钮。
  useEffect(() => {
    if (!disclosure.visible) {
      return
    }
    let unlisten: (() => void) | undefined
    let cancelled = false
    listen<InstallProgress>('install-progress', (e) => {
      if (cancelled) {
        return
      }
      const payload = e.payload
      // 只前进不后退（下载阶段 0-50，解压阶段 50-100，事件可能乱序到达）
      setPercentage(prev => Math.max(prev, payload.percentage))
      if (payload.log) {
        setLogs(prev => [...prev, payload.log].slice(-5))
      }
    })
      .then((fn) => {
        // 竞态防护：组件已卸载而 listen 才 resolve → 立即注销防泄漏
        if (cancelled)
          fn()
        else unlisten = fn
      })
      .catch(() => {})

    props.runDownload(props.tag)
      .then(() => {
        if (!cancelled) {
          disclosure.confirm()
        }
      })
      .catch((err) => {
        if (!cancelled) {
          setErrorMsg(String(err))
        }
      })

    return () => {
      cancelled = true
      unlisten?.()
    }
    // eslint-disable-next-line react/exhaustive-deps -- 仅打开时执行一次
  }, [disclosure.visible])

  const status = error ? 'danger' : 'default'

  return (
    <AlertDialog onOpenChange={disclosure.cancel} isOpen={disclosure.visible}>
      <AlertDialog.Backdrop>
        <AlertDialog.Container>
          <AlertDialog.Dialog className="sm:max-w-[420px]">
            <If cond={error}>
              <AlertDialog.CloseTrigger />
            </If>
            <AlertDialog.Header>
              <AlertDialog.Icon status={status} />
              <AlertDialog.Heading>
                {error ? t('core.download_failed') : t('core.downloading', { version: props.version })}
              </AlertDialog.Heading>
            </AlertDialog.Header>
            <AlertDialog.Body>
              <If
                cond={!error}
                else={(
                  <div className="flex flex-col gap-3">
                    <p className="break-all font-mono text-xs leading-[1.7] text-danger">{errorMsg}</p>
                    <PanelProgress logs={logs} />
                  </div>
                )}
              >
                <div className="flex flex-col items-start gap-3">
                  <div className="flex items-center gap-2">
                    <Spinner size="sm" color="current" />
                    <span className="text-xs text-muted">{t('core.downloading_hint', { version: props.version })}</span>
                  </div>
                  <PanelProgress percentage={percentage} logs={logs} />
                </div>
              </If>
            </AlertDialog.Body>
            <AlertDialog.Footer>
              <If cond={error}>
                <Button className="rounded-md" variant="tertiary" onPress={disclosure.cancel}>
                  {t('core.download_close')}
                </Button>
              </If>
            </AlertDialog.Footer>
          </AlertDialog.Dialog>
        </AlertDialog.Container>
      </AlertDialog.Backdrop>
    </AlertDialog>
  )
}
