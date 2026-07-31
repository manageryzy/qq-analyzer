import { useEffect, useMemo, useRef, useState } from 'react'
import { useInfiniteQuery, useMutation, useQuery } from '@tanstack/react-query'
import { createFileRoute, Link } from '@tanstack/react-router'
import { useVirtualizer } from '@tanstack/react-virtual'
import { AlertCircle, Brain, Filter as FilterIcon, ImageUp, Images, LoaderCircle, Search, SlidersHorizontal, TrendingUp, X } from 'lucide-react'
import { z } from 'zod'
import { ImageCard, type GalleryAsset } from '../components/ImageCard'
import { ImageInspector } from '../components/ImageInspector'
import { ApiResponseError, api, assetsPageSchema, overviewSchema, queryString, searchSchema, type ImageSearchReport } from '../lib/api'
import { applySearchDuplicateMode } from '../lib/image-results'
import styles from './images.module.css'

export const imageSearchSchema = z.object({
  format: z.string().optional().catch(undefined),
  source: z.string().optional().catch(undefined),
  quality: z.string().optional().catch(undefined),
  embeddings: z.enum(['any', 'clip', 'sscd', 'both', 'none']).optional().catch(undefined),
  references: z.enum(['with', 'without']).optional().catch(undefined),
  duplicates: z.enum(['variants', 'collapsed', 'all', 'duplicates', 'unique']).optional().catch(undefined),
  sort: z.enum(['newest', 'oldest', 'popular']).optional().catch(undefined),
  q: z.string().optional().catch(undefined),
  similar: z.coerce.number().int().positive().optional().catch(undefined),
  signal: z.enum(['all', 'pixel', 'semantic']).optional().catch(undefined),
  strategy: z.enum(['fast', 'exact']).optional().catch(undefined),
})

export const Route = createFileRoute('/images/')({
  validateSearch: (search) => imageSearchSchema.parse(search),
  component: ImagesPage,
})

function ImagesPage() {
  const search = Route.useSearch()
  const navigate = Route.useNavigate()
  const strategy = search.strategy ?? 'fast'
  const quality = search.quality ?? 'clean'
  const duplicates = search.duplicates ?? 'variants'
  const sort = search.sort ?? 'newest'
  const searchLimit = strategy === 'fast' ? 24 : 100
  const [queryDraft, setQueryDraft] = useState(search.q ?? '')
  const [uploadedName, setUploadedName] = useState<string>()
  const overview = useQuery({ queryKey: ['image-overview'], queryFn: () => api('/api/image-index/overview', overviewSchema) })
  const gallery = useInfiniteQuery({
    queryKey: ['image-assets', search.format, search.source, quality, search.embeddings, search.references, duplicates, sort],
    initialPageParam: undefined as string | undefined,
    queryFn: ({ pageParam, signal }) => api(`/api/image-index/assets${queryString({
      format: search.format,
      source: search.source,
      quality,
      embeddings: search.embeddings,
      references: search.references,
      duplicates,
      sort,
      cursor: pageParam,
      limit: 80,
    })}`, assetsPageSchema, { signal }),
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    maxPages: 8,
  })
  const textSearch = useQuery({
    queryKey: ['image-text-search', search.q, strategy],
    enabled: Boolean(search.q?.trim()) && !search.similar,
    queryFn: ({ signal }) => api(`/api/image-index/search/text${queryString({ q: search.q, strategy, limit: searchLimit })}`, searchSchema, { signal }),
  })
  const similarExact = useQuery({
    queryKey: ['image-similar-search', search.similar, strategy, 'exact'],
    enabled: Boolean(search.similar),
    queryFn: ({ signal }) => api(`/api/image-index/assets/${search.similar}/similar${queryString({ mode: 'exact', strategy, limit: searchLimit })}`, searchSchema, { signal }),
  })
  const similarCopy = useQuery({
    queryKey: ['image-similar-search', search.similar, strategy, 'copy'],
    enabled: Boolean(search.similar),
    queryFn: ({ signal }) => api(`/api/image-index/assets/${search.similar}/similar${queryString({ mode: 'copy', strategy, limit: searchLimit })}`, searchSchema, { signal }),
    retry: retryQueuedSearch,
    retryDelay: queuedSearchRetryDelay,
  })
  const similarSemantic = useQuery({
    queryKey: ['image-similar-search', search.similar, strategy, 'semantic'],
    enabled: Boolean(search.similar),
    queryFn: ({ signal }) => api(`/api/image-index/assets/${search.similar}/similar${queryString({ mode: 'semantic', strategy, limit: searchLimit })}`, searchSchema, { signal }),
    retry: retryQueuedSearch,
    retryDelay: queuedSearchRetryDelay,
  })
  const similarReport = useMemo(() => mergeSearchReports([similarExact.data, similarCopy.data, similarSemantic.data]), [similarCopy.data, similarExact.data, similarSemantic.data])
  const similarFetching = similarExact.isFetching || similarCopy.isFetching || similarSemantic.isFetching
  const upload = useMutation({
    mutationFn: async ({ file, queryStrategy }: { file: File; queryStrategy: 'fast' | 'exact' }) => {
      const searchMode = (mode: 'exact' | 'copy' | 'semantic') => {
        const body = new FormData()
        body.set('image', file)
        const limit = queryStrategy === 'fast' ? 24 : 100
        return api(`/api/image-index/search/image${queryString({ mode, strategy: queryStrategy, limit })}`, searchSchema, { method: 'POST', body })
      }
      const exact = await searchMode('exact')
      const copy = await searchMode('copy')
      const semantic = await searchMode('semantic')
      return mergeSearchReports([exact, copy, semantic])!
    },
  })

  useEffect(() => setQueryDraft(search.q ?? ''), [search.q])

  const galleryItems = gallery.data?.pages.flatMap((page) => page.items) ?? []
  const rawSearchItems = upload.data?.results ?? similarReport?.results ?? textSearch.data?.results
  const signal = search.signal ?? 'all'
  const searchedItems = rawSearchItems && applySearchDuplicateMode(
    rawSearchItems.filter((item) => signal === 'all' || (signal === 'semantic' ? isSemantic(item.match_kind) : !isSemantic(item.match_kind))),
    duplicates,
  )
  const items = (searchedItems ?? galleryItems) as GalleryAsset[]
  const activeSearch = Boolean(upload.data || upload.isPending || search.similar || search.q?.trim())
  const activeReport = upload.data ?? similarReport ?? textSearch.data
  const unavailable = activeReport?.unavailable ?? []
  const relevantLoading = upload.isPending || similarFetching || textSearch.isFetching || (!activeSearch && gallery.isPending)
  const relevantError = upload.error || textSearch.error || (!activeSearch ? gallery.error : null)
  const visibleSearchCount = rawSearchItems ? applySearchDuplicateMode(rawSearchItems, duplicates).length : 0
  const pixelCount = rawSearchItems ? applySearchDuplicateMode(rawSearchItems.filter((item) => !isSemantic(item.match_kind)), duplicates).length : 0
  const semanticCount = rawSearchItems ? applySearchDuplicateMode(rawSearchItems.filter((item) => isSemantic(item.match_kind)), duplicates).length : 0
  const hasItems = items.length > 0

  const scrollRef = useRef<HTMLDivElement>(null)
  const [columns, setColumns] = useState(5)
  useEffect(() => {
    const element = scrollRef.current
    if (!element) return
    const update = (width: number) => setColumns(width < 500 ? 2 : width < 740 ? 3 : width < 980 ? 4 : width < 1260 ? 5 : 6)
    update(element.clientWidth)
    const observer = new ResizeObserver((entries) => update(entries[0]?.contentRect.width ?? element.clientWidth))
    observer.observe(element)
    return () => observer.disconnect()
  }, [hasItems])
  const rowCount = Math.ceil(items.length / columns)
  const virtualizer = useVirtualizer({
    count: rowCount,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 250,
    overscan: 3,
  })
  const { fetchNextPage, hasNextPage, isFetchingNextPage } = gallery
  const requestedCursorRef = useRef<string | undefined>(undefined)
  const nearEndArmedRef = useRef(true)
  const requestNextGalleryPage = () => {
    const cursor = gallery.data?.pages.at(-1)?.next_cursor ?? undefined
    if (activeSearch || !hasNextPage || isFetchingNextPage || !cursor || requestedCursorRef.current === cursor) return
    requestedCursorRef.current = cursor
    void fetchNextPage().then((result) => {
      if (result.isError && requestedCursorRef.current === cursor) requestedCursorRef.current = undefined
    })
  }
  const galleryNearEnd = (element: HTMLDivElement) => element.scrollHeight - element.scrollTop - element.clientHeight <= 720
  const handleGalleryScroll = (event: React.UIEvent<HTMLDivElement>) => {
    const nearEnd = galleryNearEnd(event.currentTarget)
    if (!nearEnd) {
      nearEndArmedRef.current = true
    } else if (nearEndArmedRef.current) {
      nearEndArmedRef.current = false
      requestNextGalleryPage()
    }
  }
  const handleGalleryWheel = (event: React.WheelEvent<HTMLDivElement>) => {
    if (event.deltaY > 0 && galleryNearEnd(event.currentTarget)) requestNextGalleryPage()
  }
  useEffect(() => {
    requestedCursorRef.current = undefined
    nearEndArmedRef.current = true
  }, [search.format, search.source, quality, search.embeddings, search.references, duplicates, sort])
  useEffect(() => {
    scrollRef.current?.scrollTo({ top: 0 })
  }, [search.q, search.similar, search.signal, upload.data, search.format, search.source, quality, search.embeddings, search.references, duplicates, sort])

  const formatOptions = useMemo(() => overview.data?.distributions.format.filter((item) => item.count > 0).map((item) => item.key).filter(Boolean) ?? [], [overview.data])
  const sourceOptions = useMemo(() => overview.data?.distributions.source.filter((item) => item.count > 0).map((item) => item.key) ?? [], [overview.data])
  const qualityOptions = useMemo(() => overview.data?.distributions.quality.filter((item) => item.count > 0).map((item) => item.key) ?? [], [overview.data])
  const hasNonDefaultView = Boolean(search.format || search.source || (search.quality && search.quality !== 'clean') || search.embeddings || search.references || (search.duplicates && search.duplicates !== 'variants') || (search.sort && search.sort !== 'newest'))

  const updateSearch = (patch: Partial<z.infer<typeof imageSearchSchema>>, resetUpload = true) => {
    if (resetUpload) {
      upload.reset()
      setUploadedName(undefined)
    }
    void navigate({ search: (previous) => ({ ...previous, ...patch }), replace: true })
  }
  const selectAsset = (asset: GalleryAsset) => {
    upload.reset()
    setUploadedName(undefined)
    void navigate({ search: (previous) => ({ ...previous, q: undefined, similar: asset.id, signal: 'all' }), replace: true })
  }
  const clearSearch = () => {
    upload.reset()
    setUploadedName(undefined)
    setQueryDraft('')
    updateSearch({ q: undefined, similar: undefined, signal: undefined }, false)
  }

  return (
    <main className={styles.page}>
      <header className={styles.header}>
        <div className={styles.title}><Images size={20} /><div><h1>图像图库</h1><span>搜索、相似分析与聊天溯源</span></div></div>
        <div className={styles.stats} aria-label="图像索引概览">
          <Link to="/images/trends" className={styles.trendsLink}><TrendingUp /> 热门趋势</Link>
          <Stat value={overview.data?.health.assets} label="图片" />
          <Stat value={overview.data?.coverage.clip_assets} label="CLIP" />
          <Stat value={overview.data?.coverage.occurrences} label="聊天位置" />
          <Stat value={overview.data?.exact_copies.duplicate_files} label="重复文件" />
        </div>
      </header>

      <section className={styles.toolbar} aria-label="图像搜索与筛选">
        <form className={styles.search} onSubmit={(event) => {
          event.preventDefault()
          const q = queryDraft.trim()
          updateSearch({ q: q || undefined, similar: undefined, signal: undefined })
        }}>
          <Brain size={16} />
          <input type="search" aria-label="语义文字搜索" placeholder="用自然语言搜索，例如：海边的日落、聊天截图…" value={queryDraft} onChange={(event) => setQueryDraft(event.target.value)} />
          <button type="submit" aria-label="搜索"><Search size={16} /></button>
        </form>
        <label className={styles.upload}>
          <ImageUp size={16} /><span>{upload.isPending ? '检索排队 / 计算中…' : uploadedName || '上传图片搜索'}</span>
          <input type="file" accept="image/*" disabled={upload.isPending} onChange={(event) => {
            const file = event.target.files?.[0]
            event.currentTarget.value = ''
            if (!file) return
            setUploadedName(file.name)
            setQueryDraft('')
            void navigate({ search: (previous) => ({ ...previous, q: undefined, similar: undefined, signal: 'all' }), replace: true })
            upload.mutate({ file, queryStrategy: strategy })
          }} />
        </label>
        <div className={styles.strategy} aria-label="相似检索精度">
          <SlidersHorizontal size={14} />
          <button type="button" className={strategy === 'fast' ? styles.active : ''} onClick={() => updateSearch({ strategy: 'fast' }, false)}>快速</button>
          <button type="button" className={strategy === 'exact' ? styles.active : ''} onClick={() => updateSearch({ strategy: 'exact' }, false)}>精确</button>
        </div>
        <div className={styles.filters}><FilterIcon size={14} />
          <Filter value={search.format} label="全部格式" options={formatOptions} onChange={(format) => updateSearch({ format })} />
          <Filter value={search.source} label="全部来源" options={sourceOptions} onChange={(source) => updateSearch({ source })} />
          <Filter value={quality} label="默认质量筛选" options={['all', ...qualityOptions]} optionLabels={{ all: '全部质量', clean: '仅高质量（默认）', thumbnail: '仅缩略图', tiny: '微小图片', small: '小图', blurry: '模糊图片' }} onChange={(quality) => updateSearch({ quality })} />
          <Filter value={search.embeddings} label="全部向量" options={['clip', 'sscd', 'both', 'none']} optionLabels={{ clip: '有 CLIP', sscd: '有 SSCD', both: 'CLIP + SSCD', none: '无向量' }} onChange={(embeddings) => updateSearch({ embeddings: embeddings as z.infer<typeof imageSearchSchema>['embeddings'] })} />
          <Filter value={search.references} label="全部聊天引用" options={['with', 'without']} optionLabels={{ with: '有聊天位置', without: '无聊天位置' }} onChange={(references) => updateSearch({ references: references as z.infer<typeof imageSearchSchema>['references'] })} />
          <Filter value={duplicates} label="副本显示" options={['variants', 'collapsed', 'all', 'duplicates', 'unique']} optionLabels={{ variants: '合并相同画面（默认）', collapsed: '仅折叠完全副本', all: '显示全部文件', duplicates: '仅完全重复组', unique: '仅无完全副本' }} onChange={(duplicates) => updateSearch({ duplicates: duplicates as z.infer<typeof imageSearchSchema>['duplicates'] })} />
          <select aria-label="图库排序" value={sort} onChange={(event) => updateSearch({ sort: event.target.value as z.infer<typeof imageSearchSchema>['sort'] })}>
            <option value="newest">最新入库（默认）</option>
            <option value="popular">聊天引用最多</option>
            <option value="oldest">最早入库</option>
          </select>
          {hasNonDefaultView && <button type="button" className={styles.resetFilters} onClick={() => updateSearch({ format: undefined, source: undefined, quality: undefined, embeddings: undefined, references: undefined, duplicates: undefined, sort: undefined })}><X size={13} /> 恢复默认</button>}
        </div>
      </section>

      <div className={`${styles.workspace} ${search.similar ? styles.withInspector : ''}`}>
        <section className={styles.galleryPanel} aria-label="可搜索图像图库">
          <header className={styles.galleryHeader}>
            <div><strong>{resultTitle(search.similar, search.q, uploadedName)}</strong><span>{items.length.toLocaleString()}{!activeSearch && hasNextPage ? '+' : ''} 已载入</span></div>
            {search.similar && <div className={styles.tabs} aria-label="相似信号筛选">
              <Tab active={signal === 'all'} onClick={() => updateSearch({ signal: 'all' }, false)}>全部 {visibleSearchCount}</Tab>
              <Tab active={signal === 'pixel'} onClick={() => updateSearch({ signal: 'pixel' }, false)}>像素 / 结构 {pixelCount}</Tab>
              <Tab active={signal === 'semantic'} onClick={() => updateSearch({ signal: 'semantic' }, false)}>语义 {semanticCount}</Tab>
            </div>}
            {activeSearch && <button type="button" className={styles.clear} onClick={clearSearch}><X size={14} /> 返回全部图库</button>}
          </header>

          {search.similar && <div className={styles.searchProgress} aria-label="相似检索进度" aria-live="polite">
            <SearchStage label="完全副本" pending={similarExact.isPending} error={similarExact.error} count={similarExact.data?.results.length} />
            <SearchStage label="像素 / SSCD" pending={similarCopy.isPending} error={similarCopy.error} count={similarCopy.data?.results.length} />
            <SearchStage label="CLIP 语义" pending={similarSemantic.isPending} error={similarSemantic.error} count={similarSemantic.data?.results.length} />
          </div>}
          {overview.data?.coverage.occurrences === 0 && !search.similar && <div className={styles.notice}><MessageNotice />聊天位置索引尚未建立；图库仍可浏览和检索，执行 <code>image-index link-chat</code> 后可从图片跳回消息。</div>}
          {unavailable.map((item) => <div className={styles.warning} key={item.signal}><AlertCircle size={14} /><span><strong>{signalName(item.signal)}</strong>：{item.reason}</span></div>)}
          {relevantError && <div className={styles.stateError}><AlertCircle /> {relevantError.message}</div>}
          {relevantLoading && !items.length && <div className={styles.state}><LoaderCircle className={styles.spin} /> {search.similar ? '正在读取相似索引…' : upload.isPending ? '上传图片正在排队 / 计算…' : '正在载入图库…'}</div>}
          {!relevantLoading && !items.length && !relevantError && <div className={styles.state}><Images /> 没有符合当前条件的图片。</div>}
          {items.length > 0 && <div ref={scrollRef} className={styles.galleryScroll} tabIndex={0} aria-label="图像列表" onScroll={handleGalleryScroll} onWheel={handleGalleryWheel}>
            <div className={styles.virtualCanvas} style={{ height: virtualizer.getTotalSize() }}>
              {virtualizer.getVirtualItems().map((row) => (
                <div key={row.key} className={styles.gridRow} style={{ transform: `translateY(${row.start}px)`, gridTemplateColumns: `repeat(${columns}, minmax(0, 1fr))` }}>
                  {items.slice(row.index * columns, row.index * columns + columns).map((asset) => <ImageCard key={asset.id} asset={asset} selected={search.similar === asset.id} onSelect={selectAsset} />)}
                </div>
              ))}
            </div>
            {!activeSearch && gallery.isFetchingNextPage && <div className={styles.loadingMore}><LoaderCircle className={styles.spin} /> 懒加载下一批图片…</div>}
          </div>}
        </section>

        {search.similar && <ImageInspector
          assetId={search.similar}
          reports={{ exact: similarExact.data, copy: similarCopy.data, semantic: similarSemantic.data }}
          indexOccurrenceCount={overview.data?.coverage.occurrences}
          duplicateMode={duplicates}
          onSignalChange={(signal) => updateSearch({ signal }, false)}
          onClose={() => updateSearch({ similar: undefined, signal: undefined }, false)}
        />}
      </div>
    </main>
  )
}

function Stat({ value, label }: { value?: number; label: string }) {
  return <div><strong>{value === undefined ? '—' : compactNumber(value)}</strong><span>{label}</span></div>
}

function Filter({ value, label, options, optionLabels = {}, onChange }: { value?: string; label: string; options: string[]; optionLabels?: Record<string, string>; onChange: (value?: string) => void }) {
  return <select aria-label={label} value={value ?? ''} onChange={(event) => onChange(event.target.value || undefined)}><option value="">{label}</option>{options.map((option) => <option value={option} key={option}>{optionLabels[option] ?? option}</option>)}</select>
}

function Tab({ active, onClick, children }: { active: boolean; onClick: () => void; children: React.ReactNode }) {
  return <button type="button" className={active ? styles.active : ''} onClick={onClick}>{children}</button>
}

function MessageNotice() {
  return <AlertCircle size={14} />
}

function SearchStage({ label, pending, error, count }: { label: string; pending: boolean; error: Error | null; count?: number }) {
  if (pending) return <span className={styles.searchStagePending}><LoaderCircle className={styles.spin} /> {label}读取索引</span>
  if (error) return <span className={styles.searchStageError}><AlertCircle /> {label}失败</span>
  return <span className={styles.searchStageReady}>{label} {count ?? 0}</span>
}

function mergeSearchReports(reports: Array<ImageSearchReport | undefined>): ImageSearchReport | undefined {
  const available = reports.filter((report): report is ImageSearchReport => Boolean(report))
  if (!available.length) return undefined

  const [exact, copy, semantic] = reports.map((report) => report?.results ?? [])
  const results: ImageSearchReport['results'] = []
  const seen = new Set<number>()
  const append = (item: ImageSearchReport['results'][number] | undefined) => {
    if (!item || seen.has(item.id)) return
    seen.add(item.id)
    results.push(item)
  }
  exact.forEach(append)
  for (let index = 0; index < Math.max(copy.length, semantic.length); index += 1) {
    append(copy[index])
    append(semantic[index])
  }

  const unavailable = Array.from(
    new Map(available.flatMap((report) => report.unavailable).map((item) => [`${item.signal}\n${item.reason}`, item])).values(),
  )
  return { ...available[0], results, unavailable }
}

function isSemantic(matchKind: string) {
  return matchKind === 'semantic_clip'
}

function retryQueuedSearch(failureCount: number, error: Error) {
  if (error instanceof ApiResponseError && error.status === 429) return failureCount < 12
  return failureCount < 1
}

function queuedSearchRetryDelay(attempt: number, error: Error) {
  return error instanceof ApiResponseError && error.status === 429
    ? Math.min(5_000 + attempt * 2_000, 15_000)
    : 1_000
}

function signalName(signal: string) {
  const lower = signal.toLowerCase()
  if (lower.includes('clip')) return 'CLIP 语义检索不可用'
  if (lower.includes('sscd')) return 'SSCD 视觉副本检索不可用'
  if (lower.includes('pixel') || lower.includes('hash')) return '像素 / 结构检索不可用'
  return `${signal} 不可用`
}

function resultTitle(similar?: number, q?: string, uploadedName?: string) {
  if (similar) return `与图片 #${similar} 相似`
  if (uploadedName) return `以图搜图 · ${uploadedName}`
  if (q) return `语义搜索 · ${q}`
  return '全部索引图片'
}

function compactNumber(value: number) {
  return new Intl.NumberFormat('zh-CN', { notation: 'compact', maximumFractionDigits: 1 }).format(value)
}
