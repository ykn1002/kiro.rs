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

export function AwsSsoImportDialog({ open, onOpenChange }: AwsSsoImportDialogProps) {
  const [clientJson, setClientJson] = useState('')
  const [tokenJson, setTokenJson] = useState('')
  const [importing, setImporting] = useState(false)
  const [result, setResult] = useState<ImportResult | null>(null)

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

    // 客户端去重
    const tokenHash = await sha256Hex(parsedToken.refreshToken)
    const existing = existingCredentials?.credentials.find(c => c.refreshTokenHash === tokenHash)
    if (existing) {
      setResult({ status: 'duplicate', error: '该凭据已存在', email: existing.email })
      toast.info('该凭据已存在')
      return
    }

    setImporting(true)
    setResult({ status: 'verifying' })

    let addedCredId: number | null = null
    try {
      // 添加 idc 凭据：后端会执行 OIDC 刷新并获取 Profile ARN，即完成验活
      const addedCred = await addCredential({
        refreshToken: parsedToken.refreshToken,
        authMethod: 'idc',
        clientId: parsedClient.clientId,
        clientSecret: parsedClient.clientSecret,
        authRegion: parsedToken.region || undefined,
      })
      addedCredId = addedCred.credentialId

      // IdC 账号不支持 getUsageLimits（返回 Invalid profileArn），
      // 因此余额查询仅作为附加信息，失败不影响验活结果
      let usage: string | undefined
      try {
        await new Promise(resolve => setTimeout(resolve, 1000))
        const balance = await getCredentialBalance(addedCred.credentialId)
        usage = `${balance.currentUsage}/${balance.usageLimit}`
      } catch {
        // IdC 不支持用量查询，忽略
      }

      setResult({ status: 'verified', email: addedCred.email, usage })
      toast.success('AWS SSO 凭据导入并验活成功')
    } catch (error) {
      if (addedCredId) {
        await rollbackCredential(addedCredId)
      }
      const message = extractErrorMessage(error)
      setResult({ status: 'failed', error: message })
      toast.error('导入失败: ' + message)
    } finally {
      setImporting(false)
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
          <p className="text-sm text-muted-foreground">
            AWS IAM Identity Center（IdC/SSO）凭据由两个 JSON 文件组成：客户端注册文件
            （含 clientId / clientSecret）与鉴权 Token 文件（含 refreshToken / region）。
            请分别粘贴或上传。
          </p>

          {/* 客户端注册文件 */}
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <label className="text-sm font-medium">
                客户端注册 JSON <span className="text-red-500">*</span>
                <span className="text-xs text-muted-foreground ml-1">（clientId / clientSecret）</span>
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
                <span className="text-xs text-muted-foreground ml-1">（refreshToken / region）</span>
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
