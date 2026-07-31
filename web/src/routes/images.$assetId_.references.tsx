import { useEffect, useRef, useState } from 'react'
import { keepPreviousData, useQuery } from '@tanstack/react-query'
import { createFileRoute, Link } from '@tanstack/react-router'
import { AlertCircle, ArrowLeft, CalendarDays, ChevronLeft, ChevronRight, ExternalLink, ListFilter, LoaderCircle, MessageSquare, Network, Users, X } from 'lucide-react'
import { z } from 'zod'
import { RichMessage, type MediaAsset, type RichNode } from '../components/RichMessage'
import { api, assetDetailSchema, queryString, referenceAnalysisSchema, type ReferenceAnalysis } from '../lib/api'
import type { TimelineChart } from '../lib/timeline-chart'
import styles from './image-references.module.css'

export const referenceSearchSchema = z.object({
  offset: z.coerce.number().int().nonnegative().optional().catch(undefined),
  sender: z.string().optional().catch(undefined),
  senders: z.string().optional().catch(undefined),
  table: z.string().optional().catch(undefined),
  tables: z.string().optional().catch(undefined),
  conversation_type: z.string().optional().catch(undefined),
  period: z.string().regex(/^\d{4}-\d{2}$/).optional().catch(undefined),
  from: z.string().regex(/^\d{4}-\d{2}-\d{2}$/).optional().catch(undefined),
  to: z.string().regex(/^\d{4}-\d{2}-\d{2}$/).optional().catch(undefined),
})

export const Route = createFileRoute('/images/$assetId_/references')({
  validateSearch: (search) => referenceSearchSchema.parse(search),
  component: ImageReferencePage,
})

const PAGE_SIZE = 12

function ImageReferencePage() {
  const { assetId } = Route.useParams()
  const search = Route.useSearch()
  const navigate = Route.useNavigate()
  const id = Number(assetId)
  const offset = search.offset ?? 0
  const enabled = Number.isSafeInteger(id) && id > 0
  const detail = useQuery({
    queryKey: ['image-detail', id],
    queryFn: ({ signal }) => api(`/api/image-index/assets/${id}`, assetDetailSchema, { signal }),
    enabled,
  })
  const analysis = useQuery({
    queryKey: ['image-reference-analysis', id, offset, search.sender, search.senders, search.table, search.tables, search.conversation_type, search.period, search.from, search.to],
    queryFn: ({ signal }) => api(
      `/api/image-index/assets/${id}/reference-analysis${queryString({ offset, limit: PAGE_SIZE, context: 5, sender: search.sender, senders: search.senders, table: search.table, tables: search.tables, conversation_type: search.conversation_type, period: search.period, date_from: search.from, date_to: search.to })}`,
      referenceAnalysisSchema,
      { signal },
    ),
    enabled,
    placeholderData: keepPreviousData,
    refetchInterval: (query) => query.state.data?.same_image_status === 'warming' ? 5_000 : false,
  })
  const changePage = (nextOffset: number) => {
    void navigate({ search: (previous) => ({ ...previous, offset: nextOffset > 0 ? nextOffset : undefined }) })
    window.scrollTo({ top: 0, behavior: 'smooth' })
  }
  const applyFilter = (filter: { sender?: string; table?: string; period?: string; from?: string; to?: string }) => {
    void navigate({ search: (previous) => ({
      ...previous,
      ...filter,
      senders: filter.sender ? undefined : previous.senders,
      tables: filter.table ? undefined : previous.tables,
      offset: undefined,
    }) })
    document.querySelector(`.${styles.contextSection}`)?.scrollIntoView({ behavior: 'smooth' })
  }
  const clearFilter = (key?: 'sender' | 'senders' | 'table' | 'tables' | 'conversation_type' | 'period') => void navigate({
    search: (previous) => key ? { ...previous, [key]: undefined, offset: undefined } : {},
  })
  const clearDateFilter = () => void navigate({ search: (previous) => ({ ...previous, period: undefined, from: undefined, to: undefined, offset: undefined }) })

  if (detail.isPending || analysis.isPending) return <main className={styles.state}><LoaderCircle className={styles.spin} /> 正在汇总引用上下文…</main>
  if (detail.isError || analysis.isError) return <main className={styles.state}><AlertCircle /> {detail.error?.message || analysis.error?.message}</main>
  const asset = detail.data
  const report = analysis.data
  const page = Math.floor(report.offset / report.limit) + 1
  const pageCount = Math.max(1, Math.ceil(report.total / report.limit))

  return <main className={styles.page}>
    <nav className={styles.topbar}>
      <Link to="/images/$assetId" params={{ assetId }} search={{
        from: search.from,
        to: search.to,
        senders: search.senders,
        tables: search.tables,
        conversation_type: search.conversation_type,
      }}><ArrowLeft size={16} /> 返回图片详情</Link>
      <a href={`/chat?table=${encodeURIComponent(report.items[0]?.table ?? '')}&rowid=${report.items[0]?.rowid ?? 1}`}><MessageSquare size={16} /> 打开最近引用</a>
    </nav>

    <header className={styles.hero}>
      <img src={asset.thumbnail_url} alt={`图片 ${id}`} />
      <div><span>图片 #{id} · 高级分析</span><h1>引用人与聊天上下文</h1><p>{report.same_image_status === 'warming' ? '正在后台合并 SSCD 同图；当前先展示完全副本引用，完成后自动更新。' : '已合并 SSCD ≥ 98% 的同图资产及其完全副本；每条结果都可跳回原消息。'}</p></div>
    </header>

    <Coverage report={report} />

    <section className={styles.summary} aria-label="引用摘要">
      <Summary icon={<Network />} value={report.summary.same_image_assets} label="同图资产" />
      <Summary icon={<MessageSquare />} value={report.summary.indexed_occurrences} label="已索引引用" />
      <Summary icon={<Users />} value={report.summary.unique_senders} label="引用人" />
      <Summary icon={<Network />} value={report.summary.unique_conversations} label="会话 / 群" />
      <Summary icon={<CalendarDays />} value={report.daily_timeline.length} label="活跃天数" />
    </section>

    <div className={styles.analysisGrid}>
      <Ranking title="引用人" items={report.top_senders.map((item) => ({ key: item.uin, label: item.label, detail: item.is_self ? '自己' : `QQ ${item.uin}`, count: item.count, selectedCount: item.selected_count, avatar: qlogo(item.uin), href: item.chat_url, filter: { sender: item.uin } }))} activeKey={search.sender} onFilter={applyFilter} />
      <Ranking title="引用会话 / 群" items={report.top_conversations.map((item) => ({ key: item.table, label: item.label, detail: conversationType(item.type, item.id), count: item.count, selectedCount: item.selected_count, avatar: item.avatar_url, href: item.chat_url, filter: { table: item.table } }))} activeKey={search.table} onFilter={applyFilter} />
      <Timeline report={report} activeFrom={search.from ?? (search.period ? `${search.period}-01` : undefined)} activeTo={search.to ?? (search.period ? `${search.period}-31` : undefined)} onFilter={(from, to) => applyFilter({ period: undefined, from, to })} />
    </div>

    <section className={styles.contextSection}>
      <header><div><h2>引用上下文</h2><p>按消息时间从新到旧；每条引用已加载前后各 5 条消息，默认紧凑收起。</p></div><span>{report.total.toLocaleString()} 条中的 {report.total ? report.offset + 1 : 0}–{Math.min(report.total, report.offset + report.items.length)}</span></header>
      {(search.sender || search.senders || search.table || search.tables || search.conversation_type || search.period || search.from || search.to) && <div className={styles.activeFilter}>
        <ListFilter size={14} /><span>当前筛选</span>
        {search.sender && <button type="button" onClick={() => clearFilter('sender')}>引用人：{report.top_senders.find((item) => item.uin === search.sender)?.label ?? search.sender}<X size={12} /></button>}
        {search.senders && <button type="button" onClick={() => clearFilter('senders')}>引用人：{search.senders.split(',').length} 人<X size={12} /></button>}
        {search.table && <button type="button" onClick={() => clearFilter('table')}>会话：{report.top_conversations.find((item) => item.table === search.table)?.label ?? search.table}<X size={12} /></button>}
        {search.tables && <button type="button" onClick={() => clearFilter('tables')}>会话：{search.tables.split(',').length} 个<X size={12} /></button>}
        {search.conversation_type && <button type="button" onClick={() => clearFilter('conversation_type')}>类型：{search.conversation_type}<X size={12} /></button>}
        {(search.period || search.from || search.to) && <button type="button" onClick={clearDateFilter}>日期：{search.from ?? search.period ?? '最早'} → {search.to ?? search.period ?? '最新'}<X size={12} /></button>}
        <button className={styles.clearAll} type="button" onClick={() => clearFilter()}>全部清除</button>
      </div>}
      {analysis.isFetching && <div className={styles.refreshing}><LoaderCircle className={styles.spin} /> {report.same_image_status === 'warming' ? '正在后台合并同图引用…' : '正在切换分页…'}</div>}
      {!report.items.length && <div className={styles.empty}>当前索引范围内没有可解析的引用消息。</div>}
      <div className={styles.contextList}>{report.items.map((item) => <ReferenceCard key={`${item.table}-${item.rowid}`} item={item} />)}</div>
      <nav className={styles.pagination} aria-label="引用分页">
        <button type="button" disabled={offset === 0 || analysis.isFetching} onClick={() => changePage(Math.max(0, offset - PAGE_SIZE))}><ChevronLeft size={16} /> 上一页</button>
        <span>第 {page} / {pageCount} 页</span>
        <button type="button" disabled={!report.has_more || analysis.isFetching} onClick={() => changePage(offset + PAGE_SIZE)}>下一页 <ChevronRight size={16} /></button>
      </nav>
    </section>
  </main>
}

function Coverage({ report }: { report: ReferenceAnalysis }) {
  const coverage = report.coverage
  const scanned = coverage.rows_scanned ?? 0
  const span = coverage.estimated_rowid_span ?? scanned
  const percent = coverage.complete ? 100 : span > 0 ? Math.min(99.9, scanned / span * 100) : 0
  return <section className={`${styles.coverage} ${coverage.complete ? styles.complete : styles.partial}`}>
    <div><strong>{coverage.complete ? '引用索引已完整扫描' : '引用索引仍不完整'}</strong><span>{coverage.complete ? `${coverage.tables_indexed.toLocaleString()} 张会话表已完成` : `${coverage.tables_incomplete} 张会话表未完成，当前统计只是已索引下限`}</span></div>
    <div className={styles.coverageTrack}><i style={{ width: `${percent}%` }} /></div>
    <b>{percent.toFixed(1)}%</b>
    {report.summary.missing_messages > 0 && <p><AlertCircle size={14} /> {report.summary.missing_messages.toLocaleString()} 条索引位置在当前聊天库中已找不到，可能来自已删除或替换的消息。</p>}
  </section>
}

function Summary({ icon, value, label }: { icon: React.ReactNode; value: number; label: string }) {
  return <div>{icon}<strong>{value.toLocaleString()}</strong><span>{label}</span></div>
}

type RankingItem = { key: string; label: string; detail: string; count: number; selectedCount: number; avatar?: string; href: string; filter: { sender?: string; table?: string; period?: string } }
function Ranking({ title, items, activeKey, onFilter }: { title: string; items: RankingItem[]; activeKey?: string; onFilter: (filter: RankingItem['filter']) => void }) {
  const max = Math.max(...items.map((item) => item.count), 1)
  return <section className={styles.ranking}><h2>{title}<span>Top {items.length}</span></h2><div>{items.map((item) => {
    return <div className={`${styles.rankRow} ${activeKey === item.key ? styles.rankActive : ''}`} key={item.key}>
      <button className={styles.rankIdentity} type="button" onClick={() => onFilter(item.filter)} title={`当前条件 ${item.selectedCount.toLocaleString()} / 全部 ${item.count.toLocaleString()}；筛选下方引用详情`}><Avatar src={item.avatar} label={item.label} /><span><strong>{item.label}</strong><small>{item.detail}</small><i><span style={{ width: `${Math.max(2, item.count / max * 100)}%` }} /><b style={{ width: `${item.selectedCount > 0 ? Math.max(2, item.selectedCount / max * 100) : 0}%` }} /></i></span></button>
      <em><span><strong>{item.selectedCount.toLocaleString()}</strong> / {item.count.toLocaleString()}</span><small>当前 / 全部</small></em>
      <span className={styles.rankActions}><button type="button" onClick={() => onFilter(item.filter)}><ListFilter size={12} /> 筛选</button><a href={item.href}><ExternalLink size={12} /> 跳转</a></span>
    </div>
  })}</div></section>
}

function Timeline({ report, activeFrom, activeTo, onFilter }: { report: ReferenceAnalysis; activeFrom?: string; activeTo?: string; onFilter: (from: string, to: string) => void }) {
  const host = useRef<HTMLDivElement>(null)
  const selection = useRef<HTMLDivElement>(null)
  const chartRef = useRef<TimelineChart | null>(null)
  const filterRef = useRef(onFilter)
  const modeRef = useRef<'brush' | 'pan'>('brush')
  const datesRef = useRef<string[]>([])
  const [mode, setMode] = useState<'brush' | 'pan'>('brush')
  const [chartReady, setChartReady] = useState(false)
  const points = continuousDays(report.daily_timeline)

  useEffect(() => { filterRef.current = onFilter }, [onFilter])
  useEffect(() => { modeRef.current = mode }, [mode])

  useEffect(() => {
    if (!host.current) return
    let disposed = false
    let observer: ResizeObserver | undefined
    let chart: TimelineChart | undefined
    let removePointerListeners: (() => void) | undefined
    const target = host.current
    void import('../lib/timeline-chart').then(({ initTimelineChart }) => {
      if (disposed) return
      chart = initTimelineChart(target)
      chartRef.current = chart
      observer = new ResizeObserver(() => chart?.resize())
      observer.observe(target)
      let dragStart: { x: number; pointerId: number } | undefined
      const dateAt = (clientX: number) => {
        const rect = target.getBoundingClientRect()
        const option = chart?.getOption() as { dataZoom?: Array<{ start?: number; end?: number; startValue?: number | string; endValue?: number | string }> } | undefined
        const zoom = option?.dataZoom?.[0]
        const toIndex = (value: number | string | undefined, percent: number | undefined, fallback: number) => {
          if (typeof value === 'number') return Math.round(value)
          if (typeof value === 'string') {
            const found = datesRef.current.indexOf(value)
            if (found >= 0) return found
          }
          return percent === undefined ? fallback : Math.round(percent / 100 * Math.max(0, datesRef.current.length - 1))
        }
        const startIndex = toIndex(zoom?.startValue, zoom?.start, 0)
        const endIndex = toIndex(zoom?.endValue, zoom?.end, datesRef.current.length - 1)
        const ratio = Math.max(0, Math.min(1, (clientX - rect.left - 42) / Math.max(1, rect.width - 60)))
        const index = Math.round(startIndex + ratio * Math.max(0, endIndex - startIndex))
        return datesRef.current[Math.max(0, Math.min(datesRef.current.length - 1, index))]
      }
      const drawSelection = (from: number, to: number) => {
        if (!selection.current) return
        const rect = target.getBoundingClientRect()
        selection.current.style.display = 'block'
        selection.current.style.left = `${Math.min(from, to) - rect.left}px`
        selection.current.style.width = `${Math.abs(to - from)}px`
      }
      const pointerDown = (event: PointerEvent) => {
        if (modeRef.current !== 'brush' || event.button !== 0) return
        dragStart = { x: event.clientX, pointerId: event.pointerId }
        target.setPointerCapture(event.pointerId)
        drawSelection(event.clientX, event.clientX)
        event.preventDefault()
      }
      const pointerMove = (event: PointerEvent) => {
        if (!dragStart || dragStart.pointerId !== event.pointerId) return
        drawSelection(dragStart.x, event.clientX)
      }
      const pointerUp = (event: PointerEvent) => {
        if (!dragStart || dragStart.pointerId !== event.pointerId) return
        const from = dateAt(dragStart.x)
        const to = dateAt(event.clientX)
        dragStart = undefined
        selection.current?.style.setProperty('display', 'none')
        if (from && to) filterRef.current(from < to ? from : to, from < to ? to : from)
      }
      target.addEventListener('pointerdown', pointerDown, true)
      target.addEventListener('pointermove', pointerMove, true)
      target.addEventListener('pointerup', pointerUp, true)
      target.addEventListener('pointercancel', pointerUp, true)
      removePointerListeners = () => {
        target.removeEventListener('pointerdown', pointerDown, true)
        target.removeEventListener('pointermove', pointerMove, true)
        target.removeEventListener('pointerup', pointerUp, true)
        target.removeEventListener('pointercancel', pointerUp, true)
      }
      setChartReady(true)
    })
    return () => { disposed = true; removePointerListeners?.(); observer?.disconnect(); chart?.dispose(); chartRef.current = null }
  }, [])

  useEffect(() => {
    const chart = chartRef.current
    if (!chart || !points.length) return
    const dates = points.map((item) => item.date)
    datesRef.current = dates
    const startValue = activeFrom && dates.includes(activeFrom) ? activeFrom : dates[0]
    const endValue = activeTo && dates.includes(activeTo) ? activeTo : dates.at(-1)
    chart.setOption({
      animation: false,
      grid: { left: 42, right: 18, top: 25, bottom: 72 },
      tooltip: {
        trigger: 'axis',
        axisPointer: { type: 'shadow' },
        formatter: (params: unknown) => {
          const rows = params as Array<{ axisValue: string; seriesName: string; value: number; color: string }>
          if (!rows.length) return ''
          return `<strong>${rows[0].axisValue}</strong><br/>${rows.map((row) => `${row.seriesName}：${Number(row.value).toLocaleString()}`).join('<br/>')}`
        },
      },
      xAxis: { type: 'category', data: dates, axisLabel: { color: '#8995a8', hideOverlap: true }, axisLine: { lineStyle: { color: '#344054' } } },
      yAxis: { type: 'value', minInterval: 1, axisLabel: { color: '#8995a8' }, splitLine: { lineStyle: { color: '#283447', type: 'dashed' } } },
      dataZoom: [
        { type: 'inside', xAxisIndex: 0, filterMode: 'none', startValue, endValue, zoomOnMouseWheel: true, moveOnMouseWheel: false, moveOnMouseMove: mode === 'pan', preventDefaultMouseMove: true },
        { type: 'slider', xAxisIndex: 0, filterMode: 'none', startValue, endValue, bottom: 12, height: 26, borderColor: '#344054', backgroundColor: '#101722', fillerColor: 'rgba(91, 150, 255, .18)', dataBackground: { lineStyle: { color: '#6fa0ee' }, areaStyle: { color: 'rgba(91, 150, 255, .18)' } }, selectedDataBackground: { lineStyle: { color: '#7ba9ff' }, areaStyle: { color: 'rgba(91, 150, 255, .3)' } }, textStyle: { color: '#8995a8' } },
      ],
      series: [
        { name: '全部引用', type: 'bar', data: points.map((item) => item.count), barGap: '-100%', barWidth: '90%', itemStyle: { color: 'transparent', borderColor: '#8793a6', borderType: 'dashed', borderWidth: 1 }, emphasis: { disabled: true }, z: 1 },
        { name: '当前条件', type: 'bar', data: points.map((item) => item.selected_count), barWidth: '90%', itemStyle: { color: '#5b96ff' }, z: 2 },
      ],
    }, true)
  }, [points, activeFrom, activeTo, mode, chartReady])

  const first = points[0]?.date ?? '无数据'
  const last = points.at(-1)?.date ?? '无数据'
  return <section className={styles.timeline}>
    <header className={styles.timelineHeader}>
      <div><h2>每日引用时间分布</h2><span>{first} → {last} · {points.length.toLocaleString()} 天</span></div>
      <div className={styles.timelineControls} role="group" aria-label="时间图表交互模式">
        <button className={mode === 'brush' ? styles.scaleActive : ''} type="button" aria-pressed={mode === 'brush'} onClick={() => setMode('brush')}>拖选过滤</button>
        <button className={mode === 'pan' ? styles.scaleActive : ''} type="button" aria-pressed={mode === 'pan'} onClick={() => setMode('pan')}>平移</button>
      </div>
    </header>
    <div className={styles.timelinePlot}><div ref={host} className={`${styles.timelineCanvas} ${mode === 'brush' ? styles.timelineBrush : styles.timelinePan}`} role="img" aria-label="每日引用直方图；鼠标滚轮缩放，拖动选择日期范围" /><div ref={selection} className={styles.timelineSelection} /></div>
    <footer className={styles.timelineLegend}><span><i /> 当前条件</span><span><i /> 全部引用</span><small>滚轮缩放到日 · 拖选日期区间过滤 · 底部滑块控制全局范围</small></footer>
  </section>
}

function continuousDays(timeline: ReferenceAnalysis['daily_timeline']) {
  const valid = timeline.filter((item) => /^\d{4}-\d{2}-\d{2}$/.test(item.date))
  if (!valid.length) return timeline
  const byDate = new Map(valid.map((item) => [item.date, item]))
  const day = 86_400_000
  const first = Date.parse(`${valid[0].date}T00:00:00Z`)
  const last = Date.parse(`${valid.at(-1)?.date ?? valid[0].date}T00:00:00Z`)
  return Array.from({ length: Math.floor((last - first) / day) + 1 }, (_, offset) => {
    const date = new Date(first + offset * day).toISOString().slice(0, 10)
    return byDate.get(date) ?? { date, count: 0, selected_count: 0 }
  })
}

function ReferenceCard({ item }: { item: ReferenceAnalysis['items'][number] }) {
  const [expandedContext, setExpandedContext] = useState(false)
  const [expandedMessage, setExpandedMessage] = useState(false)
  const message = item.message
  return <article className={styles.referenceCard}>
    <header><div><Avatar src={message.conversation.group_avatar_url} label={message.conversation.label} /><span><strong>{message.conversation.label}</strong><small>{conversationType(message.conversation.type, message.conversation.id)} · {message.table}</small></span></div><a href={item.chat_url}>跳到原消息 <ExternalLink size={14} /></a></header>
    <div className={styles.messageStack}>
      {expandedContext && item.context_before.map((context) => <ContextMessage key={context.rowid} message={context} muted compact />)}
      <ContextMessage message={message} compact={!expandedMessage} />
      {expandedContext && item.context_after.map((context) => <ContextMessage key={context.rowid} message={context} muted compact />)}
    </div>
    <footer><span>{message.datetime} · row {message.rowid}</span><div><button type="button" onClick={() => setExpandedMessage((value) => !value)}>{expandedMessage ? '紧凑显示本条' : '展开本条消息'}</button><button type="button" onClick={() => setExpandedContext((value) => !value)}>{expandedContext ? '收起上下文' : `展开前后消息 (${item.context_before.length}+${item.context_after.length})`}</button></div></footer>
  </article>
}

function ContextMessage({ message, muted = false, compact = false }: { message: ReferenceAnalysis['items'][number]['message']; muted?: boolean; compact?: boolean }) {
  const sender = message.display_sender_line || message.display_sender || message.sender_uin
  return <div aria-label={`${muted ? '上下文消息' : '引用消息'} row ${message.rowid}`} className={`${styles.contextMessage} ${muted ? styles.contextMuted : ''} ${compact ? styles.contextCompact : ''}`}>
    <Avatar src={message.avatar_url} label={sender} />
    <div><header><strong>{sender}</strong><time>{message.datetime}</time></header><RichMessage nodes={message.rich_nodes as RichNode[]} assets={message.assets as MediaAsset[]} fallback={message.display_text} mediaKind={message.media_kind} mediaLabel={message.media_label} unmatchedReason={message.unmatched_reason} /></div>
  </div>
}

function Avatar({ src, label }: { src?: string; label: string }) {
  const [failed, setFailed] = useState(false)
  return <span className={styles.avatar}>{src && !failed ? <img src={src} alt="" loading="lazy" onError={() => setFailed(true)} /> : label.trim().slice(0, 2) || '?'}</span>
}

function qlogo(uin: string) { return /^\d+$/.test(uin) ? `https://q1.qlogo.cn/g?b=qq&nk=${encodeURIComponent(uin)}&s=100` : '' }
function conversationType(type: string, id: string) { return type === 'group' ? `群聊 ${id}` : type === 'buddy' ? `私聊 ${id}` : type === 'discuss' ? `讨论组 ${id}` : `${type} ${id}` }
