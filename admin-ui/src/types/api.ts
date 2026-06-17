// 凭据状态响应
export interface CredentialsStatusResponse {
  total: number
  available: number
  currentId: number
  credentials: CredentialStatusItem[]
}

// 凭据级 RPM 各模型类别当前 60 秒窗口占用
export interface RpmWindowCounts {
  opus: number
  sonnet: number
  haiku: number
  other: number
}

// 凭据级 RPM 实时状态：窗口占用 + 生效上限（0 表示不限制）
export interface RpmStatus {
  counts: RpmWindowCounts
  limitOpus: number
  limitSonnet: number
  limitHaiku: number
  limitOther: number
}

// 单个凭据状态
export interface CredentialStatusItem {
  id: number
  priority: number
  disabled: boolean
  failureCount: number
  isCurrent: boolean
  expiresAt: string | null
  authMethod: string | null
  hasProfileArn: boolean
  email?: string
  refreshTokenHash?: string
  apiKeyHash?: string
  maskedApiKey?: string
  successCount: number
  lastUsedAt: string | null
  hasProxy: boolean
  proxyUrl?: string
  refreshFailureCount: number
  disabledReason?: string
  endpoint: string
  rpm: RpmStatus
}

// 余额响应
export interface BalanceResponse {
  id: number
  subscriptionTitle: string | null
  currentUsage: number
  usageLimit: number
  remaining: number
  usagePercentage: number
  nextResetAt: number | null
}

// 成功响应
export interface SuccessResponse {
  success: boolean
  message: string
}

// 错误响应
export interface AdminErrorResponse {
  error: {
    type: string
    message: string
  }
}

// 请求类型
export interface SetDisabledRequest {
  disabled: boolean
}

export interface SetPriorityRequest {
  priority: number
}

// 添加凭据请求
export interface AddCredentialRequest {
  refreshToken?: string
  authMethod?: 'social' | 'idc' | 'api_key'
  clientId?: string
  clientSecret?: string
  priority?: number
  authRegion?: string
  apiRegion?: string
  machineId?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  kiroApiKey?: string
  endpoint?: string
}

// 添加凭据响应
export interface AddCredentialResponse {
  success: boolean
  message: string
  credentialId: number
  email?: string
}

// 模型定义（与后端 ModelDef camelCase 对应）
export interface ModelDef {
  family: string
  version?: string | null
  kiroId: string
  displayId: string
  displayName: string
  created: number
  maxTokens: number
  contextWindow: number
}

// 应用配置（页面可编辑子集）当前值
export interface AppConfig {
  apiKey: string
  credentialRpm: number
  credentialRpmOpus?: number | null
  credentialRpmSonnet?: number | null
  credentialRpmHaiku?: number | null
  kiroVersion: string
  systemVersion: string
  nodeVersion: string
  models: ModelDef[]
}

// 更新应用配置请求（全量替换可编辑子集）
export interface UpdateAppConfigRequest {
  apiKey: string
  credentialRpm: number
  credentialRpmOpus?: number | null
  credentialRpmSonnet?: number | null
  credentialRpmHaiku?: number | null
  kiroVersion: string
  systemVersion: string
  nodeVersion: string
  models: ModelDef[]
}
