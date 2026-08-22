import { Logs } from './logs'

const LOG_LIMIT = 5

/**
 * 安装/下载进度面板：进度条 + 日志面板。
 *
 * 从 Loadable 中拆出，供首次安装（setup）与核心版本下载对话框共用：
 * 两处都要展示 `install-progress` 事件驱动的百分比与日志流。
 */
export interface PanelProgressProps {
  /** 进度百分比（0-100）；不传则不渲染进度条 */
  percentage?: number
  /** 日志行；不传则不渲染日志面板（空数组渲染"等待日志"占位） */
  logs?: readonly string[]
}

export function PanelProgress({ percentage, logs }: PanelProgressProps) {
  const hasLogs = logs != null
  const showPanel = hasLogs || percentage != null

  if (!showPanel) {
    return null
  }

  return (
    <div className="flex w-full flex-col gap-4">
      {percentage != null && (
        <div className="flex items-center gap-3">
          <div className="h-2 flex-1 overflow-hidden rounded-full bg-panel2" role="progressbar" aria-valuenow={Math.round(percentage)}>
            <div className="h-full bg-gradient-to-r from-accent to-accent2 transition-[width] duration-150" style={{ width: `${Math.min(percentage, 100)}%` }} />
          </div>
          <span className="min-w-[44px] text-right text-[13px] font-semibold tabular-nums text-accent2">
            {Math.round(percentage)}
            %
          </span>
        </div>
      )}
      {hasLogs && (
        <Logs logs={logs ?? []} limit={LOG_LIMIT} bodyClassName="max-h-[184px]" />
      )}
    </div>
  )
}
