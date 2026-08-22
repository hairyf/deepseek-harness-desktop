import type { ToastContentValue } from '@heroui/react/toast'
import type { ToastVariants } from '@heroui/styles'
import { ToastQueue } from '@heroui/react'

/** toast() 可选项：库内未暴露的 HeroUIToastOptions（toast-queue 收敛的 content + 超时回调），这里用公开的 ToastContentValue 组合 */
export type ToastOptions = Partial<ToastContentValue & { timeout?: number, onClose?: () => void }> & { placement?: Placement }

export type Placement = NonNullable<ToastVariants['placement']>
export const placements = [
  'top start',
  'top',
  'top end',
  'bottom start',
  'bottom',
  'bottom end',
] as const

export const queues = Object.fromEntries(
  placements.map(p => [p, new ToastQueue({ maxVisibleToasts: 3 })]),
) as Record<Placement, ToastQueue>

const placementsKeys = new Map<string, Placement>()

export function toast(
  message: string,
  options?: ToastOptions,
) {
  // 默认右下角；个别调用方需要其他位置时显式传 placement
  const { placement = 'bottom end', ...rest } = options || {}
  const key = queues[placement].add({ title: message, ...rest })
  placementsKeys.set(key, placement)
  return key
}

function close(key: string) {
  const placement = placementsKeys.get(key)
  placementsKeys.delete(key)
  if (placement)
    queues[placement].close(key)
}

function clear() {
  placementsKeys.clear()
  placements.forEach(p => queues[p].clear())
}

toast.close = close
toast.clear = clear
