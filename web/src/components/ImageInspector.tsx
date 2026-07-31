import { useInfiniteQuery, useQuery } from '@tanstack/react-query'
import { Link } from '@tanstack/react-router'
import { AlertCircle, Brain, Copy, ExternalLink, FileSearch, Hash, Info, LoaderCircle, MessageSquare, X } from 'lucide-react'
import { api, assetDetailSchema, occurrenceSchema, queryString, type ImageSearchReport } from '../lib/api'
import { applySearchDuplicateMode, type DuplicateMode } from '../lib/image-results'
import styles from './ImageInspector.module.css'

export function ImageInspector({
  assetId,
  reports,
  indexOccurrenceCount,
  duplicateMode,
  onSignalChange,
  onClose,
}: {
  assetId: number
  reports: { exact?: ImageSearchReport; copy?: ImageSearchReport; semantic?: ImageSearchReport }
  indexOccurrenceCount?: number
  duplicateMode: DuplicateMode
  onSignalChange: (signal: 'all' | 'pixel' | 'semantic') => void
  onClose: () => void
}) {
  const detail = useQuery({
    queryKey: ['image-detail', assetId],
    queryFn: () => api(`/api/image-index/assets/${assetId}`, assetDetailSchema),
  })
  const occurrences = useInfiniteQuery({
    queryKey: ['image-occurrences', assetId],
    initialPageParam: undefined as string | undefined,
    queryFn: ({ pageParam }) => api(
      `/api/image-index/assets/${assetId}/occurrences${queryString({ cursor: pageParam, limit: 30 })}`,
      occurrenceSchema,
    ),
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    maxPages: 8,
  })
  const occurrenceItems = occurrences.data?.pages.flatMap((page) => page.items) ?? []
  const exactMatches = reports.exact ? applySearchDuplicateMode(reports.exact.results, duplicateMode).length : 0
  const copyMatches = reports.copy ? applySearchDuplicateMode(reports.copy.results.filter((item) => item.id !== assetId), duplicateMode).length : 0
  const semanticMatches = reports.semantic ? applySearchDuplicateMode(reports.semantic.results.filter((item) => item.id !== assetId), duplicateMode).length : 0
  const pixelMatches = exactMatches + copyMatches
  const unavailable = new Map(
    [reports.exact, reports.copy, reports.semantic]
      .flatMap((report) => report?.unavailable ?? [])
      .map((item) => [item.signal, item.reason]),
  )
  const scan = occurrences.data?.pages[0]?.scan

  return (
    <aside className={styles.panel} aria-label="图片详情与聊天位置">
      <header className={styles.header}>
        <div><span>当前图片</span><strong>#{assetId}</strong></div>
        <button type="button" onClick={onClose} aria-label="关闭图片面板"><X size={18} /></button>
      </header>

      {detail.isPending && <PanelState><LoaderCircle className={styles.spin} /> 正在载入图片资料…</PanelState>}
      {detail.isError && <PanelState error><AlertCircle /> {detail.error.message}</PanelState>}
      {detail.data && <>
        <a className={styles.preview} href={detail.data.content_url} target="_blank" rel="noreferrer">
          <img src={detail.data.thumbnail_url} alt={`图片 ${assetId} 预览`} />
          <span><ExternalLink size={13} /> 打开原图</span>
        </a>
        <div className={styles.identity}>
          <div>
            <strong>{detail.data.format || '图像'} · {detail.data.width ?? '?'} × {detail.data.height ?? '?'}</strong>
            <span>{sourceLabel(detail.data.source)} · {formatBytes(detail.data.file_size)}</span>
          </div>
          <Link to="/images/$assetId" params={{ assetId: String(assetId) }}><Info size={14} /> 技术详情</Link>
        </div>
        {detail.data.error && <div className={styles.error}><AlertCircle size={15} /> {detail.data.error}</div>}

        <section className={styles.section}>
          <div className={styles.sectionTitle}><h2>相似信号</h2>{(!reports.copy || !reports.semantic) && <span><LoaderCircle className={styles.spin} /> 排队 / 计算中</span>}</div>
          <div className={styles.signalGrid}>
            <Signal icon={<Hash />} label="像素与结构" value={pixelMatches} available={Boolean(detail.data.phash || detail.data.features.tile_hashes)} onClick={() => onSignalChange('pixel')} />
            <Signal icon={<Brain />} label="语义 CLIP" value={semanticMatches} available={detail.data.embeddings.some(isClip)} onClick={() => onSignalChange('semantic')} />
            <Signal icon={<FileSearch />} label="视觉副本 SSCD" value={copyMatches} available={detail.data.embeddings.some(isSscd)} onClick={() => onSignalChange('pixel')} />
            <Signal icon={<Copy />} label="完全相同" value={detail.data.exact_copy.count} available onClick={() => onSignalChange('pixel')} />
          </div>
          <p className={styles.signalHint}>点击信号只看对应结果；SSCD 与 CLIP 分开计算，不再混作同一种结果。</p>
          {unavailable.size > 0 && <div className={styles.unavailable}>
            {[...unavailable.entries()].map(([signal, reason]) => <div key={signal}><AlertCircle size={13} /><span><strong>{signalLabel(signal)}</strong>{reason}</span></div>)}
          </div>}
        </section>
      </>}

      <section className={`${styles.section} ${styles.chatSection}`}>
        <div className={styles.sectionTitle}>
          <h2>聊天位置</h2>
          <span><Link to="/images/$assetId/references" params={{ assetId: String(assetId) }}>引用分析</Link> · {occurrenceItems.length.toLocaleString()}{occurrences.hasNextPage ? '+' : ''} 条</span>
        </div>
        {occurrences.isPending && <p className={styles.muted}>正在读取聊天引用…</p>}
        {occurrences.isError && <div className={styles.error}><AlertCircle size={15} /> {occurrences.error.message}</div>}
        {!occurrences.isPending && !occurrenceItems.length && !occurrences.isError && (
          <div className={styles.emptyOccurrence}>
            <MessageSquare size={20} />
            <strong>{indexOccurrenceCount === 0 ? '聊天位置索引尚未建立' : scan && !scan.complete ? '聊天引用仍在增量扫描' : '暂未找到这张图片的聊天位置'}</strong>
            <span>{indexOccurrenceCount === 0 ? '运行 image-index link-chat 后，这里会列出原始消息。' : scan && !scan.complete ? `当前仍有 ${scan.tables_incomplete} 张会话表未完成；空结果不是“没有引用”。` : '已完成的引用索引中没有匹配消息。'}</span>
          </div>
        )}
        <div className={styles.occurrences}>
          {occurrenceItems.map((item) => (
            <a href={item.chat_url} key={`${item.table}-${item.rowid}`}>
              <MessageSquare size={15} />
              <span><strong>{conversationLabel(item.table)}</strong><small>{item.table}</small></span>
              <em>row {item.rowid}</em>
            </a>
          ))}
        </div>
        {occurrences.hasNextPage && <button className={styles.more} type="button" disabled={occurrences.isFetchingNextPage} onClick={() => void occurrences.fetchNextPage()}>
          {occurrences.isFetchingNextPage ? '正在载入…' : '载入更多聊天位置'}
        </button>}
      </section>
    </aside>
  )
}

function PanelState({ children, error = false }: { children: React.ReactNode; error?: boolean }) {
  return <div className={`${styles.state} ${error ? styles.stateError : ''}`}>{children}</div>
}

function Signal({ icon, label, value, available, onClick }: { icon: React.ReactNode; label: string; value: number; available: boolean; onClick: () => void }) {
  return <button type="button" className={!available ? styles.signalUnavailable : ''} onClick={onClick} disabled={!available}>{icon}<span>{label}</span><strong>{value}</strong><small>{available ? '点击筛选' : '尚无数据'}</small></button>
}

function isClip(embedding: { kind: string; model: string }) {
  return `${embedding.kind} ${embedding.model}`.toLowerCase().includes('clip')
}

function isSscd(embedding: { kind: string; model: string }) {
  return `${embedding.kind} ${embedding.model}`.toLowerCase().includes('sscd')
}

function signalLabel(signal: string) {
  const lower = signal.toLowerCase()
  if (lower.includes('clip')) return 'CLIP 语义模型不可用'
  if (lower.includes('sscd')) return 'SSCD 副本模型不可用'
  return `${signal} 不可用`
}

function sourceLabel(source: string) {
  const labels: Record<string, string> = { image: '聊天图片', legacy_image: '历史图片', video_thumbnail: '视频缩略图', file_recv: '接收文件', chat_pic: '聊天图片', emoji: '表情', avatar: '头像' }
  return labels[source] || source || '未知来源'
}

function conversationLabel(table: string) {
  if (table.startsWith('group_')) return `群聊 ${table.slice(6)}`
  if (table.startsWith('buddy_') || table.startsWith('c2c_')) return `私聊 ${table.split('_').slice(1).join('_')}`
  if (table.startsWith('discuss_')) return `讨论组 ${table.slice(8)}`
  return table
}

function formatBytes(value: number) {
  if (value < 1024) return `${value} B`
  if (value < 1024 ** 2) return `${(value / 1024).toFixed(1)} KiB`
  if (value < 1024 ** 3) return `${(value / 1024 ** 2).toFixed(1)} MiB`
  return `${(value / 1024 ** 3).toFixed(1)} GiB`
}
