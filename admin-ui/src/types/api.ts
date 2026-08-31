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
  /** 凭据类型：kiro（默认）/ anthropic / openai（透传） */
  kind?: string
  /** 透传上游 base URL（仅透传凭据） */
  baseUrl?: string
  rpm: RpmStatus
  // 后端带回的本地缓存余额（未过期时才有），用于刷新页面后免手动查询即可展示
  cachedBalance?: BalanceResponse
}

// 单个模型的统计（累计调用/token + 实时 RPM）
export interface ModelStat {
  model: string
  requests: number
  inputTokens: number
  outputTokens: number
  totalTokens: number
  credits: number
  todayRequests: number
  todayInputTokens: number
  todayOutputTokens: number
  todayTotalTokens: number
  todayCredits: number
  rpm: number
}

// 监控指标响应（进程级计数器快照 + 凭据池概览 + 各模型统计）
export interface MetricsResponse {
  requestsSuccess: number
  requestsError: number
  localRpmRejected: number
  streamDecodeFailures: number
  upstreamRateLimited: number
  streamInterrupted: number
  streamRestarted: number
  uptimeSeconds: number
  credentialsAvailable: number
  credentialsTotal: number
  currentId: number
  models: ModelStat[]
}

// 监控时间序列：单个时间桶
export interface TimeBucket {
  bucket: number
  requests: number
  inputTokens: number
  outputTokens: number
  cacheReadTokens: number
  cacheWriteTokens: number
  credits: number
}

// 监控时间序列：某维度（模型 / 凭据）区间聚合
export interface DimBucket {
  key: string
  requests: number
  inputTokens: number
  outputTokens: number
  cacheReadTokens: number
  cacheWriteTokens: number
  credits: number
}

// 监控时间序列响应
export interface TimeseriesResponse {
  from: number
  to: number
  bucket: 'hour' | 'day'
  series: TimeBucket[]
  byModel: DimBucket[]
  byCredential: DimBucket[]
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
  /** 凭据类型：kiro（默认）/ anthropic / openai（透传） */
  kind?: 'kiro' | 'anthropic' | 'openai'
  /** 透传上游 base URL（透传凭据必填） */
  baseUrl?: string
  /** 透传上游 API Key（透传凭据必填，格式 sk-xxx） */
  upstreamApiKey?: string
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

// Write/Edit 分块写入策略
// triggerLines：多大才需要分块；chunkLines：每块上限（与 Kiro 官方 WRITE_LIMIT 对齐）
export interface ChunkedWritePolicy {
  enabled: boolean
  triggerLines: number
  chunkLines: number
}

// 应用配置（页面可编辑子集）当前值
export interface AppConfig {
  apiKey: string
  adminApiKey: string
  credentialRpm: number
  credentialRpmOpus?: number | null
  credentialRpmSonnet?: number | null
  credentialRpmHaiku?: number | null
  credentialRpmMaxWaitMs: number
  kiroVersion: string
  machineId?: string | null
  systemVersion: string
  nodeVersion: string
  streamingSdkVersion: string
  models: ModelDef[]
  defaultModel?: string | null
  modelAliases: Record<string, string>
  chunkedWritePolicy: ChunkedWritePolicy
  codexTruncationCorrection: boolean
  /** 全局代理 URL（所有凭据默认走它） */
  proxyUrl?: string | null
  /** 全局代理认证用户名 */
  proxyUsername?: string | null
  /** 全局代理是否已设置密码（不回传明文） */
  proxyPasswordSet: boolean
}

// 更新应用配置请求（全量替换可编辑子集）
export interface UpdateAppConfigRequest {
  apiKey: string
  adminApiKey?: string
  credentialRpm: number
  credentialRpmOpus?: number | null
  credentialRpmSonnet?: number | null
  credentialRpmHaiku?: number | null
  credentialRpmMaxWaitMs: number
  kiroVersion: string
  machineId?: string | null
  systemVersion: string
  nodeVersion: string
  streamingSdkVersion: string
  models: ModelDef[]
  defaultModel?: string | null
  modelAliases: Record<string, string>
  chunkedWritePolicy: ChunkedWritePolicy
  codexTruncationCorrection?: boolean
  /** 全局代理 URL；空串清除、缺省不改。需重启生效 */
  proxyUrl?: string
  /** 全局代理认证用户名；空串清除、缺省不改 */
  proxyUsername?: string
  /** 全局代理认证密码；空串清除、缺省保留原值 */
  proxyPassword?: string
}
