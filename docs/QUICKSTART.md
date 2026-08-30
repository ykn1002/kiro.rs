# 快速开始（从源码构建）

面向从源码构建 kiro-rs。若你拿到的是打包好的二进制 / 桌面版，直接看 [入门文档](GETTING_STARTED.md)；配置与接口详解见 [配置与接口参考](CONFIGURATION.md)，项目介绍见 [README](../README.md)。

## 1. 编译

> 不想编译可直接前往 Release 下载二进制文件。

> **前置步骤**：编译前需先构建前端 Admin UI（会嵌入到二进制中）：
> ```bash
> cd admin-ui && pnpm install && pnpm build
> ```

```bash
cargo build --release
```

## 2. 最小配置

创建 `config.json`：

```json
{
   "host": "127.0.0.1",
   "port": 8990,
   "apiKey": "sk-kiro-rs-qazWSXedcRFV123456",
   "region": "us-east-1"
}
```

> 需要 Web 管理面板请配置 `adminApiKey`。凭据可在面板里导入，跳过手写 `credentials.json`。

创建 `credentials.json`（从 Kiro IDE 等获取凭证；对凭据地域有疑惑见 [Region 配置](CONFIGURATION.md#region-配置)）：

Social 认证：
```json
{
   "refreshToken": "你的刷新token",
   "expiresAt": "2025-12-31T02:32:45.144Z",
   "authMethod": "social"
}
```

IdC 认证：
```json
{
   "refreshToken": "你的刷新token",
   "expiresAt": "2025-12-31T02:32:45.144Z",
   "authMethod": "idc",
   "clientId": "你的clientId",
   "clientSecret": "你的clientSecret"
}
```

字段完整说明见 [credentials.json](CONFIGURATION.md#credentialsjson)。

## 3. 启动

```bash
./target/release/kiro-rs
```

或指定配置文件路径：

```bash
./target/release/kiro-rs -c /path/to/config.json --credentials /path/to/credentials.json
```

## 4. 验证

```bash
curl http://127.0.0.1:8990/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: sk-kiro-rs-qazWSXedcRFV123456" \
  -d '{
    "model": "claude-sonnet-4-20250514",
    "max_tokens": 1024,
    "stream": true,
    "messages": [
      {"role": "user", "content": "Hello, Claude!"}
    ]
  }'
```

## Docker

```bash
docker-compose up
```

需要将 `config.json` 和 `credentials.json` 挂载到容器中，具体参见 `docker-compose.yml`。
