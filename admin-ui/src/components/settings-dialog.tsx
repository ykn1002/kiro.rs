import { useState, useEffect } from 'react'
import {
  ChevronDown,
  ChevronRight,
  Eye,
  EyeOff,
  Plus,
  Trash2,
  KeyRound,
  Gauge,
  FileText,
  Fingerprint,
  Boxes,
} from 'lucide-react'
import { toast } from 'sonner'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { TabNav, type TabItem } from '@/components/ui/tabs'
import { useAppConfig, useUpdateAppConfig } from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'
import {
  isDesktop,
  getDesktopSettings,
  setDesktopSettings,
  getPortStatus,
  setConfiguredPort,
  importConfig,
  type PortStatus,
} from '@/lib/desktop'
import { storage } from '@/lib/storage'
import { Monitor, FileUp } from 'lucide-react'
import type { ModelDef } from '@/types/api'

// 设置分组 Tab
type SettingsTab = 'general' | 'rpm' | 'write' | 'fingerprint' | 'models' | 'desktop'

// 「桌面」Tab 仅在 Tauri 桌面壳中出现
const DESKTOP = isDesktop()

const SETTINGS_TABS: TabItem[] = [
  { value: 'general', label: '常规', icon: KeyRound },
  { value: 'rpm', label: 'RPM 限流', icon: Gauge },
  { value: 'write', label: '写入与截断', icon: FileText },
  { value: 'fingerprint', label: '版本指纹', icon: Fingerprint },
  { value: 'models', label: '模型与别名', icon: Boxes },
  ...(DESKTOP ? [{ value: 'desktop', label: '桌面', icon: Monitor } as TabItem] : []),
]

interface SettingsDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

// 可选数字输入：空字符串 → undefined（沿用兜底值）
function parseOptionalRpm(value: string): number | null | undefined {
  const trimmed = value.trim()
  if (trimmed === '') return null
  const n = parseInt(trimmed, 10)
  return Number.isFinite(n) && n >= 0 ? n : null
}


interface ModelAliasRow {
  from: string
  to: string
}

function aliasesToRows(aliases: Record<string, string>): ModelAliasRow[] {
  return Object.entries(aliases).map(([from, to]) => ({ from, to }))
}

function rowsToAliases(rows: ModelAliasRow[]): Record<string, string> {
  const out: Record<string, string> = {}
  for (const row of rows) {
    const from = row.from.trim()
    const to = row.to.trim()
    if (from && to) {
      out[from] = to
    }
  }
  return out
}

export function SettingsDialog({ open, onOpenChange }: SettingsDialogProps) {
  const { data: config, isLoading, error } = useAppConfig()
  const { mutate: updateConfig, isPending } = useUpdateAppConfig()

  const [activeTab, setActiveTab] = useState<SettingsTab>('general')
  const [apiKey, setApiKey] = useState('')
  const [showApiKey, setShowApiKey] = useState(false)
  const [credentialRpm, setCredentialRpm] = useState('0')
  const [rpmOpus, setRpmOpus] = useState('')
  const [rpmSonnet, setRpmSonnet] = useState('')
  const [rpmHaiku, setRpmHaiku] = useState('')
  const [rpmMaxWaitMs, setRpmMaxWaitMs] = useState('0')
  const [kiroVersion, setKiroVersion] = useState('')
  const [machineId, setMachineId] = useState('')
  const [systemVersion, setSystemVersion] = useState('')
  const [nodeVersion, setNodeVersion] = useState('')
  const [streamingSdkVersion, setStreamingSdkVersion] = useState('')
  const [models, setModels] = useState<ModelDef[]>([])
  const [modelsExpanded, setModelsExpanded] = useState(true)
  const [defaultModel, setDefaultModel] = useState('')
  const [modelAliases, setModelAliases] = useState<ModelAliasRow[]>([])
  const [aliasesExpanded, setAliasesExpanded] = useState(false)
  const [chunkedEnabled, setChunkedEnabled] = useState(false)
  const [chunkedTriggerLines, setChunkedTriggerLines] = useState('150')
  const [chunkedChunkLines, setChunkedChunkLines] = useState('50')
  const [codexTruncationCorrection, setCodexTruncationCorrection] = useState(true)
  // 桌面壳设置（仅桌面版）
  const [silentStart, setSilentStart] = useState(false)
  const [autostart, setAutostart] = useState(false)
  const [portStatus, setPortStatus] = useState<PortStatus | null>(null)
  const [portInput, setPortInput] = useState('')
  const [portSaving, setPortSaving] = useState(false)
  const [adminApiKey, setAdminApiKeyState] = useState('')
  const [showAdminKey, setShowAdminKey] = useState(false)

  // 打开对话框（或拉到数据）时用服务端值回填表单
  useEffect(() => {
    if (!open || !config) return
    setActiveTab('general')
    // 打开时默认：模型列表展开、别名折叠
    setModelsExpanded(true)
    setAliasesExpanded(false)
    setApiKey(config.apiKey)
    setAdminApiKeyState(config.adminApiKey ?? '')
    setCredentialRpm(String(config.credentialRpm ?? 0))
    setRpmOpus(config.credentialRpmOpus == null ? '' : String(config.credentialRpmOpus))
    setRpmSonnet(config.credentialRpmSonnet == null ? '' : String(config.credentialRpmSonnet))
    setRpmHaiku(config.credentialRpmHaiku == null ? '' : String(config.credentialRpmHaiku))
    setRpmMaxWaitMs(String(config.credentialRpmMaxWaitMs ?? 0))
    setKiroVersion(config.kiroVersion)
    setMachineId(config.machineId ?? '')
    setSystemVersion(config.systemVersion)
    setNodeVersion(config.nodeVersion)
    setStreamingSdkVersion(config.streamingSdkVersion)
    setModels(config.models.map((m) => ({ ...m })))
    setDefaultModel(config.defaultModel ?? '')
    setModelAliases(aliasesToRows(config.modelAliases ?? {}))
    setChunkedEnabled(config.chunkedWritePolicy?.enabled ?? false)
    setChunkedTriggerLines(String(config.chunkedWritePolicy?.triggerLines ?? 150))
    setChunkedChunkLines(String(config.chunkedWritePolicy?.chunkLines ?? 50))
    setCodexTruncationCorrection(config.codexTruncationCorrection ?? true)
  }, [open, config])

  // 桌面设置独立加载（不依赖服务端 config）
  useEffect(() => {
    if (!open || !DESKTOP) return
    getDesktopSettings()
      .then((s) => {
        if (s) {
          setSilentStart(s.silentStart)
          setAutostart(s.autostart)
        }
      })
      .catch((e) => {
        toast.error(`读取桌面设置失败: ${extractErrorMessage(e)}`)
      })
    getPortStatus()
      .then((p) => {
        if (p) {
          setPortStatus(p)
          setPortInput(String(p.configured))
        }
      })
      .catch((e) => {
        toast.error(`读取端口状态失败: ${extractErrorMessage(e)}`)
      })
  }, [open])

  // 桌面开关即时生效（OS 级设置，不随「保存并生效」按钮走）
  const applyDesktop = (next: { silentStart?: boolean; autostart?: boolean }) => {
    const merged = {
      silentStart: next.silentStart ?? silentStart,
      autostart: next.autostart ?? autostart,
    }
    setSilentStart(merged.silentStart)
    setAutostart(merged.autostart)
    setDesktopSettings(merged).catch((e) => {
      toast.error(`保存桌面设置失败: ${extractErrorMessage(e)}`)
    })
  }

  // 保存端口：前端先校验范围，后端写回前会再探测可用性
  const handleSavePort = async () => {
    const port = parseInt(portInput, 10)
    if (!Number.isFinite(port) || port < 1 || port > 65535) {
      toast.error('端口需在 1–65535 之间')
      return
    }
    if (portStatus && port === portStatus.configured) {
      toast.info('端口未变化')
      return
    }
    setPortSaving(true)
    try {
      await setConfiguredPort(port)
      toast.success(`端口已改为 ${port}，重启应用后生效`)
      const p = await getPortStatus()
      if (p) setPortStatus(p)
    } catch (e) {
      toast.error(`保存端口失败: ${extractErrorMessage(e)}`)
    } finally {
      setPortSaving(false)
    }
  }

  const [importing, setImporting] = useState(false)
  // 导入完整 config.json（整体覆盖，重启生效）
  const handleImportConfig = async () => {
    setImporting(true)
    try {
      const res = await importConfig()
      if (res && !res.cancelled) {
        toast.success(`配置已导入（端口 ${res.port}），重启应用后生效`)
      }
    } catch (e) {
      toast.error(`导入配置失败: ${extractErrorMessage(e)}`)
    } finally {
      setImporting(false)
    }
  }

  const updateModel = (index: number, patch: Partial<ModelDef>) => {
    setModels((prev) => prev.map((m, i) => (i === index ? { ...m, ...patch } : m)))
  }

  const addModel = () => {
    setModels((prev) => [
      ...prev,
      {
        family: '',
        version: '',
        kiroId: '',
        displayId: '',
        displayName: '',
        created: Math.floor(Date.now() / 1000),
        maxTokens: 64000,
        contextWindow: 200000,
      },
    ])
  }

  const removeModel = (index: number) => {
    setModels((prev) => prev.filter((_, i) => i !== index))
  }

  const addAlias = () => {
    setModelAliases((prev) => [...prev, { from: '', to: '' }])
  }

  const updateAlias = (index: number, patch: Partial<ModelAliasRow>) => {
    setModelAliases((prev) => prev.map((row, i) => (i === index ? { ...row, ...patch } : row)))
  }

  const removeAlias = (index: number) => {
    setModelAliases((prev) => prev.filter((_, i) => i !== index))
  }

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()

    if (!apiKey.trim()) {
      setActiveTab('general')
      toast.error('apiKey 不能为空')
      return
    }
    if (!adminApiKey.trim()) {
      setActiveTab('general')
      toast.error('Admin API Key 不能为空')
      return
    }
    if (
      !kiroVersion.trim() ||
      !systemVersion.trim() ||
      !nodeVersion.trim() ||
      !streamingSdkVersion.trim()
    ) {
      setActiveTab('fingerprint')
      toast.error('版本信息均不能为空')
      return
    }
    if (models.length === 0) {
      setActiveTab('models')
      setModelsExpanded(true)
      toast.error('至少需要一个模型定义')
      return
    }
    for (let i = 0; i < models.length; i++) {
      const m = models[i]
      if (!m.family.trim() || !m.kiroId.trim() || !m.displayId.trim() || !m.displayName.trim()) {
        setActiveTab('models')
        setModelsExpanded(true)
        toast.error(`第 ${i + 1} 个模型的 family / kiroId / displayId / displayName 均不能为空`)
        return
      }
      if (m.maxTokens <= 0 || m.contextWindow <= 0) {
        setActiveTab('models')
        setModelsExpanded(true)
        toast.error(`第 ${i + 1} 个模型的 maxTokens / contextWindow 必须为正数`)
        return
      }
    }

    for (let i = 0; i < modelAliases.length; i++) {
      const row = modelAliases[i]
      const hasFrom = row.from.trim().length > 0
      const hasTo = row.to.trim().length > 0
      if (hasFrom !== hasTo) {
        setActiveTab('models')
        setAliasesExpanded(true)
        toast.error(`第 ${i + 1} 条模型别名的「客户端名」和「映射目标」需同时填写或同时留空`)
        return
      }
    }

    const triggerLines = parseInt(chunkedTriggerLines, 10) || 0
    const chunkLines = parseInt(chunkedChunkLines, 10) || 0
    if (chunkedEnabled && chunkLines <= 0) {
      setActiveTab('write')
      toast.error('分块写入的每块行数必须为正数')
      return
    }
    if (chunkedEnabled && triggerLines < chunkLines) {
      setActiveTab('write')
      toast.error('分块写入的触发行数不能小于每块行数')
      return
    }

    const cleanedModels: ModelDef[] = models.map((m) => ({
      family: m.family.trim(),
      version: m.version?.trim() ? m.version.trim() : null,
      kiroId: m.kiroId.trim(),
      displayId: m.displayId.trim(),
      displayName: m.displayName.trim(),
      created: m.created,
      maxTokens: m.maxTokens,
      contextWindow: m.contextWindow,
    }))

    const trimmedAdminKey = adminApiKey.trim()
    updateConfig(
      {
        apiKey: apiKey.trim(),
        adminApiKey: trimmedAdminKey,
        credentialRpm: parseInt(credentialRpm, 10) || 0,
        credentialRpmOpus: parseOptionalRpm(rpmOpus),
        credentialRpmSonnet: parseOptionalRpm(rpmSonnet),
        credentialRpmHaiku: parseOptionalRpm(rpmHaiku),
        credentialRpmMaxWaitMs: Math.max(0, parseInt(rpmMaxWaitMs, 10) || 0),
        kiroVersion: kiroVersion.trim(),
        machineId: machineId.trim(),
        systemVersion: systemVersion.trim(),
        nodeVersion: nodeVersion.trim(),
        streamingSdkVersion: streamingSdkVersion.trim(),
        models: cleanedModels,
        defaultModel: defaultModel.trim() || null,
        modelAliases: rowsToAliases(modelAliases),
        chunkedWritePolicy: {
          enabled: chunkedEnabled,
          // 关闭时也提交当前值，便于下次开启时保留用户填写的数
          triggerLines: triggerLines || 150,
          chunkLines: chunkLines || 50,
        },
        codexTruncationCorrection,
      },
      {
        onSuccess: () => {
          // adminApiKey 热生效后，本地存的旧登录 key 会立刻失效；同步更新以免当前会话被登出
          storage.setApiKey(trimmedAdminKey)
          toast.success('配置已保存并热生效')
          onOpenChange(false)
        },
        onError: (err: unknown) => {
          toast.error(`保存失败: ${extractErrorMessage(err)}`)
        },
      }
    )
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-3xl h-[85vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>系统设置</DialogTitle>
        </DialogHeader>

        {isLoading ? (
          <div className="flex flex-1 items-center justify-center text-muted-foreground">加载配置中...</div>
        ) : error ? (
          <div className="flex flex-1 items-center justify-center text-center text-destructive">
            加载配置失败：{extractErrorMessage(error)}
          </div>
        ) : (
          <form onSubmit={handleSubmit} className="flex flex-col min-h-0 flex-1">
            <div className="flex min-h-0 flex-1 gap-4 py-4">
              {/* 左侧 Tab 导航 */}
              <TabNav
                items={SETTINGS_TABS}
                value={activeTab}
                onChange={(v) => setActiveTab(v as SettingsTab)}
                className="w-36 shrink-0 border-r pr-2"
              />

              {/* 右侧内容区（按 Tab 切换） */}
              <div className="space-y-6 overflow-y-auto flex-1 pr-1">
              {/* API Key */}
              <section className={`space-y-2 ${activeTab === 'general' ? '' : 'hidden'}`}>
                <h3 className="text-sm font-semibold">客户端 API Key</h3>
                <div className="relative">
                  <Input
                    type={showApiKey ? 'text' : 'password'}
                    value={apiKey}
                    onChange={(e) => setApiKey(e.target.value)}
                    disabled={isPending}
                    placeholder="客户端访问密钥"
                    className="pr-10"
                  />
                  <button
                    type="button"
                    onClick={() => setShowApiKey((v) => !v)}
                    className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                    tabIndex={-1}
                  >
                    {showApiKey ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
                  </button>
                </div>
                <p className="text-xs text-muted-foreground">
                  修改后立即对后续客户端请求生效。注意：保存后旧密钥失效，请同步更新调用方
                </p>

                <h3 className="text-sm font-semibold pt-2">Admin API Key（管理密钥）</h3>
                <div className="relative">
                  <Input
                    type={showAdminKey ? 'text' : 'password'}
                    value={adminApiKey}
                    onChange={(e) => setAdminApiKeyState(e.target.value)}
                    disabled={isPending}
                    placeholder="管理面板访问密钥"
                    className="pr-10"
                  />
                  <button
                    type="button"
                    onClick={() => setShowAdminKey((v) => !v)}
                    className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                    tabIndex={-1}
                  >
                    {showAdminKey ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
                  </button>
                </div>
                <p className="text-xs text-muted-foreground">
                  访问本管理面板的密钥，保存后立即热生效。桌面版用它自动登录；当前页面会同步更新，不会被登出。
                  注意：其它已登录的客户端需用新密钥重新登录。
                </p>
              </section>

              {/* RPM 限制 */}
              <section className={`space-y-2 ${activeTab === 'rpm' ? '' : 'hidden'}`}>
                <h3 className="text-sm font-semibold">凭据 RPM 限制</h3>
                <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
                  <div className="space-y-1">
                    <label className="text-xs text-muted-foreground">兜底 (credentialRpm)</label>
                    <Input
                      type="number"
                      min="0"
                      value={credentialRpm}
                      onChange={(e) => setCredentialRpm(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <div className="space-y-1">
                    <label className="text-xs text-muted-foreground">Opus</label>
                    <Input
                      type="number"
                      min="0"
                      placeholder="兜底"
                      value={rpmOpus}
                      onChange={(e) => setRpmOpus(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <div className="space-y-1">
                    <label className="text-xs text-muted-foreground">Sonnet</label>
                    <Input
                      type="number"
                      min="0"
                      placeholder="兜底"
                      value={rpmSonnet}
                      onChange={(e) => setRpmSonnet(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <div className="space-y-1">
                    <label className="text-xs text-muted-foreground">Haiku</label>
                    <Input
                      type="number"
                      min="0"
                      placeholder="兜底"
                      value={rpmHaiku}
                      onChange={(e) => setRpmHaiku(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                </div>
                <div className="space-y-1 max-w-xs">
                  <label className="text-xs text-muted-foreground">
                    RPM 打满等待 (credentialRpmMaxWaitMs, ms)
                  </label>
                  <Input
                    type="number"
                    min="0"
                    value={rpmMaxWaitMs}
                    onChange={(e) => setRpmMaxWaitMs(e.target.value)}
                    disabled={isPending}
                  />
                </div>
                <p className="text-xs text-muted-foreground">
                  每个凭据每分钟请求数上限。0 或留空表示不单独限制（专用项留空回退到兜底值）。
                  「RPM 打满等待」：全部凭据达上限时，发出请求前最多等待的毫秒数；0 表示不等待、立即向客户端返回 429
                </p>
              </section>

              {/* 分块写入策略 */}
              <section className={`space-y-2 ${activeTab === 'write' ? '' : 'hidden'}`}>
                <div className="flex items-center justify-between">
                  <h3 className="text-sm font-semibold">Write/Edit 分块写入</h3>
                  <Switch
                    checked={chunkedEnabled}
                    onCheckedChange={setChunkedEnabled}
                    disabled={isPending}
                  />
                </div>
                <div className="grid grid-cols-1 gap-3 md:grid-cols-2 max-w-md">
                  <div className="space-y-1">
                    <label className="text-xs text-muted-foreground">
                      触发行数 (triggerLines)
                    </label>
                    <Input
                      type="number"
                      min="1"
                      value={chunkedTriggerLines}
                      onChange={(e) => setChunkedTriggerLines(e.target.value)}
                      disabled={isPending || !chunkedEnabled}
                    />
                  </div>
                  <div className="space-y-1">
                    <label className="text-xs text-muted-foreground">
                      每块行数 (chunkLines)
                    </label>
                    <Input
                      type="number"
                      min="1"
                      value={chunkedChunkLines}
                      onChange={(e) => setChunkedChunkLines(e.target.value)}
                      disabled={isPending || !chunkedEnabled}
                    />
                  </div>
                </div>
                <p className="text-xs text-muted-foreground">
                  向 Write/Edit 工具描述注入「超长内容分块写入」约束，规避模型输出被截断导致
                  tool_use 参数不完整、整次调用作废。内容超过「触发行数」才分块，每块不超过
                  「每块行数」（含 Write 首块与后续 Edit 追加）。默认 150 / 50——每块的 50
                  取自 Kiro IDE 官方 WRITE_LIMIT 常量，触发阈值放宽到 150 让中小文件一次写完。
                  截断发生时回灌给模型的纠正指令也使用「每块行数」。
                  <span className="text-amber-600 dark:text-amber-500">
                    {' '}
                    注意：Kiro 按请求次数计费，分块会把一次写入拆成多次工具往返，显著增加配额消耗。
                  </span>
                </p>
              </section>

              {/* codex 截断纠正 */}
              <section className={`space-y-2 ${activeTab === 'write' ? '' : 'hidden'}`}>
                <div className="flex items-center justify-between">
                  <h3 className="text-sm font-semibold">codex 截断纠正</h3>
                  <Switch
                    checked={codexTruncationCorrection}
                    onCheckedChange={setCodexTruncationCorrection}
                    disabled={isPending}
                  />
                </div>
                <p className="text-xs text-muted-foreground">
                  codex（/v1/responses）端工具参数被 max_tokens 截断时（收到开始却无 stop），
                  追加一段分块纠正文本提示模型分块写，行数取上面的「每块行数」。关闭后仅停用纠正文本，
                  挂空 item 的封口修复（补 output_item.done、置 status=incomplete）仍无条件生效。
                  <span className="text-muted-foreground">
                    {' '}
                    注意：此开关只作用于 codex 客户端，其对纠正文本的实际响应需自行验证。
                  </span>
                </p>
              </section>

              {/* 版本信息 */}
              <section className={`space-y-2 ${activeTab === 'fingerprint' ? '' : 'hidden'}`}>
                <h3 className="text-sm font-semibold">版本信息（上游指纹）</h3>
                <div className="grid grid-cols-1 gap-3 md:grid-cols-2 lg:grid-cols-4">
                  <div className="space-y-1">
                    <label className="text-xs text-muted-foreground">kiroVersion</label>
                    <Input
                      value={kiroVersion}
                      onChange={(e) => setKiroVersion(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <div className="space-y-1">
                    <label className="text-xs text-muted-foreground">systemVersion</label>
                    <Input
                      value={systemVersion}
                      onChange={(e) => setSystemVersion(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <div className="space-y-1">
                    <label className="text-xs text-muted-foreground">nodeVersion</label>
                    <Input
                      value={nodeVersion}
                      onChange={(e) => setNodeVersion(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <div className="space-y-1">
                    <label className="text-xs text-muted-foreground">streamingSdkVersion</label>
                    <Input
                      value={streamingSdkVersion}
                      onChange={(e) => setStreamingSdkVersion(e.target.value)}
                      disabled={isPending}
                      placeholder="1.0.39"
                    />
                  </div>
                  <div className="space-y-1 md:col-span-2 lg:col-span-4">
                    <label className="text-xs text-muted-foreground">machineId（全局默认）</label>
                    <Input
                      value={machineId}
                      onChange={(e) => setMachineId(e.target.value)}
                      disabled={isPending}
                      placeholder="64 位 hex 或 UUID"
                    />
                  </div>
                </div>
              </section>

              {/* OpenAI/Codex 模型映射 */}
              <section className={`space-y-3 ${activeTab === 'models' ? '' : 'hidden'}`}>
                <button
                  type="button"
                  onClick={() => setAliasesExpanded((v) => !v)}
                  className="flex items-center gap-1 text-sm font-semibold hover:text-foreground/80"
                >
                  {aliasesExpanded ? (
                    <ChevronDown className="h-4 w-4" />
                  ) : (
                    <ChevronRight className="h-4 w-4" />
                  )}
                  OpenAI / Codex 模型映射
                  <span className="ml-1 text-xs font-normal text-muted-foreground">
                    （{modelAliases.length} 个别名）
                  </span>
                </button>
                {aliasesExpanded && (
                  <>
                    <div className="space-y-1">
                      <label className="text-xs text-muted-foreground">defaultModel（未识别时回退）</label>
                      <Input
                        value={defaultModel}
                        onChange={(e) => setDefaultModel(e.target.value)}
                        disabled={isPending}
                        placeholder="claude-opus-4-6"
                        list="model-target-options"
                      />
                    </div>
                    <p className="text-xs text-muted-foreground">
                      Codex 发送 gpt-5.5 等 OpenAI 模型名时，通过别名或 defaultModel 映射到下方模型列表中的 displayId
                    </p>
                    <div className="space-y-2">
                      {modelAliases.map((row, index) => (
                        <div key={index} className="grid grid-cols-[1fr_1fr_auto] gap-2 items-end">
                          <LabeledInput
                            label="客户端模型名"
                            value={row.from}
                            disabled={isPending}
                            onChange={(v) => updateAlias(index, { from: v })}
                            placeholder="gpt-5.5"
                          />
                          <LabeledInput
                            label="映射目标"
                            value={row.to}
                            disabled={isPending}
                            onChange={(v) => updateAlias(index, { to: v })}
                            placeholder="claude-opus-4-6"
                          />
                          <Button
                            type="button"
                            size="icon"
                            variant="ghost"
                            className="h-9 w-9 text-destructive hover:text-destructive shrink-0"
                            onClick={() => removeAlias(index)}
                            disabled={isPending}
                          >
                            <Trash2 className="h-4 w-4" />
                          </Button>
                        </div>
                      ))}
                    </div>
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      className="w-full"
                      onClick={addAlias}
                      disabled={isPending}
                    >
                      <Plus className="h-4 w-4 mr-1" />
                      添加别名
                    </Button>
                    <datalist id="model-target-options">
                      {models.map((m) => (
                        <option key={m.displayId} value={m.displayId} />
                      ))}
                    </datalist>
                  </>
                )}
              </section>

              {/* 模型列表 */}
              <section className={`space-y-3 ${activeTab === 'models' ? '' : 'hidden'}`}>
                <button
                  type="button"
                  onClick={() => setModelsExpanded((v) => !v)}
                  className="flex items-center gap-1 text-sm font-semibold hover:text-foreground/80"
                >
                  {modelsExpanded ? (
                    <ChevronDown className="h-4 w-4" />
                  ) : (
                    <ChevronRight className="h-4 w-4" />
                  )}
                  模型列表
                  <span className="ml-1 text-xs font-normal text-muted-foreground">
                    （{models.length} 个）
                  </span>
                </button>
                {modelsExpanded && (
                  <>
                    <div className="space-y-3">
                      {models.map((m, index) => (
                        <ModelRow
                          key={index}
                          model={m}
                          index={index}
                          disabled={isPending}
                          onChange={updateModel}
                          onRemove={removeModel}
                        />
                      ))}
                      {models.length === 0 && (
                        <p className="text-sm text-muted-foreground text-center py-4">
                          暂无模型，点击「添加模型」新增
                        </p>
                      )}
                    </div>
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      className="w-full"
                      onClick={addModel}
                      disabled={isPending}
                    >
                      <Plus className="h-4 w-4 mr-1" />
                      添加模型
                    </Button>
                  </>
                )}
              </section>

              {/* 桌面版设置（仅 Tauri 壳） */}
              {DESKTOP && (
                <section className={`space-y-4 ${activeTab === 'desktop' ? '' : 'hidden'}`}>
                  {/* 监听端口 */}
                  <div className="space-y-2">
                    <h3 className="text-sm font-semibold">监听端口</h3>
                    {portStatus?.conflicted && (
                      <div className="rounded-md border border-amber-500/50 bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-400">
                        端口冲突：期望端口 {portStatus.configured} 被占用，当前临时监听在{' '}
                        <span className="font-mono font-semibold">{portStatus.actual}</span>。
                        改用其他空闲端口并重启应用即可恢复固定端口。
                      </div>
                    )}
                    <div className="flex items-end gap-2">
                      <div className="space-y-1">
                        <label className="text-xs text-muted-foreground">期望端口 (1–65535)</label>
                        <Input
                          type="number"
                          min="1"
                          max="65535"
                          value={portInput}
                          onChange={(e) => setPortInput(e.target.value)}
                          disabled={portSaving}
                          className="w-40"
                        />
                      </div>
                      <Button type="button" onClick={handleSavePort} disabled={portSaving}>
                        {portSaving ? '保存中...' : '保存端口'}
                      </Button>
                    </div>
                    <p className="text-xs text-muted-foreground">
                      当前实际监听：
                      <span className="font-mono">{portStatus?.actual ?? '—'}</span>
                      {portStatus && !portStatus.conflicted && '（与期望一致）'}
                      。修改端口需重启应用后生效；保存时会先检测端口是否空闲。
                    </p>
                  </div>

                  {/* 导入配置 */}
                  <div className="space-y-2 border-t pt-4">
                    <h3 className="text-sm font-semibold">导入配置</h3>
                    <Button
                      type="button"
                      variant="outline"
                      onClick={handleImportConfig}
                      disabled={importing}
                    >
                      <FileUp className="mr-1 h-4 w-4" />
                      {importing ? '导入中...' : '从文件导入 config.json'}
                    </Button>
                    <p className="text-xs text-muted-foreground">
                      选择一个完整的 config.json 整体覆盖当前配置（含 apiKey / adminApiKey / 端口 /
                      版本指纹 / 模型等全部字段）。导入前会校验格式与必填项，
                      <span className="text-amber-600 dark:text-amber-500">
                        {' '}
                        导入后需重启应用才生效
                      </span>
                      。
                    </p>
                  </div>

                  <div className="flex items-center justify-between">
                    <div>
                      <h3 className="text-sm font-semibold">开机启动</h3>
                      <p className="text-xs text-muted-foreground">
                        登录系统后自动启动 kiro-rs
                      </p>
                    </div>
                    <Switch
                      checked={autostart}
                      onCheckedChange={(v) => applyDesktop({ autostart: v })}
                    />
                  </div>
                  <div className="flex items-center justify-between">
                    <div>
                      <h3 className="text-sm font-semibold">静默启动</h3>
                      <p className="text-xs text-muted-foreground">
                        开机自启时不弹出窗口，仅驻留菜单栏托盘；手动打开始终显示窗口
                      </p>
                    </div>
                    <Switch
                      checked={silentStart}
                      onCheckedChange={(v) => applyDesktop({ silentStart: v })}
                    />
                  </div>
                  <p className="text-xs text-muted-foreground">
                    以上为本机桌面设置，切换后立即生效，不受下方「保存并生效」影响。
                    窗口关闭后会隐藏到菜单栏托盘，点击 Dock 图标或托盘图标可重新唤出。
                  </p>
                </section>
              )}
              </div>
            </div>

            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={isPending}>
                取消
              </Button>
              <Button type="submit" disabled={isPending}>
                {isPending ? '保存中...' : '保存并生效'}
              </Button>
            </DialogFooter>
          </form>
        )}
      </DialogContent>
    </Dialog>
  )
}

interface ModelRowProps {
  model: ModelDef
  index: number
  disabled: boolean
  onChange: (index: number, patch: Partial<ModelDef>) => void
  onRemove: (index: number) => void
}

function ModelRow({ model, index, disabled, onChange, onRemove }: ModelRowProps) {
  return (
    <div className="rounded-md border p-3 space-y-2">
      <div className="flex items-center justify-between">
        <span className="text-xs font-medium text-muted-foreground">
          #{index + 1} {model.displayName || model.displayId || '(未命名)'}
        </span>
        <Button
          type="button"
          size="icon"
          variant="ghost"
          className="h-7 w-7 text-destructive hover:text-destructive"
          onClick={() => onRemove(index)}
          disabled={disabled}
        >
          <Trash2 className="h-4 w-4" />
        </Button>
      </div>
      <div className="grid grid-cols-2 gap-2 md:grid-cols-3">
        <LabeledInput label="family" value={model.family} disabled={disabled}
          onChange={(v) => onChange(index, { family: v })} placeholder="opus / sonnet / haiku" />
        <LabeledInput label="version" value={model.version ?? ''} disabled={disabled}
          onChange={(v) => onChange(index, { version: v })} placeholder="如 4.8（haiku 可留空）" />
        <LabeledInput label="kiroId" value={model.kiroId} disabled={disabled}
          onChange={(v) => onChange(index, { kiroId: v })} placeholder="claude-opus-4.8" />
        <LabeledInput label="displayId" value={model.displayId} disabled={disabled}
          onChange={(v) => onChange(index, { displayId: v })} placeholder="claude-opus-4-8" />
        <LabeledInput label="displayName" value={model.displayName} disabled={disabled}
          onChange={(v) => onChange(index, { displayName: v })} placeholder="Claude Opus 4.8" />
        <LabeledInput label="maxTokens" type="number" value={String(model.maxTokens)} disabled={disabled}
          onChange={(v) => onChange(index, { maxTokens: parseInt(v, 10) || 0 })} />
        <LabeledInput label="contextWindow" type="number" value={String(model.contextWindow)} disabled={disabled}
          onChange={(v) => onChange(index, { contextWindow: parseInt(v, 10) || 0 })} />
      </div>
    </div>
  )
}

interface LabeledInputProps {
  label: string
  value: string
  disabled: boolean
  onChange: (value: string) => void
  type?: string
  placeholder?: string
}

function LabeledInput({ label, value, disabled, onChange, type, placeholder }: LabeledInputProps) {
  return (
    <div className="space-y-1">
      <label className="text-xs text-muted-foreground">{label}</label>
      <Input
        type={type}
        value={value}
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
        disabled={disabled}
      />
    </div>
  )
}
