# kiro-rs 入门使用文档（macOS / Windows）

本文从零开始，带你走完：**下载并启动 → 登录管理界面 → 用 AWS SSO 导入凭证 → 修改接入 Key → 用 CC Switch 接入 Claude Code**。

kiro-rs 是一个 Anthropic API 兼容代理：它在本机监听一个端口，把标准的 Anthropic 请求转发到上游 Kiro 服务。任何支持 Anthropic API 的客户端（Claude Code、各类 GUI）只要把接口地址指向它即可。

> 本文假设你拿到的是打好的压缩包，里面**已经包含配置文件 `config.json`**，默认管理密码为 `admin`。你无需手写配置。

---

## 目录

- [1. 压缩包内容](#1-压缩包内容)
- [2. 首次启动](#2-首次启动)
  - [macOS](#macos-启动)
  - [Windows](#windows-启动)
- [3. 登录 Web 管理界面](#3-登录-web-管理界面)
- [4. 用 AWS SSO 方式导入凭证](#4-用-aws-sso-方式导入凭证)
- [5. 修改接入 Key](#5-修改接入-key)
- [6. 用 CC Switch 接入 Claude Code](#6-用-cc-switch-接入-claude-code)
- [7. 常见问题](#7-常见问题)

---

## 1. 压缩包内容

解压后大致是这样：

```
kiro-rs/
├── kiro-rs           # macOS 可执行文件
├── kiro-rs.exe       # Windows 可执行文件（按平台，二选一）
└── config.json       # 已预置好的配置（含默认管理密码 admin）
```

`config.json` 里已经配好了监听端口、接入 Key、以及管理密码（`adminApiKey`，默认 `admin`）。凭证文件 `credentials.json` **不需要你手动创建**——从 Web 界面导入后会自动生成。

kiro-rs 默认读取**当前工作目录**下的 `config.json`，所以只要在解压目录里启动即可，无需命令行参数。

---

## 2. 首次启动

### macOS 启动

从网上下载的二进制默认没有执行权限，且会被系统 Gatekeeper 拦截。首次运行需处理一下。打开「终端」(Terminal)：

```bash
# 进入解压目录（把路径换成实际路径）
cd ~/kiro-rs

# 赋予执行权限（只需一次）
chmod +x kiro-rs

# 解除 Gatekeeper 隔离，否则会报"无法打开，因为无法验证开发者"（只需一次）
xattr -d com.apple.quarantine kiro-rs

# 启动
./kiro-rs
```

### Windows 启动

打开解压后的文件夹，在地址栏输入 `powershell` 回车（或右键「在终端中打开」），然后：

```powershell
.\kiro-rs.exe
```

首次运行如果弹出 SmartScreen 蓝色提示，点「更多信息」→「仍要运行」。

### 启动成功的样子

终端里会打印监听地址和路由，类似：

```
已加载 0 个凭据配置
监听地址: 127.0.0.1:8990
  POST /v1/messages
  ...
```

看到 `监听地址` 就说明起来了。`已加载 0 个凭据` 是正常的——下一步就去导入。**保持这个终端窗口开着**，关掉服务就停了。

---

## 3. 登录 Web 管理界面

1. 浏览器打开：`http://127.0.0.1:8990/admin`
2. 页面会要求输入 **Admin API Key**，输入默认密码 `admin` 登录。

> 这个密码就是 `config.json` 里的 `adminApiKey` 字段。想改的话，编辑 `config.json` 把 `adminApiKey` 换成别的值，重启服务即可。

登录后进入管理面板，可以看到凭证列表（现在是空的）、添加凭证、设置等入口。

---

## 4. 用 AWS SSO 方式导入凭证

kiro-rs 支持导入 AWS IAM Identity Center（也叫 IdC / SSO）的凭证。它由**两个 JSON 文件**组成，都在你本机的 SSO 缓存目录里：

| 文件 | 位置 | 内容 |
|------|------|------|
| 客户端注册文件 | `<机器ID，32位十六进制>.json` | `clientId` / `clientSecret` |
| 鉴权 Token 文件 | `kiro-auth-token.json` | `refreshToken` / `region` |

SSO 缓存目录路径：

- **macOS**：`/Users/<你的用户名>/.aws/sso/cache/`
- **Windows**：`C:\Users\<你的用户名>\.aws\sso\cache\`

> 提示：客户端注册文件的文件名是一串 32 位十六进制（机器 ID），不是固定名字；`kiro-auth-token.json` 则是固定名字。两个文件都在同一个 `cache` 目录里。

### 导入步骤

1. 在管理面板里找到「**AWS SSO 导入**」（标题为「AWS SSO 导入（自动验活）」的对话框）。
2. 准备两个文件的内容，有两种填法：
   - **上传文件**：点每个输入框旁的「选择文件」，分别选中对应的 `.json` 文件；或
   - **粘贴内容**：直接把两个 JSON 文件的文本粘进对应输入框。
3. 上半部分填「**客户端注册 JSON**」（含 `clientId` / `clientSecret`）。
4. 下半部分填「**鉴权 Token JSON**」（含 `refreshToken` / `region`）。
5. 两个都填好后，界面会显示「✓ 已识别 IdC 凭据」和识别到的 Region。
6. 点「**开始导入并验活**」。

后台会用这份凭证做一次 OIDC 刷新并获取 Profile ARN，相当于自动验活：

- 成功 → 显示绿色对勾和账号信息，凭证已保存（自动写入 `credentials.json`）。
- 重复 → 提示「该凭据已存在」（按 refreshToken 去重）。
- 失败 → 显示红色错误信息，并自动回滚（不会留下无效凭证）。

导入成功后，凭证列表里就能看到这条凭证了。多个账号可以重复以上步骤逐个导入，程序会自动做负载分担和失败切换。

---

## 5. 修改接入 Key

「接入 Key」是**你的客户端调用 kiro-rs 时用的密钥**（对应 `config.json` 里的 `apiKey`），和上一步导入的上游凭证是两回事。压缩包里已经预置了一个，你可以在 Web 界面里改成自己的。

1. 在管理面板里打开「**设置**」。
2. 找到「**客户端 API Key**」一栏（输入框旁有个眼睛图标可切换明文显示）。
3. 改成你想要的值（建议用 `sk-` 开头的长随机串），不能留空。
4. 保存。修改会**热生效**并自动回写到 `config.json`，无需重启。

> 注意区分两个 Key：
> - **接入 Key（`apiKey`）**：客户端用，可在这里改。
> - **管理密码（`adminApiKey`，默认 `admin`）**：登录管理界面用，这里改不了，需编辑 `config.json` 后重启。

改完记得在客户端 / CC Switch 那边同步更新成新的接入 Key。

---

## 6. 用 CC Switch 接入 Claude Code

[CC Switch](https://github.com/farion1231/cc-switch)（CC 供应商切换器）是一个管理 Claude Code 多个 API 供应商、一键切换的 GUI 工具。它的本质是帮你改写 Claude Code 使用的接口地址和密钥。我们把 kiro-rs 添加成一个「供应商」即可。

### 需要的两个值

| CC Switch 字段 | 填什么 |
|----------------|--------|
| Base URL / 接口地址 | `http://127.0.0.1:8990` |
| API Key / 密钥 | 你的**接入 Key**（`config.json` 的 `apiKey`，或第 5 步改后的值） |

> 建议 Claude Code 用 kiro-rs 的 `/cc/v1` 端点（缓冲模式，`input_tokens` 更准）。如果 CC Switch 允许填带子路径的地址，可填 `http://127.0.0.1:8990/cc`；只接受根地址就填 `http://127.0.0.1:8990`（走 `/v1`），一样能用。

### 操作步骤

1. 打开 CC Switch，新增一个供应商配置（Provider）。
2. 名称随便起，比如 `kiro-rs (本地)`。
3. Base URL 填 `http://127.0.0.1:8990`（或 `http://127.0.0.1:8990/cc`）。
4. API Key 填你的接入 Key。
5. 保存后，在 CC Switch 里**切换**到这个供应商（点一下让它生效）。
6. CC Switch 会把配置写进 Claude Code，等价于设置了这两个环境变量：
   - `ANTHROPIC_BASE_URL=http://127.0.0.1:8990`
   - `ANTHROPIC_AUTH_TOKEN=<你的接入 Key>`
7. **重启 Claude Code**（关掉重开终端会话）让配置生效。
8. 在 Claude Code 里正常提问，请求会经过 kiro-rs → 上游 Kiro。

### 不用 CC Switch 的手动等效方式

CC Switch 只是帮你设环境变量，也可以手动设。启动 Claude Code 前：

**macOS（终端）：**

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8990
export ANTHROPIC_AUTH_TOKEN=你的接入Key
claude
```

**Windows（PowerShell）：**

```powershell
$env:ANTHROPIC_BASE_URL="http://127.0.0.1:8990"
$env:ANTHROPIC_AUTH_TOKEN="你的接入Key"
claude
```

切回官方 Anthropic：在 CC Switch 里切回官方供应商，或清掉上面两个环境变量。

---

## 7. 常见问题

**Q：macOS 报「已损坏，无法打开」或「无法验证开发者」？**
执行 `xattr -d com.apple.quarantine kiro-rs`（见第 2 步）。这是系统对下载文件的隔离标记。

**Q：管理界面打不开 / 登录不了？**
确认服务已启动（终端有「监听地址」）、地址是 `http://127.0.0.1:8990/admin`、密码是 `config.json` 里的 `adminApiKey`（默认 `admin`）。

**Q：AWS SSO 导入时找不到那两个文件？**
去 SSO 缓存目录 `~/.aws/sso/cache/`（Windows 是 `C:\Users\<用户名>\.aws\sso\cache\`）翻。里面 `kiro-auth-token.json` 是鉴权 Token 文件；文件名为一长串十六进制的 `.json` 是客户端注册文件。如果目录为空，说明本机还没用 AWS SSO 登录过。

**Q：导入提示验活失败？**
常见原因：两个文件对不上（不是同一次登录产生的）、refreshToken 已过期、或网络到不了上游。重新在本机做一次 SSO 登录拿到新文件再导。

**Q：客户端请求返回 401 / 认证失败？**
客户端填的密钥和 `config.json` 里的**接入 Key（`apiKey`）**不一致。别把管理密码或上游凭证当接入 Key 填。

**Q：请求 429 / 提示额度或频率超限？**
上游有 RPM 频率限制。可在「设置」里调 RPM 相关字段，或导入多个凭证做负载分担。

**Q：想让局域网内其他设备也能连？**
编辑 `config.json` 把 `host` 从 `127.0.0.1` 改成 `0.0.0.0`，重启。其他设备用本机局域网 IP 访问，如 `http://192.168.x.x:8990`。此时接口对局域网开放，务必把接入 Key 和管理密码都设强一点。
