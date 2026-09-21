import { useEffect } from 'react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Badge } from '@/components/ui/badge'
import { Progress } from '@/components/ui/progress'
import { useCredentialBalance } from '@/hooks/use-credentials'
import { parseError } from '@/lib/utils'
import {
  credentialTitle,
  formatAmount,
  isPassthroughKind,
  remainingIndicatorClass,
  remainingPercentage,
} from '@/lib/credential'
import type { BalanceResponse, CredentialStatusItem } from '@/types/api'

interface BalanceDialogProps {
  credential: CredentialStatusItem | null
  /** 卡片上已有的余额，开窗时先显示，避免和外部展示不一致 */
  cachedBalance: BalanceResponse | null
  open: boolean
  onOpenChange: (open: boolean) => void
  /** 查询到新余额时回传，保持卡片和弹窗同源 */
  onBalanceLoaded?: (id: number, balance: BalanceResponse) => void
}

export function BalanceDialog({
  credential,
  cachedBalance,
  open,
  onOpenChange,
  onBalanceLoaded,
}: BalanceDialogProps) {
  const credentialId = credential?.id ?? null
  const { data, isLoading, error } = useCredentialBalance(open ? credentialId : null)

  // 查询结果回传给列表，卡片与弹窗始终展示同一份数据
  useEffect(() => {
    if (data && credentialId !== null) {
      onBalanceLoaded?.(credentialId, data)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [data, credentialId])

  const balance = data ?? cachedBalance
  const isPassthrough = isPassthroughKind(credential?.kind)

  const formatDate = (timestamp: number | null) => {
    if (!timestamp) return '未知'
    return new Date(timestamp * 1000).toLocaleString('zh-CN')
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>
            {credential ? credentialTitle(credential) : '凭据'} 余额信息
          </DialogTitle>
        </DialogHeader>

        {isLoading && !balance && (
          <div className="flex items-center justify-center py-8">
            <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-primary"></div>
          </div>
        )}

        {error && !balance && (() => {
          const parsed = parseError(error)
          return (
            <div className="py-6 space-y-3">
              <div className="flex items-center justify-center gap-2 text-red-500">
                <svg className="h-5 w-5" viewBox="0 0 20 20" fill="currentColor">
                  <path fillRule="evenodd" d="M10 18a8 8 0 100-16 8 8 0 000 16zM8.707 7.293a1 1 0 00-1.414 1.414L8.586 10l-1.293 1.293a1 1 0 101.414 1.414L10 11.414l1.293 1.293a1 1 0 001.414-1.414L11.414 10l1.293-1.293a1 1 0 00-1.414-1.414L10 8.586 8.707 7.293z" clipRule="evenodd" />
                </svg>
                <span className="font-medium">{parsed.title}</span>
              </div>
              {parsed.detail && (
                <div className="text-sm text-muted-foreground text-center px-4">
                  {parsed.detail}
                </div>
              )}
            </div>
          )
        })()}

        {balance &&
          // 透传凭据（钱包模式）：无总额/百分比概念，只显示余额（USD）
          (isPassthrough ? (
            <div className="space-y-4">
              {balance.subscriptionTitle && (
                <div className="text-center">
                  <Badge variant="secondary">{balance.subscriptionTitle}</Badge>
                </div>
              )}
              <div className="flex flex-col items-center gap-1 py-2">
                <span className="text-sm text-muted-foreground">账户余额</span>
                <span
                  className={
                    balance.remaining <= 0
                      ? 'text-3xl font-bold tabular-nums text-destructive'
                      : 'text-3xl font-bold tabular-nums'
                  }
                >
                  <span className="mr-0.5 text-base font-normal text-muted-foreground">$</span>
                  {formatAmount(balance.remaining)}
                </span>
                {balance.remaining <= 0 && (
                  <span className="text-sm text-destructive">余额已耗尽</span>
                )}
              </div>
            </div>
          ) : (
            (() => {
              // Kiro 凭据：与卡片一致，主指标是「剩余用量」百分比
              const remainingPct = remainingPercentage(balance)
              return (
                <div className="space-y-4">
                  {/* 订阅类型 */}
                  <div className="text-center">
                    {balance.subscriptionTitle ? (
                      <Badge variant="secondary">{balance.subscriptionTitle}</Badge>
                    ) : (
                      <span className="text-sm text-muted-foreground">未知订阅类型</span>
                    )}
                  </div>

                  {/* 剩余用量：方向与配色和卡片一致 */}
                  <div className="space-y-2">
                    <div className="flex items-baseline justify-between">
                      <span className="text-sm text-muted-foreground">剩余用量</span>
                      <span className="text-2xl font-bold leading-none tabular-nums">
                        {remainingPct.toFixed(0)}
                        <span className="ml-0.5 text-xs font-normal text-muted-foreground">%</span>
                      </span>
                    </div>
                    <Progress
                      value={remainingPct}
                      max={100}
                      className="h-1.5"
                      indicatorClassName={remainingIndicatorClass(remainingPct)}
                    />
                    <div className="text-right text-[11px] text-muted-foreground tabular-nums">
                      剩 {formatAmount(balance.remaining)} / {formatAmount(balance.usageLimit)}
                    </div>
                  </div>

                  {/* 详细信息 */}
                  <div className="grid grid-cols-2 gap-4 pt-4 border-t text-sm">
                    <div>
                      <span className="text-muted-foreground">已使用：</span>
                      <span className="font-medium tabular-nums">
                        {formatAmount(balance.currentUsage)}
                      </span>
                    </div>
                    <div>
                      <span className="text-muted-foreground">下次重置：</span>
                      <span className="font-medium">{formatDate(balance.nextResetAt)}</span>
                    </div>
                  </div>
                </div>
              )
            })()
          ))}
      </DialogContent>
    </Dialog>
  )
}
