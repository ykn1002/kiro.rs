interface MachineIdHintProps {
  /** 全局 config 还是单条凭据 */
  scope?: 'global' | 'credential'
}

export function MachineIdHint({ scope = 'global' }: MachineIdHintProps) {
  return (
    <div className="space-y-1.5 text-xs text-muted-foreground">
      <p>
        从<strong className="font-medium text-foreground/80">已安装 Kiro IDE 的本机</strong>
        读取设备 ID（多为 UUID，原样粘贴即可；也支持 64 位十六进制）：
      </p>
      <ul className="list-disc space-y-0.5 pl-4 font-mono text-[11px] leading-relaxed">
        <li>
          macOS:{' '}
          <span className="break-all">
            ~/Library/Application Support/Kiro/machineid
          </span>
        </li>
        <li>
          Windows:{' '}
          <span className="break-all">%APPDATA%\Kiro\machineId</span>
          <span className="font-sans text-muted-foreground">
            {' '}
            （例：C:\Users\你\AppData\Roaming\Kiro\machineId）
          </span>
        </li>
        <li>
          Linux:{' '}
          <span className="break-all">~/.config/Kiro/machineid</span>
        </li>
      </ul>
      <p>
        命令行示例：macOS / Linux 执行{' '}
        <code className="rounded bg-muted px-1 py-0.5 font-mono text-[11px]">
          cat &quot;~/Library/Application Support/Kiro/machineid&quot;
        </code>
        ；Windows PowerShell 执行{' '}
        <code className="rounded bg-muted px-1 py-0.5 font-mono text-[11px]">
          Get-Content $env:APPDATA\Kiro\machineId
        </code>
        。若文件不存在，可在 Kiro 里发一条对话后抓包，从请求头{' '}
        <code className="rounded bg-muted px-1 py-0.5 font-mono text-[11px]">
          User-Agent
        </code>{' '}
        的 <code className="font-mono text-[11px]">KiroIDE-版本-</code> 后缀获取。
      </p>
      {scope === 'global' ? (
        <p>
          填在此处为<strong className="font-medium text-foreground/80">全局默认</strong>
          ；新增凭据未单独填写时使用。留空并保存会清除全局值。
        </p>
      ) : (
        <p>
          留空则使用系统设置中的<strong className="font-medium text-foreground/80">全局 machineId</strong>
          ；全局也未配置时，会按 refreshToken 派生（与真 Kiro 设备 ID 不同）。
        </p>
      )}
      <p>凭据级 machineId 优先于全局。仅影响后续上游请求与 Token 刷新的 User-Agent 指纹。</p>
    </div>
  )
}
