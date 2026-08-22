import { MutationCache, QueryClient } from '@tanstack/react-query'

/**
 * 全局 QueryClient。
 *
 * - `retry: false`：本地 Tauri IPC 连续调用没有"瞬时网络抖动"概念，失败重试
 *   只会在 IPC 层重复排队、无收益，失败应当直接呈现给调用方；
 * - `MutationCache.onError`：兜底打印（各 mutation 已按场景提供 onError 展示，
 *   这里保证任何遗漏的失败都至少留痕，不吞错）。
 */
export const queryClient = new QueryClient({
  defaultOptions: {
    mutations: { retry: false },
    queries: { retry: false },
  },
  mutationCache: new MutationCache({
    onError: (error) => {
      console.error('[QueryClient] unhandled mutation error:', error)
    },
  }),
})
