import { useMemo, useRef, useState } from 'react'
import { toast } from 'sonner'
import { CheckCircle2, XCircle, Loader2, Upload } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { useCredentials, useAddCredential, useDeleteCredential } from '@/hooks/use-credentials'
import { getCredentialBalance, setCredentialDisabled } from '@/api/credentials'
import { extractErrorMessage, sha256Hex } from '@/lib/utils'
import { isDesktop, scanSsoCredentials } from '@/lib/desktop'

interface AwsSsoImportDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

// OIDC 客户端注册文件（db6...json）：clientId + clientSecret
interface ClientRegistration {
  clientId: string
  clientSecret: string
}

// 鉴权 Token 文件（kiro-auth-token.json）：refreshToken + region 等
interface AuthToken {
  refreshToken: string
  region?: string
  authMethod?: string
  provider?: string
}

type Status = 'idle' | 'verifying' | 'verified' | 'duplicate' | 'failed'

interface ImportResult {
  status: Status
  error?: string
  usage?: string
  email?: string
}

// 解析 OIDC 客户端注册 JSON
function parseClientJson(raw: string): ClientRegistration {
  const parsed = JSON.parse(raw)
  if (typeof parsed !== 'object' || parsed === null) {
    throw new Error('客户端 JSON 必须是对象')
  }
  const clientId = typeof parsed.clientId === 'string' ? parsed.clientId.trim() : ''
  const clientSecret = typeof parsed.clientSecret === 'string' ? parsed.clientSecret.trim() : ''
  if (!clientId || !clientSecret) {
    throw new Error('客户端 JSON 缺少 clientId 或 clientSecret')
  }
  return { clientId, clientSecret }
}

// 解析鉴权 Token JSON
function parseTokenJson(raw: string): AuthToken {
  const parsed = JSON.parse(raw)
  if (typeof parsed !== 'object' || parsed === null) {
    throw new Error('Token JSON 必须是对象')
  }
  const refreshToken = typeof parsed.refreshToken === 'string' ? parsed.refreshToken.trim() : ''
  if (!refreshToken) {
    throw new Error('Token JSON 缺少 refreshToken')
  }
  return {
    refreshToken,
    region: typeof parsed.region === 'string' ? parsed.region.trim() : undefined,
    authMethod: typeof parsed.authMethod === 'string' ? parsed.authMethod : undefined,
    provider: typeof parsed.provider === 'string' ? parsed.provider : undefined,
  }
}

// 按当前平台返回 SSO 缓存目录的展示路径（仅用于提示文案）
function ssoCacheDisplayPath(): string {
  const ua = navigator.userAgent
  if (/Windows/i.test(ua)) return 'C:\\Users\\{user}\\.aws\\sso\\cache'
  return '~/.aws/sso/cache'
}

export function AwsSsoImportDialog({ open, onOpenChange }: AwsSsoImportDialogProps) {
  const [clientJson, setClientJson] = useState('')
  const [tokenJson, setTokenJson] = useState('')
  const [importing, setImporting] = useState(false)
  const [result, setResult] = useState<ImportResult | null>(null)
  const [scanning, setScanning] = useState(false)
  const [scanMsg, setScanMsg] = useState('')

  const clientFileRef = useRef<HTMLInputElement>(null)
  const tokenFileRef = useRef<HTMLInputElement>(null)

  const { data: existingCredentials } = useCredentials()
  const { mutateAsync: addCredential } = useAddCredential()
  const { mutateAsync: deleteCredential } = useDeleteCredential()

  const resetForm = () => {
    setClientJson('')
    setTokenJson('')
    setResult(null)
  }

  const handleFile = (
    e: React.ChangeEvent<HTMLInputElement>,
    setter: (value: string) => void
  ) => {
    const file = e.target.files?.[0]
    if (!file) return
    const reader = new FileReader()
    reader.onload = () => setter(typeof reader.result === 'string' ? reader.result : '')
    reader.onerror = () => toast.error('读取文件失败')
    reader.readAsText(file)
    // 允许重复选择同一文件
    e.target.value = ''
  }

  // 解析预览
  const { client, token, parseError } = useMemo(() => {
    if (!clientJson.trim() || !tokenJson.trim()) {
      return { client: null, token: null, parseError: '' }
    }
    try {
      return {
        client: parseClientJson(clientJson),
        token: parseTokenJson(tokenJson),
        parseError: '',
      }
    } catch (e) {
      return { client: null, token: null, parseError: extractErrorMessage(e) }
    }
  }, [clientJson, tokenJson])

  const rollbackCredential = async (id: number): Promise<void> => {
    try {
      await setCredentialDisabled(id, true)
      await deleteCredential(id)
    } catch (error) {
      toast.warning(`回滚失败，请手动禁用并删除凭据 #${id}: ${extractErrorMessage(error)}`)
    }
  }

  // 导入并验活单条 IdC 凭据。返回结果类别，不直接改 UI（供手动/自动扫描共用）。
  type ImportOneResult =
    | { kind: 'duplicate'; email?: string }
    | { kind: 'verified'; email?: string; usage?: string }
    | { kind: 'failed'; error: string }
  const importOne = async (params: {
    clientId: string
    clientSecret: string
    refreshToken: string
    region?: string
  }): Promise<ImportOneResult> => {
    // 去重
    const tokenHash = await sha256Hex(params.refreshToken)
    const existing = existingCredentials?.credentials.find(c => c.refreshTokenHash === tokenHash)
    if (existing) {
      return { kind: 'duplicate', email: existing.email }
    }
    let addedCredId: number | null = null
    try {
      const addedCred = await addCredential({
        refreshToken: params.refreshToken,
        authMethod: 'idc',
        clientId: params.clientId,
        clientSecret: params.clientSecret,
        authRegion: params.region || undefined,
      })
      addedCredId = addedCred.credentialId
      let usage: string | undefined
      try {
        await new Promise(resolve => setTimeout(resolve, 1000))
        const balance = await getCredentialBalance(addedCred.credentialId)
        usage = `${balance.currentUsage}/${balance.usageLimit}`
      } catch {
        // IdC 不支持用量查询，忽略
      }
      return { kind: 'verified', email: addedCred.email, usage }
    } catch (error) {
      if (addedCredId) {
        await rollbackCredential(addedCredId)
      }
      return { kind: 'failed', error: extractErrorMessage(error) }
    }
  }

  const handleImport = async () => {
    let parsedClient: ClientRegistration
    let parsedToken: AuthToken
    try {
      parsedClient = parseClientJson(clientJson)
      parsedToken = parseTokenJson(tokenJson)
    } catch (error) {
      toast.error('JSON 格式错误: ' + extractErrorMessage(error))
      return
    }

    setImporting(true)
    setResult({ status: 'verifying' })
    const r = await importOne({
      clientId: parsedClient.clientId,
      clientSecret: parsedClient.clientSecret,
      refreshToken: parsedToken.refreshToken,
      region: parsedToken.region,
    })
    if (r.kind === 'duplicate') {
      setResult({ status: 'duplicate', error: '该凭据已存在', email: r.email })
      toast.info('该凭据已存在')
    } else if (r.kind === 'verified') {
      setResult({ status: 'verified', email: r.email, usage: r.usage })
      toast.success('AWS SSO 凭据导入并验活成功')
    } else {
      setResult({ status: 'failed', error: r.error })
      toast.error('导入失败: ' + r.error)
    }
    setImporting(false)
  }

  // 一键导入：扫描本机 AWS SSO 缓存并逐条导入验活（仅桌面版）
  const handleScanImport = async () => {
    setScanning(true)
    setScanMsg('正在扫描本机 AWS SSO 缓存…')
    try {
      const candidates = await scanSsoCredentials()
      if (candidates.length === 0) {
        setScanMsg(`未在本机 ${ssoCacheDisplayPath()} 找到可导入的凭证`)
        toast.info('未扫描到可导入的凭证')
        return
      }
      let ok = 0
      let dup = 0
      let fail = 0
      for (let i = 0; i < candidates.length; i++) {
        const c = candidates[i]
        setScanMsg(`导入中 ${i + 1}/${candidates.length}（${c.sourceFile}）…`)
        const r = await importOne({
          clientId: c.clientId,
          clientSecret: c.clientSecret,
          refreshToken: c.refreshToken,
          region: c.region ?? undefined,
        })
        if (r.kind === 'verified') ok++
        else if (r.kind === 'duplicate') dup++
        else fail++
      }
      setScanMsg(`完成：新增 ${ok}，已存在 ${dup}，失败 ${fail}`)
      if (ok > 0) toast.success(`一键导入完成：新增 ${ok} 个凭证`)
      else if (dup > 0 && fail === 0) toast.info('扫描到的凭证均已存在')
      else if (fail > 0) toast.error(`有 ${fail} 个凭证导入失败`)
    } catch (e) {
      const msg = extractErrorMessage(e)
      setScanMsg(`扫描失败：${msg}`)
      toast.error('扫描失败: ' + msg)
    } finally {
      setScanning(false)
    }
  }

  const placeholderClient = '{\n  "clientId": "...",\n  "clientSecret": "..."\n}'
  const placeholderToken =
    '{\n  "refreshToken": "...",\n  "authMethod": "IdC",\n  "region": "us-east-1"\n}'

  return (
    <Dialog
      open={open}
      onOpenChange={(newOpen) => {
        if (!newOpen && importing) return
        if (!newOpen) resetForm()
        onOpenChange(newOpen)
      }}
    >
      <DialogContent className="sm:max-w-2xl max-h-[80vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>AWS SSO 导入（自动验活）</DialogTitle>
        </DialogHeader>

        <div className="flex-1 overflow-y-auto space-y-4 py-4">
          {/* 一键导入（仅桌面版：自动扫描本机 AWS SSO 缓存）*/}
          {isDesktop() && (
            <div className="rounded-md border bg-muted/30 px-3 py-3 space-y-2">
              <div className="flex items-center justify-between gap-3">
                <div>
                  <div className="text-sm font-medium">一键导入（自动扫描）</div>
                  <div className="text-xs text-muted-foreground">
                    自动读取本机 <code className="font-mono">{ssoCacheDisplayPath()}</code>{' '}
                    并导入验活，无需手动粘贴
                  </div>
                </div>
                <Button onClick={handleScanImport} disabled={scanning || importing}>
                  {scanning ? (
                    <>
                      <Loader2 className="mr-1 h-4 w-4 animate-spin" />
                      扫描导入中
                    </>
                  ) : (
                    '一键导入'
                  )}
                </Button>
              </div>
              {scanMsg && <div className="text-xs text-muted-foreground">{scanMsg}</div>}
            </div>
          )}

          <p className="text-sm text-muted-foreground">
            AWS IAM Identity Center（IdC/SSO）凭据由两个 JSON 文件组成：客户端注册文件
            （含 clientId / clientSecret）与鉴权 Token 文件（含 refreshToken / region）。
            请分别粘贴或上传。
          </p>

          <div className="rounded-md border border-dashed bg-muted/40 px-3 py-2 text-xs text-muted-foreground space-y-1">
            <div className="font-medium text-foreground">文件路径（位于 SSO 缓存目录）</div>
            <div>
              macOS：<code className="font-mono">/Users/{'{user}'}/.aws/sso/cache/</code>
            </div>
            <div>
              Windows：<code className="font-mono">C:\Users\{'{user}'}\.aws\sso\cache\</code>
            </div>
            <div>
              鉴权 Token 文件：<code className="font-mono">kiro-auth-token.json</code>
            </div>
            <div>
              客户端注册文件：<code className="font-mono">{'<机器ID（32位十六进制）>'}.json</code>
            </div>
          </div>

          {/* 客户端注册文件 */}
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <label className="text-sm font-medium">
                客户端注册 JSON <span className="text-red-500">*</span>
                <span className="text-xs text-muted-foreground ml-1">（clientId / clientSecret，文件名 <code className="font-mono">{'<机器ID 32位>'}.json</code>）</span>
              </label>
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => clientFileRef.current?.click()}
                disabled={importing}
              >
                <Upload className="h-4 w-4 mr-2" />
                选择文件
              </Button>
              <input
                ref={clientFileRef}
                type="file"
                accept=".json,application/json"
                className="hidden"
                onChange={(e) => handleFile(e, setClientJson)}
              />
            </div>
            <textarea
              placeholder={placeholderClient}
              value={clientJson}
              onChange={(e) => setClientJson(e.target.value)}
              disabled={importing}
              className="flex min-h-[100px] w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50 font-mono"
            />
          </div>

          {/* 鉴权 Token 文件 */}
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <label className="text-sm font-medium">
                鉴权 Token JSON <span className="text-red-500">*</span>
                <span className="text-xs text-muted-foreground ml-1">（refreshToken / region，文件名 <code className="font-mono">kiro-auth-token.json</code>）</span>
              </label>
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => tokenFileRef.current?.click()}
                disabled={importing}
              >
                <Upload className="h-4 w-4 mr-2" />
                选择文件
              </Button>
              <input
                ref={tokenFileRef}
                type="file"
                accept=".json,application/json"
                className="hidden"
                onChange={(e) => handleFile(e, setTokenJson)}
              />
            </div>
            <textarea
              placeholder={placeholderToken}
              value={tokenJson}
              onChange={(e) => setTokenJson(e.target.value)}
              disabled={importing}
              className="flex min-h-[100px] w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50 font-mono"
            />
          </div>

          {/* 解析预览 / 错误 */}
          {parseError && (
            <div className="text-sm text-red-600 dark:text-red-400">解析失败: {parseError}</div>
          )}
          {client && token && !result && (
            <div className="text-sm text-muted-foreground space-y-1">
              <div>✓ 已识别 IdC 凭据</div>
              <div className="text-xs">Region: {token.region || '（使用全局配置）'}</div>
            </div>
          )}

          {/* 导入结果 */}
          {result && (
            <div className="border rounded-md p-3">
              <div className="flex items-start gap-3">
                {result.status === 'verifying' && (
                  <Loader2 className="w-5 h-5 animate-spin text-blue-500" />
                )}
                {result.status === 'verified' && (
                  <CheckCircle2 className="w-5 h-5 text-green-500" />
                )}
                {(result.status === 'failed' || result.status === 'duplicate') && (
                  <XCircle className="w-5 h-5 text-red-500" />
                )}
                <div className="flex-1 min-w-0">
                  <div className="text-sm font-medium">
                    {result.status === 'verifying' && '验活中...'}
                    {result.status === 'verified' && (result.email || '验活成功')}
                    {result.status === 'duplicate' && '重复凭据'}
                    {result.status === 'failed' && '验活失败'}
                  </div>
                  {result.usage && (
                    <div className="text-xs text-muted-foreground mt-1">用量: {result.usage}</div>
                  )}
                  {result.error && (
                    <div className="text-xs text-red-600 dark:text-red-400 mt-1">{result.error}</div>
                  )}
                </div>
              </div>
            </div>
          )}
        </div>

        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            onClick={() => { onOpenChange(false); resetForm() }}
            disabled={importing}
          >
            {importing ? '导入中...' : result?.status === 'verified' ? '关闭' : '取消'}
          </Button>
          {result?.status !== 'verified' && (
            <Button
              type="button"
              onClick={handleImport}
              disabled={importing || !client || !token || !!parseError}
            >
              开始导入并验活
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
