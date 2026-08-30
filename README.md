<div align="center">

# kiro-rs 加强版

**Rust 编写的 Kiro API 代理加强版 — 兼容 Anthropic 与 OpenAI / Codex，多凭据调度、断流处理，内置 Web 管理面板，并新增开箱即用的桌面版**

[![Build](https://github.com/ykn1002/kiro.rs/actions/workflows/build.yaml/badge.svg)](https://github.com/ykn1002/kiro.rs/actions/workflows/build.yaml) [![Release](https://img.shields.io/github/v/release/ykn1002/kiro.rs?display_name=tag&sort=semver)](https://github.com/ykn1002/kiro.rs/releases) [![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE) [![Rust](https://img.shields.io/badge/Rust-stable-orange.svg?logo=rust&logoColor=white)](https://www.rust-lang.org) ![Platforms](https://img.shields.io/badge/desktop-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

[配置与接口参考](docs/CONFIGURATION.md) · [入门文档](docs/GETTING_STARTED.md) · [桌面版引导](docs/onboarding/桌面版新手引导.md)

</div>

把标准的 Anthropic `/v1/messages` 请求转换为上游 Kiro API 调用，并将 AWS event-stream 响应转回 Anthropic SSE 流式返回；同时提供 OpenAI / Codex 兼容端点、内置 Web 管理面板与桌面版。

## 功能特性

- **断流不白扣配额** — 上游断流时客户端不再整轮重试：首帧前**透明重放**（仅 Anthropic 端点，客户端无感知），中途断流按各协议原生「截断」语义**优雅收尾**（`max_tokens` / `incomplete` / `length`）让模型续写。Kiro 按请求次数计费，这一处直接省配额。
- **桌面版开箱即用** — Tauri 托盘应用，AWS SSO **一键导入**凭证、**免登录**直达管理面板、关窗**自动释放内存**，全程零命令行、零手写配置。→ [桌面版](#桌面版)
- **多凭据自动调度** — 多账号凭据池，三种负载均衡（`优先级` / `均衡` / `轮询`）叠加按模型（Opus/Sonnet/Haiku）RPM 限流、智能重试与故障转移，单账号额度耗尽自动切换，Token 自动刷新回写。
- **双生态兼容** — 同时对接 Anthropic（`/v1/messages`，Claude Code 专用 `/cc/v1` 校正 `input_tokens` 并处理工具参数截断）与 OpenAI / Codex（`/v1/chat/completions`、`/v1/responses`）客户端，模型名自动映射与别名。
- **Thinking / 工具调用 / WebSearch** — 支持 extended thinking、function calling（tool use），内置 WebSearch 工具转换。
- **多模型支持** — 模型表由配置驱动，内置默认表涵盖 Opus 4.5–5、Sonnet 4.5-5、Haiku 4.5，支持自定义与别名映射。
- **Admin 管理** — 可选的 Web 管理界面与 API：凭据管理、余额查询、实时监控、配置热更新。
- **可观测性** — 内置 `/healthz`、`/readyz` 探针与 Prometheus `/metrics`。

> 完整配置项、API 端点、模型映射、Admin 接口、项目结构见 [配置与接口参考](docs/CONFIGURATION.md)。

## 桌面版

基于 Tauri 2 的桌面壳（macOS / Windows / Linux），把代理服务与 Web 管理面板打包成一个托盘应用，开箱即用，无需手写配置或使用命令行。

<div align="center">
  <img src="https://cdn.jsdelivr.net/gh/ykn1002/kiro.rs@master/docs/onboarding/img/04-monitor.png" alt="桌面版实时监控看板：可用凭据数与调用统计" width="760">
</div>

- **托盘常驻**: 关窗不退出进程，托盘菜单提供「显示窗口 / 开机启动 / 静默启动 / 退出」
- **免登录管理面板**: 启动后自动打开内置 Admin UI 并注入密钥，直达主界面
- **AWS SSO 一键导入**: 自动扫描本机 SSO 缓存、逐条验活后导入 Kiro 凭证（也支持粘贴 / 选文件导入完整 `config.json`）
- **轻量模式（默认开）**: 关窗后延迟销毁 WebView 释放内存（常驻约 160MB → 几十 MB），托盘/Dock 再唤出时秒重建；`lightweight_minutes=0` 表示立即释放。macOS 下进入后台会降级为纯托盘应用（Dock 图标隐藏）
- **静默启动**: 开机自启时不弹窗、仅驻留托盘（需同时开启「开机启动」与「静默启动」；配合轻量模式可不建窗口直接进后台）
- **端口自动回退**: 期望端口被占用时自动改用可用端口，并在面板中提示
- **更新检查**: 对比 GitHub 最新 release 提示新版本
- **日志面板**: 内置内存日志缓冲，管理面板可实时查看

桌面版图文引导见 [桌面版新手引导](docs/onboarding/桌面版新手引导.md)。

## 注意事项

- **TLS 后端**：默认 `native-tls`（对代理 / token 刷新更稳）。若改用 rustls 后遇到无法刷新 token、`error request` 等报错，切回 `native-tls` 通常即可解决（详见 [配置说明](docs/CONFIGURATION.md#configjson)）。

## 文档

- [配置与接口参考](docs/CONFIGURATION.md) — 完整配置项、API 端点、模型映射、Admin 接口、项目结构
- [入门文档](docs/GETTING_STARTED.md) — 打包版从下载到接入 Claude Code 的全流程
- [快速开始](docs/QUICKSTART.md) — 从源码构建、最小配置、启动与验证
- [桌面版新手引导](docs/onboarding/桌面版新手引导.md) — 桌面版图文引导

## 技术栈

- **Web 框架**: [Axum](https://github.com/tokio-rs/axum) 0.8
- **异步运行时**: [Tokio](https://tokio.rs/)
- **HTTP 客户端**: [Reqwest](https://github.com/seanmonstar/reqwest)
- **序列化**: [Serde](https://serde.rs/)
- **日志**: [tracing](https://github.com/tokio-rs/tracing)
- **命令行**: [Clap](https://github.com/clap-rs/clap)
- **桌面壳**: [Tauri 2](https://tauri.app/)

## 免责声明

本项目仅供研究使用, Use at your own risk, 使用本项目所导致的任何后果由使用人承担, 与本项目无关。
本项目与 AWS/KIRO/Anthropic/Claude 等官方无关, 本项目不代表官方立场。

## License

MIT

## 致谢

本项目的实现离不开前辈的努力:  
 - [kiro2api](https://github.com/caidaoli/kiro2api)
 - [proxycast](https://github.com/aiclientproxy/proxycast)

本项目部分逻辑参考了以上的项目, 再次由衷的感谢!
