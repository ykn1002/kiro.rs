import { useMemo, useState } from 'react'
import {
  ComposedChart,
  Line,
  Area,
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
  Cell,
} from 'recharts'
import { TrendingUp, PieChart as PieIcon, Server, Coins } from 'lucide-react'
import { Card } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { useTimeseries, useCredentials } from '@/hooks/use-credentials'
import type { TimeBucket, DimBucket } from '@/types/api'

// 图例配色（recharts 的 SVG stroke 无法解析 CSS 变量，用固定 hex）
const COLORS = {
  input: '#3b82f6', // 蓝：输入
  output: '#10b981', // 绿：输出
  cacheWrite: '#f59e0b', // 橙：缓存写
  cacheRead: '#06b6d4', // 青：缓存读
  credits: '#a855f7', // 紫：费用（credits）
}

// 按模型/凭据分布饼图/柱图的循环配色
const DIST_COLORS = [
  '#3b82f6',
  '#10b981',
  '#f59e0b',
  '#a855f7',
  '#06b6d4',
  '#ef4444',
  '#8b5cf6',
  '#ec4899',
]

type RangePreset = '24h' | '7d' | '30d'

// token 大数简写
function formatTokens(n: number): string {
  if (n < 1000) return String(n)
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`
  return `${(n / 1_000_000).toFixed(2)}M`
}

// credits 消耗格式化：小数保留 2-3 位，大数简写
function formatCredits(n: number): string {
  if (n === 0) return '0'
  if (n < 1) return n.toFixed(3)
  if (n < 1000) return n.toFixed(2)
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}k`
  return `${(n / 1_000_000).toFixed(2)}M`
}

// Unix 秒 → 本地 date input 值（YYYY-MM-DD）
function toDateInput(unixSec: number): string {
  const d = new Date(unixSec * 1000)
  const y = d.getFullYear()
  const m = String(d.getMonth() + 1).padStart(2, '0')
  const day = String(d.getDate()).padStart(2, '0')
  return `${y}-${m}-${day}`
}

// date input 值 → 当日 00:00 的 Unix 秒
function fromDateInput(value: string): number {
  return Math.floor(new Date(`${value}T00:00:00`).getTime() / 1000)
}

// 桶时间戳 → X 轴标签
function formatBucketLabel(unixSec: number, bucket: 'hour' | 'day'): string {
  const d = new Date(unixSec * 1000)
  if (bucket === 'day') {
    return `${d.getMonth() + 1}/${d.getDate()}`
  }
  return `${String(d.getHours()).padStart(2, '0')}:00`
}

export function UsageTrendsPanel() {
  const now = useMemo(() => Math.floor(Date.now() / 1000), [])
  const [preset, setPreset] = useState<RangePreset>('24h')
  const [bucket, setBucket] = useState<'hour' | 'day'>('hour')
  // 自定义起止（date input 值）；默认跟随 24h 预设
  const [fromDate, setFromDate] = useState(toDateInput(now - 24 * 3600))
  const [toDate, setToDate] = useState(toDateInput(now))
  // 实际生效的查询区间（点「应用」时才更新）
  const [applied, setApplied] = useState<{ from: number; to: number; bucket: 'hour' | 'day' }>({
    from: now - 24 * 3600,
    to: now,
    bucket: 'hour',
  })

  const { data, isLoading, error } = useTimeseries(applied.from, applied.to, applied.bucket)
  const { data: credData } = useCredentials()

  // 凭据 id → 可读标签（email 优先，否则 #id）
  const credLabel = useMemo(() => {
    const map = new Map<string, string>()
    for (const c of credData?.credentials ?? []) {
      map.set(String(c.id), c.email || `#${c.id}`)
    }
    return map
  }, [credData])

  // 应用预设：更新起止 + 默认粒度
  function applyPreset(p: RangePreset) {
    const nowSec = Math.floor(Date.now() / 1000)
    const spans: Record<RangePreset, number> = {
      '24h': 24 * 3600,
      '7d': 7 * 86400,
      '30d': 30 * 86400,
    }
    const from = nowSec - spans[p]
    const b: 'hour' | 'day' = p === '24h' ? 'hour' : 'day'
    setPreset(p)
    setBucket(b)
    setFromDate(toDateInput(from))
    setToDate(toDateInput(nowSec))
    setApplied({ from, to: nowSec, bucket: b })
  }

  // 应用自定义区间
  function applyCustom() {
    const from = fromDateInput(fromDate)
    // to 取当日 23:59:59，含当天
    const to = fromDateInput(toDate) + 86399
    setApplied({ from, to, bucket })
  }

  const chartData = useMemo(
    () =>
      (data?.series ?? []).map((b: TimeBucket) => ({
        label: formatBucketLabel(b.bucket, applied.bucket),
        输入: b.inputTokens,
        输出: b.outputTokens,
        缓存写: b.cacheWriteTokens,
        缓存读: b.cacheReadTokens,
        费用: Number(b.credits.toFixed(3)),
      })),
    [data, applied.bucket]
  )

  // 区间内 credits 总消耗
  const totalCredits = useMemo(
    () => (data?.series ?? []).reduce((sum, b) => sum + b.credits, 0),
    [data]
  )

  const rangeText = `${toDateInput(applied.from)} ~ ${toDateInput(applied.to)} · 按${
    applied.bucket === 'day' ? '天' : '小时'
  }`

  return (
    <section className="space-y-3">
      {/* 趋势主图 */}
      <Card className="p-4">
        <div className="mb-3 flex flex-wrap items-center gap-2">
          <TrendingUp className="h-4 w-4 text-info" />
          <div>
            <h3 className="text-sm font-semibold">Token 使用趋势</h3>
            <p className="text-xs text-muted-foreground">
              按{applied.bucket === 'day' ? '天' : '小时'}聚合 · 输入/输出/缓存读写 · 费用
            </p>
          </div>
          {/* 区间费用总计 */}
          <div
            className="flex items-center gap-1.5 rounded-lg border px-2.5 py-1"
            style={{ borderColor: COLORS.credits }}
          >
            <Coins className="h-3.5 w-3.5" style={{ color: COLORS.credits }} />
            <span className="text-xs text-muted-foreground">区间费用</span>
            <span className="text-sm font-semibold tabular-nums" style={{ color: COLORS.credits }}>
              {formatCredits(totalCredits)}
            </span>
          </div>

          {/* 时间范围控件 */}
          <div className="ml-auto flex flex-wrap items-center gap-2">
            <div className="flex rounded-lg border p-0.5">
              {(['24h', '7d', '30d'] as RangePreset[]).map((p) => (
                <button
                  key={p}
                  onClick={() => applyPreset(p)}
                  className={`rounded-md px-3 py-1 text-xs font-medium transition-colors ${
                    preset === p
                      ? 'bg-primary text-primary-foreground'
                      : 'text-muted-foreground hover:text-foreground'
                  }`}
                >
                  {p === '24h' ? '24 小时' : p === '7d' ? '7 天' : '30 天'}
                </button>
              ))}
            </div>

            <select
              value={bucket}
              onChange={(e) => setBucket(e.target.value as 'hour' | 'day')}
              className="rounded-md border bg-background px-2 py-1 text-xs"
            >
              <option value="hour">按小时</option>
              <option value="day">按天</option>
            </select>

            <input
              type="date"
              value={fromDate}
              onChange={(e) => setFromDate(e.target.value)}
              className="rounded-md border bg-background px-2 py-1 text-xs"
            />
            <span className="text-xs text-muted-foreground">至</span>
            <input
              type="date"
              value={toDate}
              onChange={(e) => setToDate(e.target.value)}
              className="rounded-md border bg-background px-2 py-1 text-xs"
            />
            <Button size="sm" onClick={applyCustom} className="h-7 text-xs">
              应用
            </Button>
          </div>
        </div>

        {error ? (
          <div className="py-16 text-center text-sm text-destructive">
            趋势数据加载失败：{(error as Error).message}
          </div>
        ) : isLoading && !data ? (
          <div className="py-16 text-center text-sm text-muted-foreground">加载中...</div>
        ) : chartData.length === 0 ? (
          <div className="py-16 text-center text-sm text-muted-foreground">暂无数据</div>
        ) : (
          <ResponsiveContainer width="100%" height={320}>
            <ComposedChart data={chartData} margin={{ top: 8, right: 8, left: 0, bottom: 0 }}>
              <CartesianGrid strokeDasharray="3 3" stroke="hsl(var(--border))" vertical={false} />
              <XAxis
                dataKey="label"
                tick={{ fontSize: 11, fill: 'hsl(var(--muted-foreground))' }}
                tickLine={false}
                axisLine={{ stroke: 'hsl(var(--border))' }}
              />
              <YAxis
                yAxisId="tokens"
                tick={{ fontSize: 11, fill: 'hsl(var(--muted-foreground))' }}
                tickLine={false}
                axisLine={false}
                tickFormatter={formatTokens}
              />
              <YAxis
                yAxisId="credits"
                orientation="right"
                tick={{ fontSize: 11, fill: COLORS.credits }}
                tickLine={false}
                axisLine={false}
                tickFormatter={formatCredits}
              />
              <Tooltip
                contentStyle={{
                  background: 'hsl(var(--card))',
                  border: '1px solid hsl(var(--border))',
                  borderRadius: 8,
                  fontSize: 12,
                }}
                formatter={(value, name) => {
                  const v = Number(value)
                  if (name === '费用') return [`${formatCredits(v)} credits`, name as string]
                  return [formatTokens(v), name as string]
                }}
              />
              <Legend wrapperStyle={{ fontSize: 12 }} />
              <Area
                yAxisId="tokens"
                type="monotone"
                dataKey="输入"
                stroke={COLORS.input}
                fill={COLORS.input}
                fillOpacity={0.12}
                strokeWidth={2}
              />
              <Line
                yAxisId="tokens"
                type="monotone"
                dataKey="输出"
                stroke={COLORS.output}
                strokeWidth={2}
                dot={false}
              />
              <Line
                yAxisId="tokens"
                type="monotone"
                dataKey="缓存写"
                stroke={COLORS.cacheWrite}
                strokeWidth={2}
                dot={false}
              />
              <Line
                yAxisId="tokens"
                type="monotone"
                dataKey="缓存读"
                stroke={COLORS.cacheRead}
                strokeWidth={2}
                dot={false}
              />
              <Line
                yAxisId="credits"
                type="monotone"
                dataKey="费用"
                stroke={COLORS.credits}
                strokeWidth={2.5}
                dot={false}
              />
            </ComposedChart>
          </ResponsiveContainer>
        )}
      </Card>

      {/* 两个分布面板 */}
      <div className="grid gap-3 lg:grid-cols-2">
        <DistributionCard
          title="按模型分布"
          subtitle={rangeText}
          icon={PieIcon}
          items={data?.byModel ?? []}
          labelOf={(k) => k}
        />
        <DistributionCard
          title="按上游凭据分布"
          subtitle={`Top ${data?.byCredential.length ?? 0}`}
          icon={Server}
          items={data?.byCredential ?? []}
          labelOf={(k) => credLabel.get(k) ?? `#${k}`}
        />
      </div>
    </section>
  )
}

// 分布柱图卡片（按总 token 排序展示）
function DistributionCard({
  title,
  subtitle,
  icon: Icon,
  items,
  labelOf,
}: {
  title: string
  subtitle: string
  icon: React.ComponentType<{ className?: string }>
  items: DimBucket[]
  labelOf: (key: string) => string
}) {
  const rows = useMemo(
    () =>
      items
        .map((d) => ({
          label: labelOf(d.key),
          费用: Number(d.credits.toFixed(3)),
          tokens: d.inputTokens + d.outputTokens + d.cacheReadTokens + d.cacheWriteTokens,
          requests: d.requests,
        }))
        // 有费用时按费用排，否则退回按 token（估算路径无 credits）
        .sort((a, b) => b.费用 - a.费用 || b.tokens - a.tokens)
        .slice(0, 8),
    [items, labelOf]
  )

  return (
    <Card className="p-4">
      <div className="mb-3 flex items-center gap-2">
        <Icon className="h-4 w-4 text-info" />
        <h3 className="text-sm font-semibold">{title}</h3>
        <span className="ml-auto text-xs text-muted-foreground">{subtitle}</span>
      </div>
      {rows.length === 0 ? (
        <div className="py-16 text-center text-sm text-muted-foreground">暂无数据</div>
      ) : (
        <ResponsiveContainer width="100%" height={240}>
          <BarChart data={rows} layout="vertical" margin={{ top: 4, right: 16, left: 8, bottom: 4 }}>
            <CartesianGrid strokeDasharray="3 3" stroke="hsl(var(--border))" horizontal={false} />
            <XAxis
              type="number"
              tick={{ fontSize: 11, fill: 'hsl(var(--muted-foreground))' }}
              tickLine={false}
              axisLine={false}
              tickFormatter={formatCredits}
            />
            <YAxis
              type="category"
              dataKey="label"
              width={140}
              tick={{ fontSize: 11, fill: 'hsl(var(--muted-foreground))' }}
              tickLine={false}
              axisLine={false}
            />
            <Tooltip
              contentStyle={{
                background: 'hsl(var(--card))',
                border: '1px solid hsl(var(--border))',
                borderRadius: 8,
                fontSize: 12,
              }}
              formatter={(value, _name, item) => {
                const p = item?.payload as { tokens: number; requests: number } | undefined
                return [
                  `${formatCredits(Number(value))} credits · ${formatTokens(p?.tokens ?? 0)} token · ${p?.requests ?? 0} 次`,
                  '消耗',
                ]
              }}
            />
            <Bar dataKey="费用" radius={[0, 4, 4, 0]}>
              {rows.map((_, i) => (
                <Cell key={i} fill={DIST_COLORS[i % DIST_COLORS.length]} />
              ))}
            </Bar>
          </BarChart>
        </ResponsiveContainer>
      )}
    </Card>
  )
}
