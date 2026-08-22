import type { ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { If } from 'react-if-lite'
import { cn } from 'tailwind-variants'
import { formatLogLine } from '@/utils/log-utils'

/**
 * 日志面板：带边框的「› + 行」日志容器 + 空日志占位。
 * 供 panel-progress 与 preinstall-setup 的日志控制台共用；
 * 顶部可选 header（如复制按钮）。
 */
export interface LogsProps {
  logs: readonly string[]
  /** 最多展示的行数（默认 100） */
  limit?: number
  /** 顶部自定义头（如复制按钮），渲染在边框内 */
  header?: ReactNode
  /** 外层容器样式类（边框/圆角/布局等） */
  className?: string
  /** 滚动区域样式类（高度等，如 max-h-[184px]） */
  bodyClassName?: string
}

export function Logs({ logs, limit = 100, header, className, bodyClassName }: LogsProps) {
  const { t } = useTranslation()
  return (
    <div className={cn('overflow-hidden rounded-lg border border-line bg-log-bg', className)}>
      {header && (
        <div className="flex items-center justify-end border-b border-line/40 bg-panel2/60 px-2 py-1">
          {header}
        </div>
      )}
      <div
        className={cn('min-h-[112px] overflow-y-auto px-3.5 py-2.5 text-left font-mono text-xs leading-[1.7]', bodyClassName)}
        aria-label={t('ui.install_log')}
      >
        <If cond={logs.length > 0} else={<p className="m-0 text-muted">{t('ui.waiting_logs')}</p>}>
          {logs.slice(-limit).map((line, index) => (
            // 日志行内容可能重复，需以 index 区分 key
            // eslint-disable-next-line react/no-array-index-key
            <p key={`${line}-${index}`} className="m-0 flex gap-2 overflow-hidden text-ellipsis whitespace-nowrap text-log-ink">
              <span className="shrink-0 text-accent select-none">›</span>
              <span className="min-w-0 overflow-hidden text-ellipsis">{formatLogLine(line)}</span>
            </p>
          ))}
        </If>
      </div>
    </div>
  )
}
