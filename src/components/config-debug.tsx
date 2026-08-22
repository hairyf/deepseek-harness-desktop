import { ArrowRotateRight, ArrowUpRightFromSquare, ChevronRight, CircleInfo, Copy, Folder, Power, TrashBin } from '@gravity-ui/icons'
import { Button, Chip, Description, Input, Link, ListBox, Select, Spinner, Surface, Switch, Tooltip } from '@heroui/react'
import { useMutation, useQuery } from '@tanstack/react-query'
import { invoke } from '@tauri-apps/api/core'
import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { If } from 'react-if-lite'
import { useStore } from 'valtio-define'
import { store } from '@/store'
import { toast } from '@/utils'
import { Info } from './info'

export interface RuntimeInfo {
  app_version: string
  dsh_version: string | null
  node_version: string
  service_url: string
  data_dir: string
  log_path: string
  platform: string
  arch: string
}

export interface CliLinkStatus {
  enabled: boolean
  shim_exists: boolean
  path_registered: boolean
  user_dsh_preserved: boolean
  bin_dir: string
  shim_path: string
}
export interface AppConfig {
  port: number
  auto_start: boolean
  cli_link_enabled: boolean
}

export function ConfigDebug() {
  const { t, i18n } = useTranslation()
  const { serviceRunning, busyAction, preinstall } = useStore(store.harness)
  const { updateInfo } = useStore(store.harnessUpdater)

  // 端口编辑态：用户尚未输入时为 undefined，由 `data?.port ?? 3080` 提供初值。
  // 初值不写入 state（避免 queryFn 副作用 / effect 同步），渲染与保存时统一
  // 读取 config 数据；用户一旦输入即以输入值为准。
  const [portInput, setPortInput] = useState<number>()

  const { data: info } = useQuery({
    queryKey: ['info'],
    queryFn: () => invoke<RuntimeInfo>('get_runtime_info'),
  })

  const { data: config, refetch: refreshConfig } = useQuery({
    queryKey: ['config'],
    queryFn: () => invoke<AppConfig>('get_app_config'),
  })
  const port = portInput ?? config?.port ?? 3080

  const { data: cliStatus, refetch: refreshCliStatus } = useQuery({
    queryKey: ['cli_status'],
    queryFn: () => invoke<CliLinkStatus>('get_cli_link_status'),
  })

  const { data: logs, refetch: refreshLogs } = useQuery({
    queryKey: ['logs'],
    queryFn: () => invoke<string>('read_service_logs'),
    refetchInterval: 2000,
  })

  const { mutate: onClearLogs } = useMutation({
    mutationFn: async () => {
      await invoke('clear_service_logs')
      await refreshLogs()
      toast(t('messages.logs_cleared'))
    },
    onError: (err: unknown) => {
      console.error('[ConfigDebug] clear logs failed:', err)
      toast(t('messages.logs_clear_failed'), { variant: 'danger' })
    },
  })

  const { mutate: onToggleCliLink } = useMutation({
    mutationFn: async (enabled: boolean) => {
      await invoke<AppConfig>('update_app_config', { cliLinkEnabled: enabled })
      await refreshCliStatus()
    },
    onError: (err: unknown) => {
      console.error('[ConfigDebug] toggle cli link failed:', err)
      toast(t('messages.cli_link_failed'), { variant: 'danger' })
    },
  })

  const { mutate: onCopyServiceUrl } = useMutation({
    mutationFn: async () => {
      await invoke('copy_service_url')
      toast(t('messages.copy_success'))
    },
    onError: (err: unknown) => {
      console.error('[ConfigDebug] copy url failed:', err)
      toast(t('messages.copy_failed'), { variant: 'danger' })
    },
  })

  const { mutate: onSavePort } = useMutation({
    mutationFn: async (port: number) => {
      // 保存前校验：必须是 1–65535 的整数（输入框可能被清空成 0 / 浮点 / NaN）
      if (!Number.isInteger(port) || port < 1 || port > 65535) {
        throw new Error('PORT_INVALID')
      }
      await invoke<AppConfig>('update_app_config', { port })
      await refreshConfig()
      const key = toast(t('messages.port_changed'), {
        variant: 'accent',
        description: t('messages.port_restart_hint'),
        timeout: 10_000,
        actionProps: {
          children: t('app.restart'),
          onPress: () => {
            store.harness.restart()
            toast.close(key)
          },
        },
      })
    },
    onError: (err: unknown) => {
      console.error('[ConfigDebug] save port failed:', err)
      if (String(err).includes('PORT_INVALID')) {
        toast(t('messages.port_invalid'), { variant: 'danger' })
      }
      else {
        toast(t('messages.port_save_failed'), { variant: 'danger' })
      }
    },
  })

  const { mutate: onRevealDataDir } = useMutation({
    mutationFn: () => invoke('reveal_data_dir'),
    onError: (err: unknown) => {
      console.error('[ConfigDebug] reveal data dir failed:', err)
      toast(t('messages.reveal_dir_failed'), { variant: 'danger' })
    },
  })

  return (
    <div className="space-y-3">
      <div className="space-y-1.5">
        <div className="flex items-center justify-between">
          <span className="text-xs font-semibold uppercase tracking-wider text-muted">
            {t('ui.connection_status')}
          </span>
          <Chip
            size="sm"
            variant="soft"
            color={serviceRunning ? 'success' : 'danger'}
            className="font-medium"
          >
            {serviceRunning ? t('ui.running') : t('ui.stopped')}
          </Chip>
        </div>
        <div className="space-y-1.5">
          <div className="flex gap-1.5">
            <Input
              readOnly
              variant="secondary"
              value={info?.service_url ?? '-'}
              aria-label={t('ui.service_url')}
              className="font-mono text-xs flex-1 rounded-md"
            />
            <Button
              size="sm"
              variant="ghost"
              isIconOnly
              className="rounded-md"

              onPress={() => onCopyServiceUrl()}
              aria-label={t('buttons.copy')}
            >
              <Copy className="size-3.5" />
            </Button>
            <Button
              size="sm"
              variant="ghost"
              className="rounded-md"
              isIconOnly
              onPress={store.harness.openBrowser}
              isDisabled={busyAction !== null}
              aria-label={t('app.open_browser')}
            >
              <If cond={busyAction === 'openBrowser'} then={<Spinner size="sm" color="current" />} else={<ArrowUpRightFromSquare className="size-3.5" />} />
            </Button>
          </div>
        </div>
      </div>
      <div className="flex items-center gap-2">
        <If cond={serviceRunning}>
          <Button
            size="sm"
            variant="tertiary"
            className="flex-1 rounded-md"
            onPress={store.harness.restart}
            isDisabled={busyAction !== null}
          >
            <If cond={busyAction === 'restart'} then={<Spinner size="sm" color="current" />} else={<ArrowRotateRight className="size-3.5" />} />
            {t('app.restart')}
          </Button>
          <Button
            size="sm"
            variant="danger"
            className="flex-1 rounded-md"
            onPress={store.harness.shutdown}
            isDisabled={busyAction !== null}
          >
            <If cond={busyAction === 'shutdown'} then={<Spinner size="sm" color="current" />} else={<Power className="size-3.5" />} />
            {t('app.shutdown')}
          </Button>
        </If>
      </div>
      <div className="border-t border-line/30" />
      <div>
        <div className="space-y-1">
          <Info term={t('ui.current_version')}>{info?.app_version ?? '-'}</Info>
          <Info term={t('ui.dsh_version')}>
            <span>{info?.dsh_version ?? '-'}</span>
            <If cond={updateInfo}>
              <Link className="ml-2 text-[10px] text-accent" onClick={store.harnessUpdater.showToast}>
                {t('menu.new_version')}
                <ChevronRight className="scale-75" />
              </Link>
            </If>

          </Info>
          <Info term={t('ui.node_version')}>{info?.node_version ? `v${info.node_version}` : '-'}</Info>
          <Info term="Platform">
            {info ? `${info.platform} / ${info.arch}` : '-'}
          </Info>
          <div className="flex items-center justify-between gap-2 text-xs">
            <span className="shrink-0 min-w-[30%] text-muted font-medium">{t('ui.data_dir')}</span>
            <span className="min-w-0 flex items-center gap-1">
              <span className="truncate font-mono text-[11px] text-muted/80" title={info?.data_dir ?? '-'}>
                {info?.data_dir ?? '-'}
              </span>
              <Button
                size="sm"
                variant="ghost"
                isIconOnly
                className="size-6 min-w-6 rounded-md"
                aria-label={t('app.reveal_dir')}
                onPress={() => onRevealDataDir()}
              >
                <Folder className="size-3.5" />
              </Button>
            </span>
          </div>
        </div>
      </div>
      <div className="border-t border-line/30" />
      <div className="space-y-1.5">
        <div>
          <div className="flex items-center justify-between">
            <span className="text-xs font-medium text-ink">{t('ui.cli_link_enabled')}</span>
            <Switch
              isSelected={cliStatus?.enabled ?? false}
              onChange={onToggleCliLink}
              aria-label={t('ui.cli_link_enabled')}
            >
              <Switch.Content>
                <Switch.Control>
                  <Switch.Thumb />
                </Switch.Control>
              </Switch.Content>
            </Switch>
          </div>
          <If cond={cliStatus != null}>
            <div className="flex flex-col">
              <If
                cond={!cliStatus?.user_dsh_preserved}
                else={(
                  <Description className="text-[10px] text-muted/70">
                    {t('ui.cli_link_user_dsh_preserved')}
                  </Description>
                )}
              >
                <Description className="text-[10px] text-muted/70">{cliStatus?.bin_dir}</Description>
                <Description className="text-[10px] text-muted/70">
                  {t('ui.cli_link_hint')}
                </Description>
              </If>
            </div>
          </If>
        </div>

        <div className="flex items-center justify-between gap-2">
          <span className="text-xs font-medium text-ink">{t('ui.port')}</span>
          <div className="flex items-center gap-1.5">
            <Input
              type="number"
              variant="secondary"
              value={String(port)}
              onChange={e => setPortInput(Number(e.target.value))}
              className="w-24 h-8 rounded-md [appearance:textfield] [&::-webkit-outer-spin-button]:appearance-none [&::-webkit-inner-spin-button]:appearance-none"
              aria-label={t('ui.port')}
            />
            <Button
              size="sm"
              variant="primary"
              className="rounded-md h-8"
              onPress={() => onSavePort(port)}
            >
              {t('buttons.save')}
            </Button>
          </div>
        </div>
        <div className="flex items-center justify-between gap-2">
          <div className="flex items-center gap-2 flex-1">
            <span className="text-xs font-medium text-ink">{t('preinstall.settings_title')}</span>
          </div>
          <Tooltip delay={0}>
            <Button
              isIconOnly
              size="sm"
              className="rounded-md text-xs size-6 text-muted"
              variant="ghost"
            >
              <CircleInfo />
            </Button>
            <Tooltip.Content>
              <p>{t('preinstall.settings_hint')}</p>
            </Tooltip.Content>
          </Tooltip>
          <Button
            size="sm"
            variant="primary"
            className="rounded-md"
            onPress={store.harness.openPreinstall}
            isDisabled={busyAction !== null || preinstall.installing}
          >
            {t('preinstall.open_preset')}
          </Button>
        </div>

        <div className="flex items-center justify-between">
          <span className="text-xs font-medium text-ink">{t('ui.language')}</span>
          <Select
            variant="secondary"
            selectedKey={i18n.language}
            onSelectionChange={key => i18n.changeLanguage(String(key))}
            className="w-[80px]"
            aria-label={t('ui.language')}
          >
            <Select.Trigger className="rounded-md min-h-8! h-8 py-0 items-center">
              <Select.Value />
              <Select.Indicator />
            </Select.Trigger>
            <Select.Popover className="rounded-md">
              <ListBox>
                <ListBox.Item className="rounded-md min-h-8!" id="zh-CN" textValue="中文">中文</ListBox.Item>
                <ListBox.Item className="rounded-md min-h-8!" id="en-US" textValue="English">English</ListBox.Item>
              </ListBox>
            </Select.Popover>
          </Select>
        </div>
      </div>

      <div className="border-t border-line/30" />

      <div className="space-y-1.5">
        <div className="flex items-center justify-between">
          <span className="text-xs font-medium text-ink">{t('ui.logs')}</span>
          <div className="flex gap-1">
            <Button
              isIconOnly
              size="sm"
              className="rounded-md size-6"
              variant="ghost"
              onPress={async () => {
                await navigator.clipboard.writeText(logs || '')
                toast(t('messages.logs_copied'))
              }}
            >
              <Copy className="scale-80" />
            </Button>
            <Button
              isIconOnly
              size="sm"
              className="rounded-md size-6"
              variant="ghost"
              onPress={() => onClearLogs()}
            >
              <TrashBin className="scale-80" />
            </Button>
          </div>
        </div>
        <Surface className="bg-default rounded-md p-2 min-h-[140px] max-h-[180px] font-mono text-[11px] w-full leading-relaxed overflow-auto">
          {logs || t('ui.no_logs')}
        </Surface>
      </div>
    </div>
  )
}
