import { useQuery } from '@tanstack/react-query'
import { createFileRoute, Link } from '@tanstack/react-router'
import { AlertCircle, ArrowLeft, Brain, Copy, ExternalLink, FileSearch, Hash, LoaderCircle, MessageSquare, ScanSearch } from 'lucide-react'
import { z } from 'zod'
import { api, assetDetailSchema, occurrenceSchema, queryString, searchSchema, type ImageSearchReport } from '../lib/api'
import { applySearchDuplicateMode } from '../lib/image-results'
import styles from './image-detail.module.css'

const detailSearchSchema = z.object({
  from: z.string().optional().catch(undefined),
  to: z.string().optional().catch(undefined),
  senders: z.string().optional().catch(undefined),
  tables: z.string().optional().catch(undefined),
  conversation_type: z.string().optional().catch(undefined),
})

export const Route = createFileRoute('/images/$assetId')({
  validateSearch: (search) => detailSearchSchema.parse(search),
  component: ImageDetailPage,
})

function ImageDetailPage() {
  const { assetId } = Route.useParams()
  const preservedFilters = Route.useSearch()
  const id = Number(assetId)
  const enabled = Number.isSafeInteger(id)
  const detail = useQuery({ queryKey: ['image-detail', id], queryFn: ({ signal }) => api(`/api/image-index/assets/${id}`, assetDetailSchema, { signal }), enabled })
  const occurrences = useQuery({ queryKey: ['image-occurrences', id], queryFn: ({ signal }) => api(`/api/image-index/assets/${id}/occurrences?limit=100`, occurrenceSchema, { signal }), enabled })
  const similarExact = useSimilar(id, 'exact', enabled)
  const similarCopy = useSimilar(id, 'copy', enabled)
  const similarSemantic = useSimilar(id, 'semantic', enabled)

  if (detail.isPending) return <main className={styles.state}><LoaderCircle className={styles.spin} /> 正在载入图片资料…</main>
  if (detail.isError) return <main className={styles.state}><AlertCircle /> {detail.error.message}</main>
  const asset = detail.data
  const copyResults = groupedWithoutCurrent(similarCopy.data, id)
  const semanticResults = groupedWithoutCurrent(similarSemantic.data, id)
  const exactCount = similarExact.data?.results.length ?? asset.exact_copy.count
  const scan = occurrences.data?.scan
  const occurrenceCount = occurrences.data?.items.length ?? 0
  const occurrenceLabel = `${occurrenceCount.toLocaleString()}${occurrences.data?.next_cursor ? '+' : ''}`

  return (
    <main className={styles.page}>
      <div className={styles.topbar}>
        <Link to="/images"><ArrowLeft size={16} /> 返回图库</Link>
        <Link to="/images" search={{ similar: id, signal: 'all' }}><ScanSearch size={16} /> 在图库查看全部相似结果</Link>
      </div>
      <section className={styles.lead}>
        <div className={styles.preview}><a href={asset.content_url} target="_blank" rel="noreferrer"><img src={asset.thumbnail_url} alt={`图片 ${id}`} /><ExternalLink size={16} /></a></div>
        <div className={styles.summary}>
          <span>图片 #{id}</span><h1>{asset.format || '图像'} · {asset.width ?? '?'} × {asset.height ?? '?'}</h1>
          <p>{sourceLabel(asset.source)} · {formatBytes(asset.file_size)} · 索引于 {asset.indexed_at}</p>
          {asset.quality_flags && <div className={styles.flags}>{asset.quality_flags}</div>}
          {asset.error && <div className={styles.error}>{asset.error}</div>}
          <div className={styles.metrics}><Metric icon={<Copy />} value={asset.exact_copy.count} label="完全相同文件" /><Metric icon={<Hash />} value={asset.embeddings.length} label="可用向量" /><Metric icon={<MessageSquare />} value={occurrenceLabel} label="聊天引用（当前 SHA 组）" /></div>
        </div>
      </section>

      <section className={styles.panel}>
        <h2>相似信号 <span>每种信号独立查询，数量不再由混合列表推断</span></h2>
        <div className={styles.signalGrid}>
          <Signal icon={<Hash />} label="像素与结构" value={exactCount + copyResults.length} pending={similarExact.isPending || similarCopy.isPending} />
          <Signal icon={<Brain />} label="语义 CLIP" value={semanticResults.length} pending={similarSemantic.isPending} error={similarSemantic.error} />
          <Signal icon={<FileSearch />} label="视觉副本 SSCD" value={copyResults.length} pending={similarCopy.isPending} error={similarCopy.error} />
          <Signal icon={<Copy />} label="完全相同" value={exactCount} pending={similarExact.isPending} error={similarExact.error} />
        </div>
      </section>

      <SimilarStrip assetId={id} title="视觉副本 SSCD" description="同一画面的缩放、压缩、裁剪或编辑副本" report={similarCopy.data} results={copyResults} pending={similarCopy.isPending} error={similarCopy.error} signal="pixel" />
      <SimilarStrip assetId={id} title="CLIP 语义相似" description="内容和语义接近，不等同于视觉副本" report={similarSemantic.data} results={semanticResults} pending={similarSemantic.isPending} error={similarSemantic.error} signal="semantic" />

      <div className={styles.columns}>
        <section className={styles.panel}><h2>技术元数据</h2><dl><Row label="SHA-256" value={asset.sha256} mono /><Row label="pHash" value={asset.phash || '不可用'} mono /><Row label="pHash 算法" value={asset.phash_algo} /><Row label="模糊度" value={asset.blur_score?.toFixed(3) ?? '不可用'} /><Row label="模糊度算法" value={asset.blur_algo} /><Row label="指纹版本" value={asset.fingerprint_version} /><Row label="分块哈希" value={String(asset.features.tile_hashes)} /><Row label="局部特征" value={String(asset.features.local_features)} /></dl></section>
        <section className={styles.panel}><h2>向量覆盖</h2>{asset.embeddings.length ? asset.embeddings.map((embedding) => <div className={styles.embedding} key={`${embedding.kind}-${embedding.model}`}><strong>{embedding.kind}</strong><span>{embedding.model}</span><small>{embedding.dimensions} 维 · {embedding.updated_at}</small></div>) : <p className={styles.muted}>该图片没有可用的模型向量。</p>}</section>
      </div>
      <section className={styles.panel}><h2>完全相同文件 <span>可节省 {formatBytes(asset.exact_copy.duplicate_bytes)}</span></h2><div className={styles.copies}>{asset.copies.map((copy) => <Link to="/images/$assetId" params={{ assetId: String(copy.id) }} key={copy.id}><img src={copy.thumbnail_url} alt="" loading="lazy" /><span>#{copy.id}{copy.representative ? ' · 代表文件' : ''}</span><small>{formatBytes(copy.file_size)} · {copy.width ?? '?'}×{copy.height ?? '?'}</small></Link>)}</div></section>
      <section className={styles.panel}>
        <h2>聊天位置 <span>{scan && !scan.complete ? '引用索引仍在扫描' : `${occurrenceLabel} 条`} · <Link to="/images/$assetId/references" params={{ assetId }} search={preservedFilters}>引用分析与上下文</Link></span></h2>
        {occurrences.isPending && <p className={styles.muted}>正在读取聊天引用…</p>}
        {occurrences.isError && <p className={styles.error}>{occurrences.error.message}</p>}
        {occurrences.data && !occurrences.data.items.length && <p className={styles.muted}>{scan && !scan.complete ? `当前仍有 ${scan.tables_incomplete} 张会话表未完成；空结果不代表没有聊天引用。` : '已完成的引用索引中没有匹配消息。'}</p>}
        <div className={styles.occurrences}>{occurrences.data?.items.map((item) => <a href={item.chat_url} key={`${item.table}-${item.rowid}`}><MessageSquare size={15} /><span>{conversationLabel(item.table)}</span><strong>row {item.rowid}</strong></a>)}</div>
      </section>
      <details className={styles.diagnostics}><summary>文件系统诊断</summary><dl><Row label="路径" value={asset.diagnostics.path} mono /><Row label="来源根目录" value={asset.diagnostics.source_root} mono /></dl></details>
    </main>
  )
}

function useSimilar(id: number, mode: 'exact' | 'copy' | 'semantic', enabled: boolean) {
  return useQuery({
    queryKey: ['image-similar-search', id, 'fast', mode],
    queryFn: ({ signal }) => api(`/api/image-index/assets/${id}/similar${queryString({ mode, strategy: 'fast', limit: 24 })}`, searchSchema, { signal }),
    enabled,
  })
}

function groupedWithoutCurrent(report: ImageSearchReport | undefined, id: number) {
  return report ? applySearchDuplicateMode(report.results.filter((result) => result.id !== id), 'variants') : []
}

function SimilarStrip({ assetId, title, description, report, results, pending, error, signal }: { assetId: number; title: string; description: string; report?: ImageSearchReport; results: ImageSearchReport['results']; pending: boolean; error: Error | null; signal: 'pixel' | 'semantic' }) {
  return <section className={styles.panel}>
    <h2>{title} <Link to="/images" search={{ similar: assetId, signal }}>在图库查看 {results.length} 张</Link></h2>
    <p className={styles.sectionDescription}>{description}；相同 pHash 画面已折叠。</p>
    {pending && <p className={styles.muted}><LoaderCircle className={styles.spin} /> 正在读取相似索引…</p>}
    {error && <p className={styles.error}>{error.message}</p>}
    {report && !results.length && !error && <p className={styles.muted}>没有找到其他结果。</p>}
    <div className={styles.similarGrid}>{results.slice(0, 12).map((result) => <Link to="/images/$assetId" params={{ assetId: String(result.id) }} key={result.id}><img src={result.thumbnail_url} alt={`图片 ${result.id}`} loading="lazy" /><span>#{result.id}</span><strong>{Math.round(result.score * 100)}%</strong></Link>)}</div>
  </section>
}

function Signal({ icon, value, label, pending, error }: { icon: React.ReactNode; value: number; label: string; pending: boolean; error?: Error | null }) { return <div className={error ? styles.signalError : ''}>{icon}<strong>{pending ? '…' : value.toLocaleString()}</strong><span>{label}</span><small>{error ? '不可用' : pending ? '读取索引' : '可用'}</small></div> }
function Metric({ icon, value, label }: { icon: React.ReactNode; value: number | string; label: string }) { return <div>{icon}<strong>{typeof value === 'number' ? value.toLocaleString() : value}</strong><span>{label}</span></div> }
function Row({ label, value, mono = false }: { label: string; value: string; mono?: boolean }) { return <><dt>{label}</dt><dd className={mono ? styles.mono : ''}>{value}</dd></> }
function sourceLabel(source: string) { const labels: Record<string, string> = { image: '聊天图片', legacy_image: '历史图片', video_thumbnail: '视频缩略图', file_recv: '接收文件', chat_pic: '聊天图片', emoji: '表情', avatar: '头像' }; return labels[source] || source || '未知来源' }
function conversationLabel(table: string) { if (table.startsWith('group_')) return `群聊 ${table.slice(6)}`; if (table.startsWith('buddy_') || table.startsWith('c2c_')) return `私聊 ${table.split('_').slice(1).join('_')}`; if (table.startsWith('discuss_')) return `讨论组 ${table.slice(8)}`; return table }
function formatBytes(value: number) { if (value < 1024) return `${value} B`; if (value < 1024 ** 2) return `${(value / 1024).toFixed(1)} KiB`; return `${(value / 1024 ** 2).toFixed(1)} MiB` }
