import { useEffect, useId, useRef, useState } from 'react'
import { keepPreviousData, useQuery } from '@tanstack/react-query'
import { createFileRoute, Link } from '@tanstack/react-router'
import { Activity, AlertCircle, ArrowUpRight, CalendarDays, Images, LoaderCircle, RotateCcw, Sparkles, Users } from 'lucide-react'
import { z } from 'zod'
import {
  api,
  insightsAssetsSchema,
  insightsCompareSchema,
  insightsOverviewSchema,
  queryString,
  type InsightsAssets,
  type InsightsOverview,
  type RankedAsset,
} from '../lib/api'
import type { TimelineChart } from '../lib/timeline-chart'
import styles from './images-trends.module.css'

const csvSearchValue = z.preprocess(
  (value) => typeof value === 'string' ? normalizeCsvSearch(value) : value,
  z.string().optional(),
).catch(undefined)

export const trendsSearchSchema = z.object({
  from: z.string().optional().catch(undefined),
  to: z.string().optional().catch(undefined),
  senders: csvSearchValue,
  tables: csvSearchValue,
  conversation_type: csvSearchValue,
  rank: z.enum(['popular', 'growth', 'reach', 'new', 'revival']).optional().catch(undefined),
})

export const Route = createFileRoute('/images/trends')({
  validateSearch: (search) => trendsSearchSchema.parse(search),
  component: ImageTrendsPage,
})

function ImageTrendsPage() {
  const search = Route.useSearch()
  const navigate = Route.useNavigate()
  const requestClientId = useId()
  const [requestGeneration, setRequestGeneration] = useState(0)
  const rank = search.rank ?? 'popular'
  const filterParams = {
    from: search.from,
    to: search.to,
    senders: search.senders,
    tables: search.tables,
    conversation_type: search.conversation_type,
  }
  const requestParams = {
    ...filterParams,
    rank,
  }
  const requestToken = `${requestClientId}-${requestGeneration}`
  const overview = useQuery({
    queryKey: ['image-insights-overview', filterParams],
    queryFn: ({ signal }) => api(
      `/api/image-index/insights/overview${queryString(filterParams)}`,
      insightsOverviewSchema,
      { signal },
    ),
    placeholderData: keepPreviousData,
  })
  const [lastCompleteAssets, setLastCompleteAssets] = useState<InsightsAssets>()
  const assets = useQuery({
    queryKey: ['image-insights-assets', requestParams],
    enabled: Boolean(overview.data?.coverage.ready),
    queryFn: async ({ signal }) => {
      const result = await api(
        `/api/image-index/insights/assets${queryString({ ...requestParams, request_token: requestToken, limit: 60 })}`,
        insightsAssetsSchema,
        { signal },
      )
      if (result.status === 'complete') setLastCompleteAssets(result)
      return result
    },
    placeholderData: keepPreviousData,
    refetchInterval: (query) => query.state.data?.status === 'warming' ? 700 : false,
  })
  const senderCompare = useQuery({
    queryKey: ['image-insights-compare', 'sender', requestParams],
    enabled: !assets.isPlaceholderData && assets.data?.status === 'complete',
    queryFn: ({ signal }) => api(
      `/api/image-index/insights/compare${queryString({ ...requestParams, by: 'sender' })}`,
      insightsCompareSchema,
      { signal },
    ),
    placeholderData: keepPreviousData,
  })
  const conversationCompare = useQuery({
    queryKey: ['image-insights-compare', 'conversation', requestParams],
    enabled: !assets.isPlaceholderData && assets.data?.status === 'complete',
    queryFn: ({ signal }) => api(
      `/api/image-index/insights/compare${queryString({ ...requestParams, by: 'conversation' })}`,
      insightsCompareSchema,
      { signal },
    ),
    placeholderData: keepPreviousData,
  })
  const displayedAssets = assets.data?.status === 'complete'
    ? assets.data
    : lastCompleteAssets

  const patchSearch = (patch: Partial<z.infer<typeof trendsSearchSchema>>) => {
    setRequestGeneration((value) => value + 1)
    void navigate({ search: (previous) => ({ ...previous, ...patch }), replace: true })
  }
  const toggleCsv = (field: 'senders' | 'tables', value: string) => {
    const selected = new Set(csv(search[field]))
    if (selected.has(value)) selected.delete(value)
    else selected.add(value)
    patchSearch({ [field]: Array.from(selected).sort().join(',') || undefined })
  }
  const activeFilters = Boolean(search.from || search.to || search.senders || search.tables || search.conversation_type)

  return <main className={styles.page}>
    <header className={styles.hero}>
      <div>
        <span className={styles.eyebrow}><Sparkles /> IMAGE PROPAGATION</span>
        <h1>热门图片与传播趋势</h1>
        <p>按每条引用消息计数；完全副本与 SSCD ≥ 0.98 变体在当前服务进程中合并。</p>
      </div>
      <div className={styles.heroActions}>
        {activeFilters && <button type="button" onClick={() => patchSearch({
          from: undefined,
          to: undefined,
          senders: undefined,
          tables: undefined,
          conversation_type: undefined,
        })}><RotateCcw /> 清除筛选</button>}
        <Link to="/images"><Images /> 返回图库</Link>
      </div>
    </header>

    {overview.isPending && <State icon={<LoaderCircle className={styles.spin} />} text="正在读取传播统计…" />}
    {overview.isError && <State icon={<AlertCircle />} text={overview.error.message} error />}
    {overview.data && <>
      {!overview.data.coverage.ready && <section className={styles.buildNotice}>
        <Activity />
        <div><strong>传播分析尚未完成</strong><span>请在“后台维护”启动“热门图片消息事实与传播聚合”。已有图库与聊天功能不受影响。</span></div>
        <Link to="/tasks">打开后台任务 <ArrowUpRight /></Link>
      </section>}

      <section className={styles.summary} aria-label="当前筛选摘要">
        <Metric value={overview.data.summary.references} label="引用消息" />
        <Metric value={overview.data.summary.images} label="完全副本组" />
        <Metric value={overview.data.summary.senders} label="发送人数" />
        <Metric value={overview.data.summary.conversations} label="覆盖会话" />
        <div className={styles.coverage}>
          <span>事实覆盖率</span>
          <strong>{(overview.data.coverage.fact_coverage * 100).toFixed(1)}%</strong>
          <small>{formatNumber(overview.data.coverage.facts)} / {formatNumber(overview.data.coverage.occurrences)}</small>
        </div>
      </section>

      <section className={styles.timelinePanel}>
        <header>
          <div><CalendarDays /><span><strong>传播时间线</strong><small>虚线为全量，实线为当前人员 / 会话筛选；滚轮缩放、拖动时间窗。</small></span></div>
          <div className={styles.dateInputs}>
            <select aria-label="会话类型" value={search.conversation_type ?? ''} onChange={(event) => patchSearch({ conversation_type: event.target.value || undefined })}>
              <option value="">全部会话类型</option>
              <option value="group">群聊</option>
              <option value="buddy">好友</option>
              <option value="discuss">讨论组</option>
              <option value="system">系统</option>
            </select>
            <input aria-label="开始日期" type="date" value={search.from ?? ''} min={overview.data.range.from} max={search.to ?? overview.data.range.to} onChange={(event) => patchSearch({ from: event.target.value || undefined })} />
            <span>—</span>
            <input aria-label="结束日期" type="date" value={search.to ?? ''} min={search.from ?? overview.data.range.from} max={overview.data.range.to} onChange={(event) => patchSearch({ to: event.target.value || undefined })} />
          </div>
        </header>
        <Timeline
          data={overview.data}
          selectedFrom={search.from}
          selectedTo={search.to}
          onRange={(from, to) => patchSearch({ from, to })}
        />
      </section>

      <section className={styles.facets}>
        <Facet
          title="人员"
          icon={<Users />}
          items={overview.data.senders.map((item) => ({ key: item.uin, label: item.label || item.uin, count: item.count }))}
          selected={new Set(csv(search.senders))}
          onToggle={(value) => toggleCsv('senders', value)}
        />
        <Facet
          title="群聊与会话"
          icon={<Images />}
          items={overview.data.conversations.map((item) => ({ key: item.table, label: item.label || item.table, count: item.count }))}
          selected={new Set(csv(search.tables))}
          onToggle={(value) => toggleCsv('tables', value)}
        />
      </section>

      <section className={styles.ranking}>
        <header>
          <div><strong>图片榜单</strong><small>2,000 候选池排名；卡片合并计数准确，长尾合并可能漏榜。</small></div>
          <div className={styles.rankTabs}>
            {([
              ['popular', '总热门'],
              ['growth', '上升'],
              ['reach', '传播最广'],
              ['new', '新出现'],
              ['revival', '复燃'],
            ] as const).map(([value, label]) => <button type="button" key={value} className={rank === value ? styles.active : ''} onClick={() => patchSearch({ rank: value })}>{label}</button>)}
          </div>
        </header>
        {assets.data?.status === 'warming' && <div className={styles.warming}>
          <LoaderCircle className={styles.spin} />
          <span>正在合并 SSCD 同图集合</span>
          <progress max={Math.max(assets.data.progress.total, 1)} value={assets.data.progress.processed} />
          <strong>{assets.data.progress.processed} / {assets.data.progress.total || '…'}</strong>
        </div>}
        {assets.isError && <State icon={<AlertCircle />} text={assets.error.message} error />}
        {assets.data?.status === 'failed' && <State icon={<AlertCircle />} text={assets.data.error ?? '榜单构建失败'} error />}
        {displayedAssets?.status === 'complete' && displayedAssets.items.length === 0 && <State icon={<Images />} text="当前筛选没有符合此排名模式的图片。" />}
        {displayedAssets?.items.length ? <div className={styles.cardGrid}>
          {displayedAssets.items.map((asset, index) => <TrendCard key={asset.id} asset={asset} rank={rank} position={index + 1} filters={requestParams} />)}
        </div> : null}
      </section>

      <section className={styles.compareGrid}>
        <Compare
          title="人员排行"
          items={senderCompare.data?.items}
          loading={senderCompare.isPending}
          labels={new Map(overview.data.senders.map((item) => [item.uin, item.label]))}
        />
        <Compare
          title="会话排行"
          items={conversationCompare.data?.items}
          loading={conversationCompare.isPending}
          labels={new Map(overview.data.conversations.map((item) => [item.table, item.label]))}
        />
      </section>
    </>}
  </main>
}

function Timeline({ data, selectedFrom, selectedTo, onRange }: {
  data: InsightsOverview
  selectedFrom?: string
  selectedTo?: string
  onRange: (from?: string, to?: string) => void
}) {
  const target = useRef<HTMLDivElement>(null)
  const chartRef = useRef<TimelineChart | null>(null)
  const bucketsRef = useRef<ReturnType<typeof timelineBuckets>>([])
  const chartInputRef = useRef({ data, selectedFrom, selectedTo })
  const onRangeRef = useRef(onRange)
  const selectedRangeRef = useRef({ from: selectedFrom, to: selectedTo })
  const timerRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)
  onRangeRef.current = onRange
  selectedRangeRef.current = { from: selectedFrom, to: selectedTo }
  useEffect(() => {
    const element = target.current
    if (!element) return
    let disposed = false
    let observer: ResizeObserver | undefined
    void import('../lib/timeline-chart').then(({ initTimelineChart }) => {
      if (disposed) return
      const chart = initTimelineChart(element)
      chartRef.current = chart
      updateTimelineChart(chart, bucketsRef, chartInputRef.current)
      chart.on('datazoom', (event: unknown) => {
        const buckets = bucketsRef.current
        if (!buckets.length) return
        const payload = event as { start?: number; end?: number; batch?: Array<{ start?: number; end?: number }> }
        const zoom = payload.batch?.[0] ?? payload
        const start = Math.max(0, Math.round((zoom.start ?? 0) / 100 * Math.max(buckets.length - 1, 0)))
        const end = Math.min(buckets.length - 1, Math.round((zoom.end ?? 100) / 100 * Math.max(buckets.length - 1, 0)))
        const from = start === 0 ? undefined : buckets[start]?.from
        const to = end === buckets.length - 1 ? undefined : buckets[end]?.to
        if (from === selectedRangeRef.current.from && to === selectedRangeRef.current.to) return
        clearTimeout(timerRef.current)
        timerRef.current = setTimeout(() => onRangeRef.current(from, to), 900)
      })
      observer = new ResizeObserver(() => chart.resize())
      observer.observe(element)
    })
    return () => {
      disposed = true
      clearTimeout(timerRef.current)
      observer?.disconnect()
      chartRef.current?.dispose()
      chartRef.current = null
    }
  }, [])
  useEffect(() => {
    chartInputRef.current = { data, selectedFrom, selectedTo }
    const chart = chartRef.current
    if (chart) updateTimelineChart(chart, bucketsRef, { data, selectedFrom, selectedTo })
  }, [data, selectedFrom, selectedTo])
  return <div ref={target} className={styles.timeline} role="img" aria-label="图片引用传播时间线" />
}

function updateTimelineChart(
  chart: TimelineChart,
  bucketsRef: { current: ReturnType<typeof timelineBuckets> },
  input: { data: InsightsOverview; selectedFrom?: string; selectedTo?: string },
) {
  const buckets = timelineBuckets(input.data.timeline)
  bucketsRef.current = buckets
  const dates = buckets.map((item) => item.label)
  const [start, end] = selectedZoom(buckets, input.selectedFrom, input.selectedTo)
  chart.setOption({
    animation: false,
    grid: { left: 48, right: 18, top: 22, bottom: 54 },
    tooltip: { trigger: 'axis' },
    xAxis: { type: 'category', data: dates, axisLabel: { color: '#78869b', hideOverlap: true } },
    yAxis: { type: 'value', axisLabel: { color: '#78869b' }, splitLine: { lineStyle: { color: '#28324466' } } },
    dataZoom: [
      { type: 'inside', start, end, zoomOnMouseWheel: true, moveOnMouseMove: true, throttle: 300 },
      { type: 'slider', start, end, realtime: false, height: 18, bottom: 12, borderColor: '#283244', fillerColor: '#72a7ff33' },
    ],
    series: [
      { name: '全量', type: 'line', showSymbol: false, data: buckets.map((item) => item.count), lineStyle: { type: 'dashed', width: 1.5, color: '#718097' }, areaStyle: { color: '#71809712' } },
      { name: '当前筛选', type: 'line', showSymbol: false, data: buckets.map((item) => item.selected_count), lineStyle: { width: 2, color: '#72a7ff' }, areaStyle: { color: '#72a7ff22' } },
    ],
  }, true)
}

function selectedZoom(
  buckets: ReturnType<typeof timelineBuckets>,
  selectedFrom?: string,
  selectedTo?: string,
): [number, number] {
  if (buckets.length < 2) return [0, 100]
  const matchingStart = selectedFrom
    ? buckets.findIndex((bucket) => bucket.to >= selectedFrom)
    : 0
  const startIndex = matchingStart < 0 ? 0 : matchingStart
  let endIndex = buckets.length - 1
  if (selectedTo) {
    for (let index = buckets.length - 1; index >= 0; index -= 1) {
      if (buckets[index].from <= selectedTo) {
        endIndex = index
        break
      }
    }
  }
  const denominator = buckets.length - 1
  return [startIndex / denominator * 100, endIndex / denominator * 100]
}

function timelineBuckets(timeline: InsightsOverview['timeline']) {
  const mode = timeline.length > 730 ? 'month' : timeline.length > 180 ? 'week' : 'day'
  const buckets = new Map<string, {
    label: string
    from: string
    to: string
    count: number
    selected_count: number
  }>()
  for (const point of timeline) {
    const date = new Date(`${point.date}T00:00:00Z`)
    const label = mode === 'month'
      ? point.date.slice(0, 7)
      : mode === 'week'
        ? new Date(date.getTime() - ((date.getUTCDay() + 6) % 7) * 86_400_000).toISOString().slice(0, 10)
        : point.date
    const bucket = buckets.get(label) ?? {
      label,
      from: point.date,
      to: point.date,
      count: 0,
      selected_count: 0,
    }
    bucket.to = point.date
    bucket.count += point.count
    bucket.selected_count += point.selected_count
    buckets.set(label, bucket)
  }
  return Array.from(buckets.values())
}

function Facet({ title, icon, items, selected, onToggle }: {
  title: string
  icon: React.ReactNode
  items: Array<{ key: string; label: string; count: number }>
  selected: Set<string>
  onToggle: (value: string) => void
}) {
  return <section className={styles.facet}>
    <header>{icon}<strong>{title}</strong><small>可多选 · 同维度 OR</small></header>
    <div>{items.slice(0, 36).map((item) => <button type="button" className={selected.has(item.key) ? styles.selected : ''} key={item.key} onClick={() => onToggle(item.key)} title={item.key}><span>{item.label}</span><strong>{formatNumber(item.count)}</strong></button>)}</div>
  </section>
}

function TrendCard({ asset, rank, position, filters }: {
  asset: RankedAsset
  rank: string
  position: number
  filters: { from?: string; to?: string; senders?: string; tables?: string; conversation_type?: string }
}) {
  const max = Math.max(...asset.trend.map((point) => point.count), 1)
  const preserved = queryString(filters)
  return <article className={styles.card}>
    <a href={`${asset.detail_url}${preserved}`} className={styles.visual}>
      <img src={asset.thumbnail_url} loading="lazy" alt={`热门图片 #${asset.id}`} />
      <span>#{position}</span>
    </a>
    <div className={styles.cardBody}>
      <header><strong>{formatNumber(asset.reference_count)} 次引用</strong><small>{asset.same_image_members} 个同图文件</small></header>
      <div className={styles.cardMetrics}>
        <span><strong>{asset.sender_count}</strong> 人</span>
        <span><strong>{asset.conversation_count}</strong> 会话</span>
        {rank === 'growth' && <span className={asset.growth >= 0 ? styles.positive : styles.negative}><strong>{asset.growth >= 0 ? '+' : ''}{asset.growth}</strong> 增长</span>}
      </div>
      <div className={styles.sparkline} aria-label="引用趋势">
        {asset.trend.slice(-28).map((point) => <i key={point.date} style={{ height: `${Math.max(8, point.count / max * 100)}%` }} title={`${point.date}: ${point.count}`} />)}
      </div>
      <dl>
        <div><dt>首次</dt><dd>{asset.first_date || '—'}</dd></div>
        <div><dt>峰值</dt><dd>{asset.peak_date || '—'}</dd></div>
        <div><dt>最近</dt><dd>{asset.last_date || '—'}</dd></div>
      </dl>
      <footer>
        <a href={`${asset.detail_url}${preserved}`}>图片详情</a>
        <a href={`${asset.references_url}${preserved}`}>引用上下文 <ArrowUpRight /></a>
      </footer>
    </div>
  </article>
}

function Compare({ title, items, loading, labels }: {
  title: string
  items?: Array<{ key: string; label: string; references: number; top_images: Array<{ id: number; thumbnail_url: string; detail_url: string }> }>
  loading: boolean
  labels?: Map<string, string>
}) {
  return <section className={styles.compare}>
    <header><strong>{title}</strong><small>当前筛选</small></header>
    {loading && <div className={styles.compareLoading}><LoaderCircle className={styles.spin} /> 读取排行…</div>}
    {items?.slice(0, 12).map((item, index) => <div className={styles.compareRow} key={item.key}>
      <span className={styles.compareRank}>{index + 1}</span>
      <div className={styles.compareName}><strong>{labels?.get(item.key) || item.label}</strong><small>{formatNumber(item.references)} 次引用</small></div>
      <div className={styles.thumbStrip}>{item.top_images.slice(0, 6).map((image) => <Link key={image.id} to="/images/$assetId" params={{ assetId: String(image.id) }}><img src={image.thumbnail_url} loading="lazy" alt="" /></Link>)}</div>
    </div>)}
  </section>
}

function Metric({ value, label }: { value: number; label: string }) {
  return <div className={styles.metric}><strong>{formatNumber(value)}</strong><span>{label}</span></div>
}

function State({ icon, text, error = false }: { icon: React.ReactNode; text: string; error?: boolean }) {
  return <div className={`${styles.state} ${error ? styles.error : ''}`}>{icon}<span>{text}</span></div>
}

function csv(value?: string) {
  return value ? normalizeCsvSearch(value)?.split(',') ?? [] : []
}

function normalizeCsvSearch(value: string) {
  const values = value
    .split(',')
    .map((item) => item.trim())
    .map((item) => item.startsWith('"') && item.endsWith('"') ? item.slice(1, -1).trim() : item)
    .filter(Boolean)
  return Array.from(new Set(values)).sort().join(',') || undefined
}

function formatNumber(value: number) {
  return new Intl.NumberFormat('zh-CN').format(value)
}
