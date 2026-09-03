import { useState } from 'react'
import { toast } from 'sonner'
import {
  RefreshCw,
  ChevronUp,
  ChevronDown,
  Wallet,
  Trash2,
  Loader2,
  Check,
  X,
  Pencil,
  RotateCcw,
} from 'lucide-react'
import { Card, CardContent, CardHeader } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Switch } from '@/components/ui/switch'
import { Input } from '@/components/ui/input'
import { Checkbox } from '@/components/ui/checkbox'
import { Progress } from '@/components/ui/progress'
import { cn } from '@/lib/utils'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import type { CredentialStatusItem, BalanceResponse } from '@/types/api'
import {
  useSetDisabled,
  useSetPriority,
  useSetName,
  useResetFailure,
  useDeleteCredential,
  useForceRefreshToken,
} from '@/hooks/use-credentials'

interface CredentialCardProps {
  credential: CredentialStatusItem
  onViewBalance: (id: number) => void
  selected: boolean
  onToggleSelect: () => void
  balance: BalanceResponse | null
  loadingBalance: boolean
}

function formatLastUsed(lastUsedAt: string | null): string {
  if (!lastUsedAt) return '从未使用'
  const date = new Date(lastUsedAt)
  const now = new Date()
  const diff = now.getTime() - date.getTime()
  if (diff < 0) return '刚刚'
  const seconds = Math.floor(diff / 1000)
  if (seconds < 60) return `${seconds} 秒前`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小时前`
  const days = Math.floor(hours / 24)
  return `${days} 天前`
}

// 构造凭据级 RPM 占用展示：仅显示有上限（>0）的模型类别，格式如 "Opus 2/5"
function formatRpmUsage(rpm: CredentialStatusItem['rpm']): string | null {
  if (!rpm) return null
  const parts: string[] = []
  if (rpm.limitOpus > 0) parts.push(`Opus ${rpm.counts.opus}/${rpm.limitOpus}`)
  if (rpm.limitSonnet > 0) parts.push(`Sonnet ${rpm.counts.sonnet}/${rpm.limitSonnet}`)
  if (rpm.limitHaiku > 0) parts.push(`Haiku ${rpm.counts.haiku}/${rpm.limitHaiku}`)
  if (rpm.limitOther > 0) parts.push(`其他 ${rpm.counts.other}/${rpm.limitOther}`)
  return parts.length > 0 ? parts.join('，') : null
}

// 认证方式的展示名
function authMethodLabel(method: string | null | undefined): string | null {
  if (!method) return null
  switch (method) {
    case 'api_key':
      return 'API Key'
    case 'idc':
      return 'IdC'
    case 'social':
      return 'Social'
    default:
      return method
  }
}

// 透传凭据的类型标签（Kiro 凭据返回 null，不显示）
function passthroughKindLabel(kind: string | null | undefined): string | null {
  switch (kind) {
    case 'anthropic':
      return 'Claude 透传'
    case 'openai':
      return 'Codex 透传'
    default:
      return null
  }
}

// 凭据健康状态：驱动标题前的状态圆点
type Health = 'active' | 'ok' | 'warn' | 'disabled'

function credentialHealth(credential: CredentialStatusItem): Health {
  if (credential.disabled) return 'disabled'
  if (credential.failureCount > 0 || credential.refreshFailureCount > 0) return 'warn'
  if (credential.isCurrent) return 'active'
  return 'ok'
}

const healthDot: Record<Health, { color: string; label: string }> = {
  active: { color: 'bg-primary', label: '当前活跃' },
  ok: { color: 'bg-success', label: '正常' },
  warn: { color: 'bg-warning', label: '存在失败记录' },
  disabled: { color: 'bg-muted-foreground/50', label: '已禁用' },
}

// 一行元信息：标签在左，值靠右
function MetaRow({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-2 text-sm">
      <span className="shrink-0 text-muted-foreground">{label}</span>
      <span className="min-w-0 truncate text-right font-medium">{children}</span>
    </div>
  )
}

export function CredentialCard({
  credential,
  onViewBalance,
  selected,
  onToggleSelect,
  balance,
  loadingBalance,
}: CredentialCardProps) {
  const [editingPriority, setEditingPriority] = useState(false)
  const [priorityValue, setPriorityValue] = useState(String(credential.priority))
  const [editingName, setEditingName] = useState(false)
  const [nameValue, setNameValue] = useState(credential.name ?? '')
  const [showDeleteDialog, setShowDeleteDialog] = useState(false)

  const setDisabled = useSetDisabled()
  const setPriority = useSetPriority()
  const setName = useSetName()
  const resetFailure = useResetFailure()
  const deleteCredential = useDeleteCredential()
  const forceRefresh = useForceRefreshToken()

  const handleToggleDisabled = () => {
    setDisabled.mutate(
      { id: credential.id, disabled: !credential.disabled },
      {
        onSuccess: (res) => {
          toast.success(res.message)
        },
        onError: (err) => {
          toast.error('操作失败: ' + (err as Error).message)
        },
      }
    )
  }

  const handlePriorityChange = () => {
    const newPriority = parseInt(priorityValue, 10)
    if (isNaN(newPriority) || newPriority < 0) {
      toast.error('优先级必须是非负整数')
      return
    }
    setPriority.mutate(
      { id: credential.id, priority: newPriority },
      {
        onSuccess: (res) => {
          toast.success(res.message)
          setEditingPriority(false)
        },
        onError: (err) => {
          toast.error('操作失败: ' + (err as Error).message)
        },
      }
    )
  }

  const startEditName = () => {
    setNameValue(credential.name ?? '')
    setEditingName(true)
  }

  const handleNameChange = () => {
    const trimmed = nameValue.trim()
    // 与当前值一致（都为空或字符串相同）则直接收起，不发请求
    if (trimmed === (credential.name ?? '')) {
      setEditingName(false)
      return
    }
    setName.mutate(
      { id: credential.id, name: trimmed === '' ? null : trimmed },
      {
        onSuccess: (res) => {
          toast.success(res.message)
          setEditingName(false)
        },
        onError: (err) => {
          toast.error('操作失败: ' + (err as Error).message)
        },
      }
    )
  }

  const handleReset = () => {
    resetFailure.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
      },
      onError: (err) => {
        toast.error('操作失败: ' + (err as Error).message)
      },
    })
  }

  const handleForceRefresh = () => {
    forceRefresh.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
      },
      onError: (err) => {
        toast.error('刷新失败: ' + (err as Error).message)
      },
    })
  }

  const handleDelete = () => {
    if (!credential.disabled) {
      toast.error('请先禁用凭据再删除')
      setShowDeleteDialog(false)
      return
    }

    deleteCredential.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
        setShowDeleteDialog(false)
      },
      onError: (err) => {
        toast.error('删除失败: ' + (err as Error).message)
      },
    })
  }

  const health = credentialHealth(credential)
  const dot = healthDot[health]
  const authLabel = authMethodLabel(credential.authMethod)
  const kindLabel = passthroughKindLabel(credential.kind)
  const isPassthrough = kindLabel !== null
  const rpmUsage = formatRpmUsage(credential.rpm)
  const hasFailure = credential.failureCount > 0 || credential.refreshFailureCount > 0
  const title = credential.name || credential.email || `凭据 #${credential.id}`
  // 有自定义名或邮箱作标题时，副标题才显示 #ID，避免「凭据 #3 / #3」重复
  const showIdInSub = Boolean(credential.name || credential.email)
  const remainingPct = balance ? 100 - balance.usagePercentage : null

  return (
    <>
      <Card
        className={cn(
          'flex flex-col rounded-xl border-border/70 shadow-sm transition-all hover:border-border hover:shadow-md',
          credential.isCurrent && 'ring-1 ring-primary/40',
          credential.disabled && 'opacity-70'
        )}
      >
        <CardHeader className="gap-0 space-y-0 p-4 pb-3">
          <div className="flex items-center gap-2.5">
            <Checkbox checked={selected} onCheckedChange={onToggleSelect} className="shrink-0" />

            <div className="min-w-0 flex-1">
              <div className="group flex items-center gap-2">
                <span
                  className={cn('h-2 w-2 shrink-0 rounded-full', dot.color)}
                  title={dot.label}
                />
                {editingName ? (
                  <span className="flex min-w-0 flex-1 items-center gap-1">
                    <Input
                      value={nameValue}
                      onChange={(e) => setNameValue(e.target.value)}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter') handleNameChange()
                        if (e.key === 'Escape') setEditingName(false)
                      }}
                      placeholder="备注名（留空清除）"
                      autoFocus
                      className="h-7 min-w-0 flex-1 text-sm"
                    />
                    <Button
                      size="icon"
                      variant="ghost"
                      className="h-7 w-7 shrink-0"
                      onClick={handleNameChange}
                      disabled={setName.isPending}
                    >
                      <Check className="h-4 w-4" />
                    </Button>
                    <Button
                      size="icon"
                      variant="ghost"
                      className="h-7 w-7 shrink-0"
                      onClick={() => setEditingName(false)}
                    >
                      <X className="h-4 w-4" />
                    </Button>
                  </span>
                ) : (
                  <>
                    <span
                      className="truncate text-sm font-semibold leading-tight"
                      title={title}
                    >
                      {title}
                    </span>
                    <button
                      type="button"
                      className="shrink-0 text-muted-foreground opacity-0 transition-opacity hover:text-primary group-hover:opacity-100"
                      onClick={startEditName}
                      title="编辑备注名"
                    >
                      <Pencil className="h-3 w-3" />
                    </button>
                  </>
                )}
              </div>
              <div className="mt-1 flex items-center gap-1.5 text-xs text-muted-foreground">
                {showIdInSub && <span className="font-mono">#{credential.id}</span>}
                {isPassthrough ? (
                  <>
                    {showIdInSub && <span className="text-border">·</span>}
                    <span>{kindLabel}</span>
                    {credential.baseUrl && (
                      <>
                        <span className="text-border">·</span>
                        <span className="truncate">{credential.baseUrl}</span>
                      </>
                    )}
                  </>
                ) : (
                  <>
                    {authLabel && (
                      <>
                        {showIdInSub && <span className="text-border">·</span>}
                        <span>{authLabel}</span>
                      </>
                    )}
                    {credential.endpoint && (
                      <>
                        {(showIdInSub || authLabel) && <span className="text-border">·</span>}
                        <span className="truncate">{credential.endpoint}</span>
                      </>
                    )}
                  </>
                )}
              </div>
            </div>

            <Switch
              checked={!credential.disabled}
              onCheckedChange={handleToggleDisabled}
              disabled={setDisabled.isPending}
              className="shrink-0"
            />
          </div>

          {/* 状态 badge 行（仅在有内容时出现） */}
          {(credential.isCurrent || credential.disabled || credential.hasProfileArn) && (
            <div className="mt-2.5 flex flex-wrap items-center gap-1.5">
              {credential.isCurrent && <Badge variant="success">当前活跃</Badge>}
              {credential.disabled && <Badge variant="destructive">已禁用</Badge>}
              {credential.disabled && credential.disabledReason && (
                <Badge variant="outline" className="text-destructive">
                  {credential.disabledReason}
                </Badge>
              )}
              {credential.hasProfileArn && <Badge variant="outline">Profile ARN</Badge>}
            </div>
          )}
        </CardHeader>

        <CardContent className="flex flex-1 flex-col gap-3 p-4 pt-0">
          {/* 余额区：透传凭据只有余额（无总额/百分比），Kiro 凭据显示剩余用量百分比 */}
          {isPassthrough ? (
            <div className="rounded-lg bg-muted/40 p-3">
              <div className="flex items-center justify-between">
                <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
                  <Wallet className="h-3.5 w-3.5" />
                  <span>账户余额</span>
                  {!loadingBalance && balance?.subscriptionTitle && (
                    <Badge variant="secondary" className="ml-0.5 px-1.5 py-0 text-[10px]">
                      {balance.subscriptionTitle}
                    </Badge>
                  )}
                </div>
                {loadingBalance ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin text-muted-foreground" />
                ) : balance ? (
                  <span
                    className={cn(
                      'text-lg font-bold leading-none tabular-nums',
                      balance.remaining <= 0 && 'text-destructive'
                    )}
                  >
                    <span className="mr-0.5 text-xs font-normal text-muted-foreground">$</span>
                    {balance.remaining.toFixed(2)}
                  </span>
                ) : (
                  <span className="text-xs text-muted-foreground">未知</span>
                )}
              </div>
              {!balance && !loadingBalance && (
                <div className="mt-1.5 text-[11px] text-muted-foreground">
                  点击「查看余额」获取账户余额
                </div>
              )}
              {balance && !loadingBalance && balance.remaining <= 0 && (
                <div className="mt-1.5 text-[11px] text-destructive">余额已耗尽</div>
              )}
            </div>
          ) : (
            <div className="rounded-lg bg-muted/40 p-3">
              <div className="flex items-center justify-between">
                <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
                  <Wallet className="h-3.5 w-3.5" />
                  <span>剩余用量</span>
                  {!loadingBalance && balance?.subscriptionTitle && (
                    <Badge variant="secondary" className="ml-0.5 px-1.5 py-0 text-[10px]">
                      {balance.subscriptionTitle}
                    </Badge>
                  )}
                </div>
                {loadingBalance ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin text-muted-foreground" />
                ) : remainingPct !== null ? (
                  <span className="text-lg font-bold leading-none tabular-nums">
                    {remainingPct.toFixed(0)}
                    <span className="ml-0.5 text-xs font-normal text-muted-foreground">%</span>
                  </span>
                ) : (
                  <span className="text-xs text-muted-foreground">未知</span>
                )}
              </div>
              {balance && !loadingBalance ? (
                <>
                  <Progress
                    value={remainingPct ?? 0}
                    max={100}
                    className="mt-2 h-1.5"
                    indicatorClassName={
                      (remainingPct ?? 0) < 20
                        ? 'bg-destructive'
                        : (remainingPct ?? 0) < 40
                          ? 'bg-warning'
                          : 'bg-success'
                    }
                  />
                  <div className="mt-1.5 text-right text-[11px] text-muted-foreground tabular-nums">
                    剩 {balance.remaining.toFixed(2)} / {balance.usageLimit.toFixed(2)}
                  </div>
                </>
              ) : (
                !loadingBalance && (
                  <div className="mt-1.5 text-[11px] text-muted-foreground">
                    点击「查询信息」或「查看余额」获取用量
                  </div>
                )
              )}
            </div>
          )}

          {/* 元信息：标签—值 竖排 */}
          <div className="space-y-2">
            <div className="flex items-center justify-between gap-2 text-sm">
              <span className="text-muted-foreground">优先级</span>
              {editingPriority ? (
                <span className="flex items-center gap-1">
                  <Input
                    type="number"
                    value={priorityValue}
                    onChange={(e) => setPriorityValue(e.target.value)}
                    className="h-7 w-14 text-sm"
                    min="0"
                  />
                  <Button
                    size="icon"
                    variant="ghost"
                    className="h-7 w-7"
                    onClick={handlePriorityChange}
                    disabled={setPriority.isPending}
                  >
                    <Check className="h-4 w-4" />
                  </Button>
                  <Button
                    size="icon"
                    variant="ghost"
                    className="h-7 w-7"
                    onClick={() => {
                      setEditingPriority(false)
                      setPriorityValue(String(credential.priority))
                    }}
                  >
                    <X className="h-4 w-4" />
                  </Button>
                </span>
              ) : (
                <button
                  type="button"
                  className="inline-flex items-center gap-1 font-medium hover:text-primary"
                  onClick={() => setEditingPriority(true)}
                  title="点击编辑优先级"
                >
                  {credential.priority}
                  <Pencil className="h-3 w-3 text-muted-foreground" />
                </button>
              )}
            </div>

            <MetaRow label="成功 / 失败">
              <span className="text-success">{credential.successCount}</span>
              <span className="mx-1 text-muted-foreground">/</span>
              <span className={hasFailure ? 'text-destructive' : ''}>
                {credential.failureCount}
              </span>
              {credential.refreshFailureCount > 0 && (
                <span className="ml-1 text-xs text-destructive">
                  刷新 {credential.refreshFailureCount}
                </span>
              )}
            </MetaRow>

            <MetaRow label="最后调用">{formatLastUsed(credential.lastUsedAt)}</MetaRow>

            {rpmUsage && <MetaRow label="RPM 占用">{rpmUsage}</MetaRow>}

            {credential.maskedApiKey && (
              <MetaRow label="API Key">
                <span className="font-mono">{credential.maskedApiKey}</span>
              </MetaRow>
            )}

            {credential.hasProxy && (
              <MetaRow label="代理">
                <span title={credential.proxyUrl}>{credential.proxyUrl}</span>
              </MetaRow>
            )}
          </div>

          {/* 操作区 */}
          <div className="mt-auto flex items-center gap-1.5 border-t pt-3">
            <Button
              size="sm"
              variant="secondary"
              className="flex-1"
              onClick={() => onViewBalance(credential.id)}
            >
              <Wallet className="mr-1 h-4 w-4" />
              查看余额
            </Button>
            <Button
              size="icon"
              variant="ghost"
              className="h-8 w-8"
              onClick={handleForceRefresh}
              disabled={
                forceRefresh.isPending ||
                credential.disabled ||
                credential.authMethod === 'api_key' ||
                isPassthrough
              }
              title={
                isPassthrough
                  ? '透传凭据无需刷新 Token'
                  : credential.authMethod === 'api_key'
                    ? 'API Key 凭据无需刷新 Token'
                    : credential.disabled
                      ? '已禁用的凭据无法刷新 Token'
                      : '强制刷新 Token'
              }
            >
              <RefreshCw className={cn('h-4 w-4', forceRefresh.isPending && 'animate-spin')} />
            </Button>
            <Button
              size="icon"
              variant="ghost"
              className="h-8 w-8"
              onClick={handleReset}
              disabled={resetFailure.isPending || !hasFailure}
              title="重置失败计数"
            >
              <RotateCcw className="h-4 w-4" />
            </Button>
            <div className="mx-0.5 h-5 w-px bg-border" />
            <Button
              size="icon"
              variant="ghost"
              className="h-8 w-8"
              title="提高优先级"
              onClick={() => {
                const newPriority = Math.max(0, credential.priority - 1)
                setPriority.mutate(
                  { id: credential.id, priority: newPriority },
                  {
                    onSuccess: (res) => toast.success(res.message),
                    onError: (err) => toast.error('操作失败: ' + (err as Error).message),
                  }
                )
              }}
              disabled={setPriority.isPending || credential.priority === 0}
            >
              <ChevronUp className="h-4 w-4" />
            </Button>
            <Button
              size="icon"
              variant="ghost"
              className="h-8 w-8"
              title="降低优先级"
              onClick={() => {
                const newPriority = credential.priority + 1
                setPriority.mutate(
                  { id: credential.id, priority: newPriority },
                  {
                    onSuccess: (res) => toast.success(res.message),
                    onError: (err) => toast.error('操作失败: ' + (err as Error).message),
                  }
                )
              }}
              disabled={setPriority.isPending}
            >
              <ChevronDown className="h-4 w-4" />
            </Button>
            <Button
              size="icon"
              variant="ghost"
              className="h-8 w-8 text-destructive hover:bg-destructive/10 hover:text-destructive"
              onClick={() => setShowDeleteDialog(true)}
              disabled={!credential.disabled}
              title={!credential.disabled ? '需要先禁用凭据才能删除' : '删除凭据'}
            >
              <Trash2 className="h-4 w-4" />
            </Button>
          </div>
        </CardContent>
      </Card>

      {/* 删除确认对话框 */}
      <Dialog open={showDeleteDialog} onOpenChange={setShowDeleteDialog}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>确认删除凭据</DialogTitle>
            <DialogDescription>
              您确定要删除凭据 #{credential.id} 吗？此操作无法撤销。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setShowDeleteDialog(false)}
              disabled={deleteCredential.isPending}
            >
              取消
            </Button>
            <Button
              variant="destructive"
              onClick={handleDelete}
              disabled={deleteCredential.isPending || !credential.disabled}
            >
              确认删除
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}
