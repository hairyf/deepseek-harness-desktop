import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useEffect } from 'react'

/** Rust 侧 service::profile::Profile 的序列化形态（camelCase） */
export interface Profile {
  /** 档案 id（$DSH_HOME/profiles/<id> 目录名） */
  id: string
  /** 展示名（manifest name 去 dsh-profile- 前缀，首字母大写） */
  name: string
  /** 是否桌面端内置默认档案（web） */
  default: boolean
  /** 是否当前使用中的档案 */
  active: boolean
}

export interface UseDshProfilesResult {
  profiles: Profile[]
  loading: boolean
  error: string
  /** 新建档案（返回新档案；不自动激活） */
  createProfile: (name: string) => Promise<Profile>
  /** 切换当前使用中的档案（持久化；重启服务后生效） */
  activateProfile: (id: string) => Promise<Profile>
  /** 删除档案（默认/使用中的档案会被后端拒绝） */
  removeProfile: (id: string) => Promise<void>
  /** 操作进行中标记（新建/切换/删除任一） */
  busy: boolean
}

/**
 * 档案列表与操作（react-query）。
 *
 * 查询键 `['profiles']`：`set_active_profile` 会写桌面端 store（触发
 * `setting_updated` 事件），这里监听该事件一并失效重拉，保证与后端设置一致。
 */
export function useDshProfiles(): UseDshProfilesResult {
  const queryClient = useQueryClient()

  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ['profiles'],
    queryFn: () => invoke<Profile[]>('get_profiles'),
  })

  // 后端设置变更（切换档案等）后刷新档案列表
  useEffect(() => {
    let disposed = false
    let unlisten: (() => void) | undefined
    listen('setting_updated', () => {
      void queryClient.invalidateQueries({ queryKey: ['profiles'] })
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
    void queryClient.invalidateQueries({ queryKey: ['profiles'] })
  }

  const create = useMutation({
    mutationFn: (name: string) => invoke<Profile>('create_profile', { name }),
    onSuccess: invalidate,
  })
  const activate = useMutation({
    mutationFn: (id: string) => invoke<Profile>('set_active_profile', { id }),
    onSuccess: invalidate,
  })
  const remove = useMutation({
    mutationFn: (id: string) => invoke<void>('remove_profile', { id }),
    onSuccess: invalidate,
  })

  return {
    profiles: data ?? [],
    loading: isLoading,
    error: error ? String(error) : '',
    createProfile: async (name) => {
      const created = await create.mutateAsync(name)
      await refetch()
      return created
    },
    activateProfile: async (id) => {
      const activated = await activate.mutateAsync(id)
      await refetch()
      return activated
    },
    removeProfile: async (id) => {
      await remove.mutateAsync(id)
      await refetch()
    },
    busy: create.isPending || activate.isPending || remove.isPending,
  }
}
