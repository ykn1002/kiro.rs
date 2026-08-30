# kiro-rs 桌面版（Tauri）打包脚本 —— Windows / PowerShell
#
# 用法（在仓库根目录 PowerShell 中）:
#   .\build-desktop.ps1                    # 打当前架构（x64），nsis+msi
#   .\build-desktop.ps1 -Bundles nsis      # 只出 nsis 安装包
#   .\build-desktop.ps1 -Bundles msi       # 只出 msi
#   .\build-desktop.ps1 -SkipFrontend      # admin-ui\dist 已最新时跳过前端构建
#   .\build-desktop.ps1 -Rustls            # 改用 rustls（默认 native-tls）
#
# 说明:
#   - 前端 admin-ui 会先构建（rust-embed 需要 admin-ui\dist），除非 -SkipFrontend。
#   - 默认 native-tls（Windows 走 SChannel，用系统证书栈，代理/证书更稳；vendored 静态链接无需 OpenSSL 环境）。
#   - 需要已安装 Rust、Node/pnpm、以及 tauri CLI：
#       cargo install tauri-cli --version "^2" --locked
#   - 产物位于 src-tauri\target\release\bundle\{nsis,msi}\

param(
    [string]$Bundles = "nsis,msi",
    [switch]$SkipFrontend,
    [switch]$Rustls
)

$ErrorActionPreference = "Stop"
Set-Location -Path $PSScriptRoot

# ---- 依赖检查 ----
function Require-Cmd($name) {
    if (-not (Get-Command $name -ErrorAction SilentlyContinue)) {
        Write-Error "错误: 未找到 $name 命令"
        exit 1
    }
}
Require-Cmd pnpm
Require-Cmd cargo
try { cargo tauri --version | Out-Null } catch {
    Write-Error "错误: 未找到 tauri CLI，请先安装：cargo install tauri-cli --version '^2' --locked"
    exit 1
}

# ---- 前端构建（rust-embed 依赖 admin-ui\dist）----
if ($SkipFrontend) {
    Write-Host "==> 跳过前端构建（-SkipFrontend）"
    if (-not (Test-Path "admin-ui\dist\index.html")) {
        Write-Error "错误: admin-ui\dist 不存在，不能跳过前端构建"
        exit 1
    }
} else {
    Write-Host "==> 构建前端 admin-ui"
    Push-Location admin-ui
    pnpm install
    pnpm build
    Pop-Location
}

# ---- TLS feature ----
if ($Rustls) {
    Write-Host "==> TLS: rustls"
    $featureArgs = @("--no-default-features", "--features", "rustls")
} else {
    Write-Host "==> TLS: native-tls（默认）"
    $featureArgs = @("--no-default-features", "--features", "native-tls")
}

# ---- 打包 ----
Write-Host "==> 打包桌面版（bundles: $Bundles）"
cargo tauri build --bundles $Bundles -- @featureArgs

# ---- 产物提示 ----
Write-Host ""
Write-Host "==> 打包完成，产物位于:"
Get-ChildItem -Path "src-tauri\target\release\bundle" -Recurse -Include *.exe,*.msi -ErrorAction SilentlyContinue |
    ForEach-Object { Write-Host "  $($_.FullName)" }
