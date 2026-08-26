import { useEffect, useRef, useState } from 'react'
import {
  Activity,
  CheckCircle2,
  XCircle,
  Timer,
  Unplug,
  RotateCw,
  FileWarning,
  Server,
  BarChart3,
} from 'lucide-react'
import { Card } from '@/components/ui/card'
import { useMetrics } from '@/hooks/use-credentials'
import type { MetricsResponse, ModelStat } from '@/types/api'

// token 大数简写：1234 → 1.2k，1234567 → 1.23M
function formatTokens(n: number): string {
  if (n < 1000) return String(n)
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`
  return `${(n / 1_000_000).toFixed(2)}M`
}

// credits 费用格式化：小数保留 2-3 位，大数简写
function formatCredits(n: number): string {
  if (n === 0) return '0'
  if (n < 1) return n.toFixed(3)
  if (n < 1000) return n.toFixed(2)
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}k`
  return `${(n / 1_000_000).toFixed(2)}M`
}

// 累计计数器的字段名（用于计算轮询增量）
type CounterKey =
  | 'requestsSuccess'
  | 'requestsError'
  | 'upstreamRateLimited'
  | 'streamInterrupted'
  | 'streamRestarted'
  | 'streamDecodeFailures'

function formatUptime(seconds: number): string {
  if (seconds < 60) return `${seconds} 秒`
  const m = Math.floor(seconds / 60)
  if (m < 60) return `${m} 分钟`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h} 小时 ${m % 60} 分`
  const d = Math.floor(h / 24)
  return `${d} 天 ${h % 24} 小时`
}

// 纯 SVG 成功率环形
function SuccessRing({ rate, total }: { rate: number; total: number }) {
  const size = 92
  const stroke = 7
  const r = (size - stroke) / 2
  const c = 2 * Math.PI * r
  const pct = Number.isFinite(rate) ? rate : 0
  const offset = c * (1 - pct / 100)
  const color =
    total === 0
      ? 'hsl(var(--muted-foreground))'
      : pct >= 95
        ? 'hsl(var(--success))'
        : pct >= 80
          ? 'hsl(var(--warning))'
          : 'hsl(var(--destructive))'

  return (
    <div className="relative shrink-0" style={{ width: size, height: size }}>
      <svg width={size} height={size} className="-rotate-90">
        <circle
          cx={size / 2}
          cy={size / 2}
          r={r}
          fill="none"
          stroke="hsl(var(--muted))"
          strokeWidth={stroke}
        />
        <circle
          cx={size / 2}
          cy={size / 2}
          r={r}
          fill="none"
          stroke={color}
          strokeOpacity={0.85}
          strokeWidth={stroke}
          strokeLinecap="round"
          strokeDasharray={c}
          strokeDashoffset={offset}
          className="transition-all duration-500"
        />
      </svg>
      <div className="absolute inset-0 flex flex-col items-center justify-center">
        <span className="text-lg font-bold tabular-nums">
          {total === 0 ? '—' : `${pct.toFixed(0)}%`}
        </span>
        <span className="text-[10px] text-muted-foreground">成功率</span>
      </div>
    </div>
  )
}

interface CounterCardProps {
  icon: React.ComponentType<{ className?: string }>
  label: string
  value: number
  delta: number
  tone: 'success' | 'destructive' | 'warning' | 'info' | 'muted'
}

const toneClass: Record<CounterCardProps['tone'], string> = {
  success: 'text-success',
  destructive: 'text-destructive',
  warning: 'text-warning',
  info: 'text-info',
  muted: 'text-muted-foreground',
}

// 紧凑统计条：一行内 图标+标签+数值；0 值整体弱化，非 0 才用语义色
function CounterCard({ icon: Icon, label, value, delta, tone }: CounterCardProps) {
  const active = value > 0
  return (
    <div className="flex items-center gap-2.5 rounded-lg border bg-card px-3 py-2">
      <Icon className={`h-4 w-4 shrink-0 ${active ? toneClass[tone] : 'text-muted-foreground/50'}`} />
      <span className="min-w-0 truncate text-xs text-muted-foreground">{label}</span>
      <span className="ml-auto flex items-baseline gap-1">
        <span className={`text-base font-semibold tabular-nums ${active ? '' : 'text-muted-foreground/60'}`}>
          {value.toLocaleString()}
        </span>
        {delta > 0 && (
          <span className={`text-[10px] font-medium ${toneClass[tone]}`}>+{delta}</span>
        )}
      </span>
    </div>
  )
}

export function MonitorPanel() {
  const { data, isLoading, error } = useMetrics()
  // 保存上一次快照，用于计算轮询间的增量
  const prevRef = useRef<MetricsResponse | null>(null)
  const [deltas, setDeltas] = useState<Record<CounterKey, number>>({
    requestsSuccess: 0,
    requestsError: 0,
    upstreamRateLimited: 0,
    streamInterrupted: 0,
    streamRestarted: 0,
    streamDecodeFailures: 0,
  })

  useEffect(() => {
    if (!data) return
    const prev = prevRef.current
    if (prev) {
      const keys: CounterKey[] = [
        'requestsSuccess',
        'requestsError',
        'upstreamRateLimited',
        'streamInterrupted',
        'streamRestarted',
        'streamDecodeFailures',
      ]
      const next = {} as Record<CounterKey, number>
      for (const k of keys) {
        // 进程重启会让计数归零，负增量归 0 做保护
        next[k] = Math.max(0, data[k] - prev[k])
      }
      setDeltas(next)
    }
    prevRef.current = data
  }, [data])

  if (error) {
    return (
      <Card className="p-4 text-sm text-destructive">
        监控数据加载失败：{(error as Error).message}
      </Card>
    )
  }

  const totalReq = data ? data.requestsSuccess + data.requestsError : 0
  const successRate = totalReq > 0 ? (data!.requestsSuccess / totalReq) * 100 : 0

  return (
    <section className="space-y-3">
      <div className="flex items-center gap-2">
        <Activity className="h-4 w-4 text-info" />
        <h2 className="text-sm font-semibold">实时监控</h2>
        {isLoading && !data && (
          <span className="text-xs text-muted-foreground">加载中...</span>
        )}
        <span className="ml-auto text-xs text-muted-foreground">每 4 秒刷新</span>
      </div>

      <div className="grid items-stretch gap-3 lg:grid-cols-[minmax(280px,340px)_1fr]">
        {/* 概览：成功率环 + 关键指标 */}
        <Card className="flex items-center gap-4 p-4">
          <SuccessRing rate={successRate} total={totalReq} />
          <div className="space-y-2.5">
            <div className="flex items-center gap-2 text-sm">
              <Server className="h-3.5 w-3.5 text-info" />
              <span className="text-muted-foreground">可用凭据</span>
              <span className="ml-auto font-semibold tabular-nums">
                {data?.credentialsAvailable ?? '—'} / {data?.credentialsTotal ?? '—'}
              </span>
            </div>
            <div className="flex items-center gap-2 text-sm">
              <CheckCircle2 className="h-3.5 w-3.5 text-success" />
              <span className="text-muted-foreground">累计成功</span>
              <span className="ml-auto font-semibold tabular-nums">
                {data?.requestsSuccess.toLocaleString() ?? '—'}
              </span>
            </div>
            <div className="flex items-center gap-2 text-sm">
              <Timer className="h-3.5 w-3.5 text-muted-foreground" />
              <span className="text-muted-foreground">运行时长</span>
              <span className="ml-auto font-semibold tabular-nums">
                {data ? formatUptime(data.uptimeSeconds) : '—'}
              </span>
            </div>
          </div>
        </Card>

        {/* 细分计数（紧凑条，含轮询增量） */}
        <div className="grid grid-cols-1 content-center gap-2 sm:grid-cols-2 xl:grid-cols-3">
          <CounterCard
            icon={XCircle}
            label="请求失败"
            value={data?.requestsError ?? 0}
            delta={deltas.requestsError}
            tone="destructive"
          />
          <CounterCard
            icon={Timer}
            label="上游 429"
            value={data?.upstreamRateLimited ?? 0}
            delta={deltas.upstreamRateLimited}
            tone="warning"
          />
          <CounterCard
            icon={Unplug}
            label="断流中断"
            value={data?.streamInterrupted ?? 0}
            delta={deltas.streamInterrupted}
            tone="destructive"
          />
          <CounterCard
            icon={RotateCw}
            label="断流重连"
            value={data?.streamRestarted ?? 0}
            delta={deltas.streamRestarted}
            tone="info"
          />
          <CounterCard
            icon={FileWarning}
            label="解码失败"
            value={data?.streamDecodeFailures ?? 0}
            delta={deltas.streamDecodeFailures}
            tone="muted"
          />
        </div>
      </div>

      {/* 各模型统计表 */}
      <ModelStatsTable models={data?.models ?? []} />
    </section>
  )
}

// 各模型累计调用 + token + 实时 RPM 表
function ModelStatsTable({ models }: { models: ModelStat[] }) {
  return (
    <Card className="p-4">
      <div className="mb-3 flex items-center gap-2">
        <BarChart3 className="h-4 w-4 text-info" />
        <h3 className="text-sm font-semibold">模型统计</h3>
        <span className="ml-auto text-xs text-muted-foreground">今日 / 累计 · 实时 RPM 为近 60 秒</span>
      </div>
      {models.length === 0 ? (
        <div className="py-6 text-center text-sm text-muted-foreground">暂无调用记录</div>
      ) : (
        // 默认显示约 3 行，超过纵向滚动；两级表头 sticky 固定
        <div className="max-h-[140px] overflow-y-auto overflow-x-auto">
          <table className="w-full table-fixed text-sm">
            {/* 显式列宽，保证两级表头与数据列边界一致 */}
            <colgroup>
              <col className="w-[22%]" />
              <col className="w-[10%]" />
              <col className="w-[12%]" />
              <col className="w-[12%]" />
              <col className="w-[10%]" />
              <col className="w-[12%]" />
              <col className="w-[12%]" />
              <col className="w-[10%]" />
            </colgroup>
            <thead>
              {/* 分组表头：今日 | 累计 | 实时（sticky 第一行） */}
              <tr className="text-[11px] text-muted-foreground">
                <th className="sticky top-0 z-10 bg-card pb-1" />
                <th className="sticky top-0 z-10 border-l bg-card pb-1 px-3 text-center font-medium" colSpan={3}>
                  今日
                </th>
                <th className="sticky top-0 z-10 border-l bg-card pb-1 px-3 text-center font-medium" colSpan={3}>
                  累计
                </th>
                <th className="sticky top-0 z-10 border-l bg-card pb-1 pl-3 text-center font-medium">实时</th>
              </tr>
              <tr className="text-xs text-muted-foreground">
                <th className="sticky top-[22px] z-10 border-b bg-card py-1 pr-3 text-left font-medium">模型</th>
                <th className="sticky top-[22px] z-10 border-b border-l bg-card py-1 px-3 text-right font-medium">调用</th>
                <th className="sticky top-[22px] z-10 border-b bg-card py-1 px-3 text-right font-medium">token</th>
                <th className="sticky top-[22px] z-10 border-b bg-card py-1 px-3 text-right font-medium">费用</th>
                <th className="sticky top-[22px] z-10 border-b border-l bg-card py-1 px-3 text-right font-medium">调用</th>
                <th className="sticky top-[22px] z-10 border-b bg-card py-1 px-3 text-right font-medium">token</th>
                <th className="sticky top-[22px] z-10 border-b bg-card py-1 px-3 text-right font-medium">费用</th>
                <th className="sticky top-[22px] z-10 border-b border-l bg-card py-1 pl-3 text-right font-medium">RPM</th>
              </tr>
            </thead>
            <tbody>
              {models.map((m) => (
                <tr key={m.model} className="border-b last:border-0 hover:bg-muted/40">
                  <td className="truncate py-1 pr-3 font-medium" title={m.model}>
                    {m.model}
                  </td>
                  {/* 今日 */}
                  <td className="border-l py-1 px-3 text-right tabular-nums">
                    {m.todayRequests > 0 ? (
                      m.todayRequests.toLocaleString()
                    ) : (
                      <span className="text-muted-foreground/50">0</span>
                    )}
                  </td>
                  <td
                    className="py-1 px-3 text-right tabular-nums"
                    title={m.todayTotalTokens.toLocaleString()}
                  >
                    {m.todayTotalTokens > 0 ? (
                      formatTokens(m.todayTotalTokens)
                    ) : (
                      <span className="text-muted-foreground/50">0</span>
                    )}
                  </td>
                  <td
                    className="py-1 px-3 text-right tabular-nums"
                    title={`${m.todayCredits} credits`}
                  >
                    {m.todayCredits > 0 ? (
                      <span className="font-medium text-[#a855f7]">
                        {formatCredits(m.todayCredits)}
                      </span>
                    ) : (
                      <span className="text-muted-foreground/50">0</span>
                    )}
                  </td>
                  {/* 累计 */}
                  <td className="border-l py-1 px-3 text-right tabular-nums text-muted-foreground">
                    {m.requests.toLocaleString()}
                  </td>
                  <td
                    className="py-1 px-3 text-right tabular-nums text-muted-foreground"
                    title={m.totalTokens.toLocaleString()}
                  >
                    {formatTokens(m.totalTokens)}
                  </td>
                  <td
                    className="py-1 px-3 text-right tabular-nums text-muted-foreground"
                    title={`${m.credits} credits`}
                  >
                    {m.credits > 0 ? formatCredits(m.credits) : (
                      <span className="text-muted-foreground/50">0</span>
                    )}
                  </td>
                  {/* 实时 RPM */}
                  <td className="border-l py-1 pl-3 text-right tabular-nums">
                    {m.rpm > 0 ? (
                      <span className="font-medium text-success">{m.rpm}</span>
                    ) : (
                      <span className="text-muted-foreground/50">0</span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Card>
  )
}
