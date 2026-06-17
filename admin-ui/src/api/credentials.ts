import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  CredentialsStatusResponse,
  BalanceResponse,
  SuccessResponse,
  SetDisabledRequest,
  SetPriorityRequest,
  AddCredentialRequest,
  AddCredentialResponse,
  AppConfig,
  UpdateAppConfigRequest,
} from '@/types/api'

// 创建 axios 实例
const api = axios.create({
  baseURL: '/api/admin',
  headers: {
    'Content-Type': 'application/json',
  },
})

// 请求拦截器添加 API Key
api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) {
    config.headers['x-api-key'] = apiKey
  }
  return config
})

// 获取所有凭据状态
export async function getCredentials(): Promise<CredentialsStatusResponse> {
  const { data } = await api.get<CredentialsStatusResponse>('/credentials')
  return data
}

// 设置凭据禁用状态
export async function setCredentialDisabled(
  id: number,
  disabled: boolean
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/disabled`,
    { disabled } as SetDisabledRequest
  )
  return data
}

// 设置凭据优先级
export async function setCredentialPriority(
  id: number,
  priority: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/priority`,
    { priority } as SetPriorityRequest
  )
  return data
}

// 重置失败计数
export async function resetCredentialFailure(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/reset`)
  return data
}

// 强制刷新 Token
export async function forceRefreshToken(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/refresh`)
  return data
}

// 获取凭据余额
export async function getCredentialBalance(id: number): Promise<BalanceResponse> {
  const { data } = await api.get<BalanceResponse>(`/credentials/${id}/balance`)
  return data
}

// 添加新凭据
export async function addCredential(
  req: AddCredentialRequest
): Promise<AddCredentialResponse> {
  const { data } = await api.post<AddCredentialResponse>('/credentials', req)
  return data
}

// 删除凭据
export async function deleteCredential(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/credentials/${id}`)
  return data
}

// 负载均衡模式
export type LoadBalancingMode = 'priority' | 'balanced' | 'round-robin'

// 获取负载均衡模式
export async function getLoadBalancingMode(): Promise<{ mode: LoadBalancingMode }> {
  const { data } = await api.get<{ mode: LoadBalancingMode }>('/config/load-balancing')
  return data
}

// 设置负载均衡模式
export async function setLoadBalancingMode(mode: LoadBalancingMode): Promise<{ mode: LoadBalancingMode }> {
  const { data } = await api.put<{ mode: LoadBalancingMode }>('/config/load-balancing', { mode })
  return data
}

// 获取应用配置（页面可编辑子集）
export async function getAppConfig(): Promise<AppConfig> {
  const { data } = await api.get<AppConfig>('/config/app')
  return data
}

// 更新应用配置（回写文件并热生效）
export async function updateAppConfig(req: UpdateAppConfigRequest): Promise<AppConfig> {
  const { data } = await api.put<AppConfig>('/config/app', req)
  return data
}
