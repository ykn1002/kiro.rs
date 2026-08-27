// 桌面壳（Tauri）集成。
//
// 通过 tauri.conf.json 的 withGlobalTauri，桌面壳会在页面注入 window.__TAURI__。
// 纯 Web 部署下该对象不存在，isDesktop() 返回 false，相关 UI 自动隐藏，
// 因此这里不引入 @tauri-apps/api npm 依赖，保持 Web 构建零影响。

interface TauriGlobal {
  core: {
    invoke: <T = unknown>(cmd: string, args?: Record<string, unknown>) => Promise<T>
  }
}

function tauri(): TauriGlobal | null {
  const g = window as unknown as { __TAURI__?: TauriGlobal }
  return g.__TAURI__ ?? null
}

/** 当前是否运行在桌面壳（Tauri）中。 */
export function isDesktop(): boolean {
  return tauri() !== null
}

export interface DesktopSettings {
  /** 静默启动：开机自启时不弹窗，仅驻留托盘 */
  silentStart: boolean
  /** 开机启动是否已在系统注册 */
  autostart: boolean
  /** 自动轻量模式：关窗后延迟销毁 WebView 释放内存 */
  autoLightweight: boolean
  /** 进入轻量模式的延迟（分钟，0 表示关窗立即销毁） */
  lightweightMinutes: number
}

// Tauri invoke 失败时 reject 的通常是字符串（命令返回的 Err(String)）或错误对象，
// 而非 axios 风格的 { response }。这里统一转成带前缀的 Error，避免上层兜底成「未知错误」。
function toError(e: unknown, cmd: string): Error {
  if (e instanceof Error) return e
  if (typeof e === 'string') return new Error(e)
  try {
    return new Error(`${cmd}: ${JSON.stringify(e)}`)
  } catch {
    return new Error(`${cmd}: ${String(e)}`)
  }
}

/** 读取桌面设置；非桌面环境返回 null。 */
export async function getDesktopSettings(): Promise<DesktopSettings | null> {
  const t = tauri()
  if (!t) return null
  try {
    return await t.core.invoke<DesktopSettings>('get_desktop_settings')
  } catch (e) {
    throw toError(e, 'get_desktop_settings')
  }
}

/** 写入桌面设置；非桌面环境为 no-op。 */
export async function setDesktopSettings(settings: DesktopSettings): Promise<void> {
  const t = tauri()
  if (!t) return
  try {
    await t.core.invoke('set_desktop_settings', {
      silentStart: settings.silentStart,
      autostart: settings.autostart,
      autoLightweight: settings.autoLightweight,
      lightweightMinutes: settings.lightweightMinutes,
    })
  } catch (e) {
    throw toError(e, 'set_desktop_settings')
  }
}

export interface PortStatus {
  /** 配置中期望的端口 */
  configured: number
  /** 实际监听端口（可能因冲突回退为随机端口） */
  actual: number
  /** 是否发生端口冲突 */
  conflicted: boolean
}

/** 读取端口状态；非桌面环境返回 null。 */
export async function getPortStatus(): Promise<PortStatus | null> {
  const t = tauri()
  if (!t) return null
  try {
    return await t.core.invoke<PortStatus>('get_port_status')
  } catch (e) {
    throw toError(e, 'get_port_status')
  }
}

/** 探测端口当前是否空闲；非桌面环境返回 true（不拦截）。 */
export async function checkPortAvailable(port: number): Promise<boolean> {
  const t = tauri()
  if (!t) return true
  try {
    return await t.core.invoke<boolean>('check_port_available', { port })
  } catch (e) {
    throw toError(e, 'check_port_available')
  }
}

/** 修改期望端口并写回配置（需重启生效）；非桌面环境为 no-op。 */
export async function setConfiguredPort(port: number): Promise<void> {
  const t = tauri()
  if (!t) return
  try {
    await t.core.invoke('set_configured_port', { port })
  } catch (e) {
    throw toError(e, 'set_configured_port')
  }
}

export interface LogLine {
  seq: number
  ts: string
  level: string
  target: string
  message: string
}

export interface LogPull {
  enabled: boolean
  lines: LogLine[]
}

/** 拉取序号 > after 的新日志行；非桌面环境返回 null。 */
export async function getLogs(after: number): Promise<LogPull | null> {
  const t = tauri()
  if (!t) return null
  try {
    return await t.core.invoke<LogPull>('get_logs', { after })
  } catch (e) {
    throw toError(e, 'get_logs')
  }
}

/** 开启/关闭日志捕获；非桌面环境为 no-op。 */
export async function setLogCapture(enabled: boolean): Promise<void> {
  const t = tauri()
  if (!t) return
  try {
    await t.core.invoke('set_log_capture', { enabled })
  } catch (e) {
    throw toError(e, 'set_log_capture')
  }
}

/** 清空日志缓冲；非桌面环境为 no-op。 */
export async function clearLogs(): Promise<void> {
  const t = tauri()
  if (!t) return
  try {
    await t.core.invoke('clear_logs')
  } catch (e) {
    throw toError(e, 'clear_logs')
  }
}

export interface ImportConfigResult {
  /** 用户是否取消了文件选择 */
  cancelled: boolean
  /** 导入成功后配置声明的端口 */
  port: number
}

/** 弹系统文件选择器导入完整 config.json（整体覆盖，重启生效）；非桌面环境返回 null。 */
export async function importConfig(): Promise<ImportConfigResult | null> {
  const t = tauri()
  if (!t) return null
  try {
    return await t.core.invoke<ImportConfigResult>('import_config')
  } catch (e) {
    throw toError(e, 'import_config')
  }
}


export interface SsoCredentialCandidate {
  refreshToken: string
  clientId: string
  clientSecret: string
  region: string | null
  sourceFile: string
}

/** 扫描本机 AWS SSO 缓存，返回可导入的凭证候选；非桌面环境返回 []。 */
export async function scanSsoCredentials(): Promise<SsoCredentialCandidate[]> {
  const t = tauri()
  if (!t) return []
  try {
    return await t.core.invoke<SsoCredentialCandidate[]>('scan_sso_credentials')
  } catch (e) {
    throw toError(e, 'scan_sso_credentials')
  }
}
