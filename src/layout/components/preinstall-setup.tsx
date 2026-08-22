import type { PreinstallPlugin } from '@/store/modules/harness'
import { CircleInfo, Copy, Xmark } from '@gravity-ui/icons'
import { Button, Checkbox, Chip, Label, Spinner, Typography } from '@heroui/react'
import { invoke } from '@tauri-apps/api/core'
import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { If } from 'react-if-lite'
import { useStore } from 'valtio-define'
import { Empty } from '@/components/empty'
import { Item } from '@/components/item'
import { Logs } from '@/components/logs'
import { store } from '@/store'
import { toast } from '@/utils'

/**
 * 预装插件引导页：首次安装（或老版本升级）后展示推荐插件列表，
 * 用户确认后调用 `dsh plugin` 安装（日志实时回流到控制台），
 * 或跳过；两者都会标记完成并继续启动服务。
 */

/** 派生默认勾选集合：未安装的推荐/修复/默认勾选项（用户手动调整前的基础态） */
function defaultSelectedSet(plugins: readonly PreinstallPlugin[]): Set<string> {
  return new Set(plugins.filter(p => !p.installed && (p.recommended || p.fix || p.defaultChecked)).map(p => p.id))
}

/** 插件列表的一行：名称 + 推荐/已安装标签在左，勾选框 + 仓库跳转按钮在右 */
function PluginRow({ plugin, checked, disabled, onToggle, onOpenRepo }: {
  plugin: PreinstallPlugin
  checked: boolean
  disabled: boolean
  onToggle: (id: string, checked: boolean) => void
  onOpenRepo: (id: string) => void
}) {
  const { t } = useTranslation()

  return (
    <Item
      left={(
        <>
          <Label className={`min-w-0 truncate text-sm font-medium ${plugin.installed ? 'text-muted line-through' : 'text-ink'}`}>
            {plugin.name}
          </Label>
          <If cond={plugin.recommended && !plugin.installed}>
            <Chip size="sm" variant="soft" color="success" className="shrink-0 font-medium">
              {t('preinstall.recommend')}
            </Chip>
          </If>
          <If cond={plugin.fix && !plugin.installed}>
            <Chip size="sm" variant="soft" color="warning" className="shrink-0 font-medium">
              {t('preinstall.fix')}
            </Chip>
          </If>
          <If cond={plugin.installed}>
            <Chip size="sm" variant="soft" color="success" className="shrink-0 font-medium">
              {t('preinstall.installed')}
            </Chip>
          </If>
        </>
      )}
      right={(
        <>
          <Checkbox
            isSelected={checked || plugin.installed}
            isDisabled={disabled || plugin.installed}
            onChange={isSelected => onToggle(plugin.id, isSelected)}
            className="shrink-0"
            aria-label={plugin.name}
          >
            <Checkbox.Content>
              <Checkbox.Control>
                <Checkbox.Indicator />
              </Checkbox.Control>
            </Checkbox.Content>
          </Checkbox>
          <Button
            isIconOnly
            size="sm"
            variant="ghost"
            className="size-6 shrink-0 rounded-md"
            aria-label={t('preinstall.open_repo', { name: plugin.name })}
            onPress={() => onOpenRepo(plugin.id)}
          >
            <CircleInfo className="size-4" />
          </Button>
        </>
      )}
    />
  )
}

/** 日志控制台：dsh plugin 进程输出，顶部带复制按钮，样式与安装/加载页日志面板一致 */
function LogPanel({ logs }: { logs: readonly string[] }) {
  const { t } = useTranslation()
  const text = logs.join('\n')

  async function copyLogs() {
    try {
      await navigator.clipboard.writeText(text || '')
      toast(t('messages.log_copied'), {})
    }
    catch (err) {
      console.error('[Harness] copy preinstall logs failed:', err)
    }
  }

  return (
    <Logs
      logs={logs}
      limit={100}
      bodyClassName="max-h-[240px]"
      header={(
        <Button
          size="sm"
          variant="ghost"
          isIconOnly
          className="size-6 shrink-0 rounded-md"
          aria-label={t('buttons.copy')}
          onPress={copyLogs}
        >
          <Copy className="size-3.5" />
        </Button>
      )}
    />
  )
}

export function PreinstallSetup() {
  const { t } = useTranslation()
  const { preinstall } = useStore(store.harness)
  // 用户手动调整后的选择（一旦交互即接管默认勾选）
  const [selected, setSelected] = useState<Set<string>>(() => new Set())
  const [touched, setTouched] = useState(false)

  // 进入引导页时拉取插件列表
  useEffect(() => {
    void store.harness.loadPreinstallPlugins()
  }, [])

  // 默认勾选：未安装的推荐插件 +「修复」类项 + 无 chip 但标记默认勾选的项（如 dsh-notification）。
  // 派生计算而非在加载回调里 setState，避免与 store 的加载去重守卫竞争，
  // 保证插件到位后默认勾选必定生效（用户手动调整后以用户选择为准）。
  const effectiveSelected = !touched
    ? defaultSelectedSet(preinstall.plugins)
    : selected

  function toggle(id: string, checked: boolean) {
    // 首次交互以「当前默认勾选」为起点：selected 初始为空，若直接在其上增删，
    // 取消一个会误把其余默认项一并取消。这里先以 defaultSelectedSet 播种，
    // 再应用本次勾选，保证「取消一个 = 只取消这一个」。
    const seed = !touched ? defaultSelectedSet(preinstall.plugins) : null
    setTouched(true)
    setSelected((prev) => {
      const next = new Set(seed ?? prev)
      if (checked) {
        next.add(id)
      }
      else {
        next.delete(id)
      }
      return next
    })
  }

  function openRepo(id: string) {
    void invoke('open_preinstall_repo', { id }).catch((err) => {
      console.error('[Harness] open preinstall repo failed:', err)
    })
  }

  function handleConfirm() {
    void store.harness.confirmPreinstall([...effectiveSelected])
  }

  function handleSkip() {
    void store.harness.skipPreinstall()
  }

  // 可选中的插件（未安装项）勾选数，用于禁用"确定"
  const selectableCount = preinstall.plugins.filter(p => !p.installed).length
  const selectedCount = [...effectiveSelected].filter(id => preinstall.plugins.some(p => p.id === id && !p.installed)).length
  const installing = preinstall.installing

  return (
    <div className="flex h-full w-full items-center justify-center bg-canvas">
      <div className="flex w-[min(560px,88vw)] flex-col gap-5">
        <header className="flex flex-col items-center gap-1.5 text-center">
          <Typography type="h4">{t('preinstall.title')}</Typography>
          <Typography color="muted" type="body-sm" className="max-w-[440px]">{t('preinstall.subtitle')}</Typography>
        </header>

        <If
          cond={installing}
          else={(
            // 安装失败时不叠加插件列表，只展示错误 + 日志 + 重试/跳过
            <If
              cond={preinstall.error !== ''}
              else={(
                <>
                  {/* 插件列表 */}
                  <div className="space-y-3 flex-wrap gap-2">
                    <If
                      cond={preinstall.loadError === ''}
                      else={(
                        // 列表加载失败（区别于空列表）：错误说明 + 重试
                        <div className="flex flex-col items-center gap-2 rounded-md border border-danger/30 bg-danger/5 p-4 text-center">
                          <p className="text-xs font-medium text-danger">{t('preinstall.load_failed')}</p>
                          <p className="max-h-[80px] max-w-full overflow-y-auto break-all font-mono text-[11px] text-muted">
                            {preinstall.loadError}
                          </p>
                          <Button
                            className="h-8 rounded-md"
                            size="sm"
                            variant="primary"
                            onPress={() => void store.harness.loadPreinstallPlugins()}
                            isDisabled={preinstall.loading}
                          >
                            {t('app.retry')}
                          </Button>
                        </div>
                      )}
                    >
                      <If
                        cond={preinstall.plugins.length > 0}
                        else={(
                          <Empty>{t('preinstall.empty')}</Empty>
                        )}
                      >
                        {preinstall.plugins.map(plugin => (
                          <PluginRow
                            key={plugin.id}
                            plugin={plugin}
                            checked={effectiveSelected.has(plugin.id)}
                            disabled={installing}
                            onToggle={toggle}
                            onOpenRepo={openRepo}
                          />
                        ))}
                      </If>
                    </If>
                  </div>

                  {/* 操作区：跳过 / 确定 */}
                  <div className="flex items-center justify-end gap-2">
                    <Button className="h-8 rounded-md" size="sm" variant="tertiary" onPress={handleSkip} isDisabled={installing}>
                      {t('preinstall.skip')}
                    </Button>
                    <Button
                      className="h-8 rounded-md"
                      size="sm"
                      variant="primary"
                      onPress={handleConfirm}
                      isDisabled={installing || selectedCount === 0 || selectableCount === 0}
                    >
                      {t('preinstall.confirm')}
                    </Button>
                  </div>
                </>
              )}
            >
              {/* 安装失败：错误信息 + 日志 + 操作 */}
              <div className="flex flex-col gap-2.5">
                <div className="flex flex-col gap-2 rounded-md border border-danger/30 bg-danger/5 p-3">
                  <p className="text-xs font-medium text-danger">{t('preinstall.failed')}</p>
                  <p className="max-h-[120px] overflow-y-auto break-all font-mono text-[11px] leading-relaxed text-muted">
                    {preinstall.error}
                  </p>
                </div>
                <LogPanel logs={preinstall.logs} />
                <div className="flex items-center justify-end gap-2">
                  <Button className="h-8 rounded-md" size="sm" variant="tertiary" onPress={handleSkip} isDisabled={installing}>
                    {t('preinstall.skip')}
                  </Button>
                  <Button
                    className="h-8 rounded-md"
                    size="sm"
                    variant="primary"
                    onPress={handleConfirm}
                    isDisabled={installing || selectedCount === 0 || selectableCount === 0}
                  >
                    {t('app.retry')}
                  </Button>
                </div>
              </div>
            </If>
          )}
        >
          {/* 安装中：spinner 在上、文案在下，图标旁不加文字 */}
          <div className="flex flex-col gap-2.5">
            <div className="flex flex-col items-center gap-3">
              <Spinner size="md" color="current" />
              <p className="text-xs text-muted">{t('preinstall.installing')}</p>
            </div>
            <LogPanel logs={preinstall.logs} />
            {/* 取消安装：网络抖动/限流（429）时可能长时间卡在重试，给用户退出入口 */}
            <div className="flex items-center justify-center">
              <Button
                className="h-8 rounded-md"
                size="sm"
                variant="tertiary"
                onPress={store.harness.cancelPreinstall}
                isDisabled={preinstall.cancelling}
              >
                <Xmark className="size-3.5" />
                {preinstall.cancelling ? t('preinstall.cancelling') : t('preinstall.cancel')}
              </Button>
            </div>
          </div>
        </If>
      </div>
    </div>
  )
}
