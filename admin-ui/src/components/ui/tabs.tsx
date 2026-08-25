import * as React from 'react'
import { cn } from '@/lib/utils'

// 轻量受控 Tab（避免引入 @radix-ui/react-tabs 依赖）
export interface TabItem {
  value: string
  label: string
  icon?: React.ComponentType<{ className?: string }>
}

interface TabNavProps {
  items: TabItem[]
  value: string
  onChange: (value: string) => void
  /// 'vertical' 竖向侧边导航；'horizontal' 顶部横向
  orientation?: 'vertical' | 'horizontal'
  className?: string
}

export function TabNav({
  items,
  value,
  onChange,
  orientation = 'vertical',
  className,
}: TabNavProps) {
  const vertical = orientation === 'vertical'
  return (
    <div
      role="tablist"
      aria-orientation={orientation}
      className={cn(
        vertical ? 'flex flex-col gap-1' : 'flex flex-row gap-1 border-b',
        className
      )}
    >
      {items.map((item) => {
        const active = item.value === value
        const Icon = item.icon
        return (
          <button
            key={item.value}
            type="button"
            role="tab"
            aria-selected={active}
            onClick={() => onChange(item.value)}
            className={cn(
              'flex items-center gap-2 rounded-md px-3 py-2 text-sm font-medium transition-colors text-left',
              vertical ? 'w-full' : 'rounded-b-none border-b-2 border-transparent',
              active
                ? vertical
                  ? 'bg-secondary text-secondary-foreground'
                  : 'border-primary text-foreground'
                : 'text-muted-foreground hover:bg-accent hover:text-accent-foreground'
            )}
          >
            {Icon && <Icon className="h-4 w-4 shrink-0" />}
            <span className="truncate">{item.label}</span>
          </button>
        )
      })}
    </div>
  )
}
