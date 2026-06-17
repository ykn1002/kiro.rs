import { useState, useEffect } from 'react'
import { ChevronDown, ChevronRight, Eye, EyeOff, Plus, Trash2 } from 'lucide-react'
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
import { useAppConfig, useUpdateAppConfig } from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'
import type { ModelDef } from '@/types/api'

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

// PLACEHOLDER_BODY

export function SettingsDialog({ open, onOpenChange }: SettingsDialogProps) {
  const { data: config, isLoading, error } = useAppConfig()
  const { mutate: updateConfig, isPending } = useUpdateAppConfig()

  const [apiKey, setApiKey] = useState('')
  const [showApiKey, setShowApiKey] = useState(false)
  const [credentialRpm, setCredentialRpm] = useState('0')
  const [rpmOpus, setRpmOpus] = useState('')
  const [rpmSonnet, setRpmSonnet] = useState('')
  const [rpmHaiku, setRpmHaiku] = useState('')
  const [kiroVersion, setKiroVersion] = useState('')
  const [systemVersion, setSystemVersion] = useState('')
  const [nodeVersion, setNodeVersion] = useState('')
  const [models, setModels] = useState<ModelDef[]>([])
  const [modelsExpanded, setModelsExpanded] = useState(false)

  // 打开对话框（或拉到数据）时用服务端值回填表单
  useEffect(() => {
    if (!open || !config) return
    setModelsExpanded(false)
    setApiKey(config.apiKey)
    setCredentialRpm(String(config.credentialRpm ?? 0))
    setRpmOpus(config.credentialRpmOpus == null ? '' : String(config.credentialRpmOpus))
    setRpmSonnet(config.credentialRpmSonnet == null ? '' : String(config.credentialRpmSonnet))
    setRpmHaiku(config.credentialRpmHaiku == null ? '' : String(config.credentialRpmHaiku))
    setKiroVersion(config.kiroVersion)
    setSystemVersion(config.systemVersion)
    setNodeVersion(config.nodeVersion)
    setModels(config.models.map((m) => ({ ...m })))
  }, [open, config])

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

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()

    if (!apiKey.trim()) {
      toast.error('apiKey 不能为空')
      return
    }
    if (!kiroVersion.trim() || !systemVersion.trim() || !nodeVersion.trim()) {
      toast.error('版本信息均不能为空')
      return
    }
    if (models.length === 0) {
      setModelsExpanded(true)
      toast.error('至少需要一个模型定义')
      return
    }
    for (let i = 0; i < models.length; i++) {
      const m = models[i]
      if (!m.family.trim() || !m.kiroId.trim() || !m.displayId.trim() || !m.displayName.trim()) {
        setModelsExpanded(true)
        toast.error(`第 ${i + 1} 个模型的 family / kiroId / displayId / displayName 均不能为空`)
        return
      }
      if (m.maxTokens <= 0 || m.contextWindow <= 0) {
        setModelsExpanded(true)
        toast.error(`第 ${i + 1} 个模型的 maxTokens / contextWindow 必须为正数`)
        return
      }
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

    updateConfig(
      {
        apiKey: apiKey.trim(),
        credentialRpm: parseInt(credentialRpm, 10) || 0,
        credentialRpmOpus: parseOptionalRpm(rpmOpus),
        credentialRpmSonnet: parseOptionalRpm(rpmSonnet),
        credentialRpmHaiku: parseOptionalRpm(rpmHaiku),
        kiroVersion: kiroVersion.trim(),
        systemVersion: systemVersion.trim(),
        nodeVersion: nodeVersion.trim(),
        models: cleanedModels,
      },
      {
        onSuccess: () => {
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
      <DialogContent className="sm:max-w-3xl max-h-[88vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>系统设置</DialogTitle>
        </DialogHeader>

        {isLoading ? (
          <div className="py-12 text-center text-muted-foreground">加载配置中...</div>
        ) : error ? (
          <div className="py-12 text-center text-red-500">
            加载配置失败：{extractErrorMessage(error)}
          </div>
        ) : (
          <form onSubmit={handleSubmit} className="flex flex-col min-h-0 flex-1">
            <div className="space-y-6 py-4 overflow-y-auto flex-1 pr-1">
              {/* API Key */}
              <section className="space-y-2">
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
              </section>

              {/* RPM 限制 */}
              <section className="space-y-2">
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
                <p className="text-xs text-muted-foreground">
                  每个凭据每分钟请求数上限。0 或留空表示不单独限制（专用项留空回退到兜底值）
                </p>
              </section>

              {/* 版本信息 */}
              <section className="space-y-2">
                <h3 className="text-sm font-semibold">版本信息（上游指纹）</h3>
                <div className="grid grid-cols-1 gap-3 md:grid-cols-3">
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
                </div>
                <p className="text-xs text-muted-foreground">
                  仅影响后续新发起的上游请求与 Token 刷新
                </p>
              </section>

              {/* 模型列表 */}
              <section className="space-y-3">
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
