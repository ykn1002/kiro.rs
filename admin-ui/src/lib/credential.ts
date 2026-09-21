import type { BalanceResponse, CredentialStatusItem } from '@/types/api'

// 透传凭据的类型标签（Kiro 凭据返回 null，不显示）
export function passthroughKindLabel(kind: string | null | undefined): string | null {
  switch (kind) {
    case 'anthropic':
      return 'Claude 透传'
    case 'openai':
      return 'Codex 透传'
    default:
      return null
  }
}

// 是否透传凭据：钱包模式，只有余额（USD），无总额/百分比概念
export function isPassthroughKind(kind: string | null | undefined): boolean {
  return passthroughKindLabel(kind) !== null
}

// 凭据展示名：备注名 > 邮箱 > #ID
export function credentialTitle(credential: CredentialStatusItem): string {
  return credential.name || credential.email || `凭据 #${credential.id}`
}

// 剩余用量百分比（后端 usagePercentage 是「已使用」比例）
export function remainingPercentage(balance: BalanceResponse): number {
  return 100 - balance.usagePercentage
}

// 剩余用量对应的进度条颜色：剩得越少越红
export function remainingIndicatorClass(pct: number): string {
  if (pct < 20) return 'bg-destructive'
  if (pct < 40) return 'bg-warning'
  return 'bg-success'
}

// 额度数值统一两位小数（不加千分位，与卡片一致）
export function formatAmount(num: number): string {
  return num.toFixed(2)
}
