import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useEffect } from 'react'

/** 核心来源：local = 用户 CLI 安装；app = 桌面端预打包 */
export type CoreSource = 'local' | 'app'

/** Rust 侧 service::core::HarnessCore 的序列化形态（camelCase） */
export interface HarnessCore {
  /** `local` | `app`（无 tag 记录的旧激活行）| `app-<tag>` */
  id: string
  source: CoreSource
  /** 版本号（不含 v 前缀；缺失为空串） */
  version: string
  /** 完整 release tag（如 `dsh-0.1.0-rc.8-32331963388`；local 行为空串） */
  tag: string
  /** 核心入口（cli path）：本地核心为 bin.js，预打包为安装目录 */
  path: string
  /** 「打开目录」入口：本地核心为包目录，预打包为安装/槽位目录；未下载为空 */
  dir: string
  /** 本地是否可用（文件在盘/可解析） */
  present: boolean
  /** 当前是否使用中 */
  active: boolean
  error?: string | null
}

export interface UseDshCoresResult {
  cores: HarnessCore[]
  loading: boolean
  error: string
  /** 切换活动核心（id: local | app | app-<tag>；持久化；服务重启由调用方触发） */
  setActiveCore: (id: string) => Promise<HarnessCore>
  /** 下载指定 tag 的预打包核心到历史槽位（不激活） */
  downloadCore: (tag: string) => Promise<HarnessCore>
  /** 卸载已下载的历史版本（激活中的版本不可卸载） */
  removeCore: (id: string) => Promise<void>
  /** 通过用户包管理器 CLI 更新本地核心，返回更新后的版本号 */
  updateLocalCore: () => Promise<string>
  /** 操作进行中标记 */
  busy: boolean
}

/**
 * 核心列表与操作（react-query）。
 *
 * 查询键 `['cores']`：`set_active_core` / `download_core` / `remove_core` 写
 * 桌面端 store（触发 `setting_updated` 事件），监听该事件一并失效重拉；本地
 * 核心被外部更新后重新打开面板即最新。
 */
export function useDshCores(): UseDshCoresResult {
  const queryClient = useQueryClient()

  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ['cores'],
    queryFn: () => invoke<HarnessCore[]>('get_cores'),
  })

  useEffect(() => {
    let disposed = false
    let unlisten: (() => void) | undefined
    listen('setting_updated', () => {
      void queryClient.invalidateQueries({ queryKey: ['cores'] })
    })
      .then((fn) => {
        // 竞态防护：若组件已卸载而 listen 才 resolve，立即注销防泄漏
        if (disposed)
          fn()
        else unlisten = fn
      })
      .catch(() => {})
    return () => {
      disposed = true
      unlisten?.()
    }
  }, [queryClient])

  const invalidate = () => {
    void queryClient.invalidateQueries({ queryKey: ['cores'] })
  }

  const activate = useMutation({
    mutationFn: (id: string) => invoke<HarnessCore>('set_active_core', { id }),
    onSuccess: invalidate,
  })
  const download = useMutation({
    mutationFn: (tag: string) => invoke<HarnessCore>('download_core', { tag }),
    onSuccess: invalidate,
  })
  const remove = useMutation({
    mutationFn: (id: string) => invoke<void>('remove_core', { id }),
    onSuccess: invalidate,
  })
  const update = useMutation({
    mutationFn: () => invoke<string>('update_local_core'),
    onSuccess: invalidate,
  })

  return {
    cores: data ?? [],
    loading: isLoading,
    error: error ? String(error) : '',
    setActiveCore: async (id) => {
      const activated = await activate.mutateAsync(id)
      await refetch()
      return activated
    },
    downloadCore: async (tag) => {
      const core = await download.mutateAsync(tag)
      await refetch()
      return core
    },
    removeCore: async (id) => {
      await remove.mutateAsync(id)
      await refetch()
    },
    updateLocalCore: async () => {
      const version = await update.mutateAsync()
      await refetch()
      return version
    },
    busy: activate.isPending || download.isPending || remove.isPending || update.isPending,
  }
}
