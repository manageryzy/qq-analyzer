import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { useInfiniteQuery, useQuery } from '@tanstack/react-query'
import { createFileRoute } from '@tanstack/react-router'
import { useVirtualizer } from '@tanstack/react-virtual'
import {
  AlertCircle,
  ArrowDown,
  ArrowUp,
  ChevronLeft,
  ChevronRight,
  ChevronsLeft,
  ChevronsRight,
  LoaderCircle,
  Search,
} from 'lucide-react'
import { z } from 'zod'
import { RichMessage, type MediaAsset, type RichNode } from '../components/RichMessage'
import { api, queryString } from '../lib/api'
import styles from './chat.module.css'

const PAGE_SIZE = 20
const STREAM_PAGE_CAP = 8

export const chatSearchSchema = z.object({
  table: z.string().optional().catch(undefined),
  rowid: z.coerce.number().int().positive().optional().catch(undefined),
  offset: z.coerce.number().int().positive().optional().catch(undefined),
  mode: z.enum(['paged', 'stream']).optional().catch(undefined),
  q: z.string().optional().catch(undefined),
  type: z.enum(['buddy', 'group', 'discuss', 'system']).optional().catch(undefined),
})

const conversationSchema = z.object({
  table: z.string(),
  key: z.string().optional(),
  label: z.string().optional(),
  type: z.string().optional(),
  id: z.string().optional(),
  rows: z.number().optional(),
  rows_is_estimate: z.boolean().optional(),
  last_datetime: z.string().optional(),
  label_source_basic: z.string().optional(),
  label_source: z.string().optional(),
  group_memo: z.string().optional(),
  group_avatar_url: z.string().optional(),
  group_avatar_candidates: z.array(z.string()).optional(),
  group_avatar_reason: z.string().optional(),
}).passthrough()

type Conversation = z.infer<typeof conversationSchema>

const conversationsSchema = z.object({
  total: z.number(),
  offset: z.number(),
  limit: z.number(),
  items: z.array(conversationSchema),
}).passthrough()

const conversationDetailsSchema = z.object({ items: z.array(conversationSchema) })

const assetSchema = z.object({
  href: z.string().optional(),
  path: z.string().optional(),
  kind: z.string().optional(),
  name: z.string().optional(),
  image_index_id: z.number().optional(),
  similar_href: z.string().optional(),
}).passthrough()

const messageSchema = z.object({
  table: z.string(),
  rowid: z.number(),
  datetime: z.string().optional(),
  sender_uin: z.string().optional(),
  display_sender: z.string().optional(),
  display_sender_line: z.string().optional(),
  sender_identity_note: z.string().optional(),
  avatar_url: z.string().optional(),
  display_text: z.string().default(''),
  rich_nodes: z.array(z.record(z.string(), z.unknown())).optional(),
  assets: z.array(assetSchema).optional(),
  is_self: z.boolean().optional(),
  unmatched_reason: z.string().optional(),
  media_kind: z.string().optional(),
  media_label: z.string().optional(),
  candidate_path_count: z.number().optional(),
  candidate_path_hit_count: z.number().optional(),
  style_meta: z.record(z.string(), z.unknown()).optional(),
}).passthrough()

type ChatMessage = z.infer<typeof messageSchema>

const messagesSchema = z.object({
  items: z.array(messageSchema),
  total: z.number(),
  max_rowid: z.number(),
  offset: z.number(),
  limit: z.number(),
  first_rowid: z.number(),
  last_rowid: z.number(),
  next_offset: z.number(),
  prev_offset: z.number(),
  has_next: z.boolean(),
  has_prev: z.boolean(),
}).passthrough()

type MessagesPage = z.infer<typeof messagesSchema>
type PageParam = { cursor: number; before: boolean }

export const Route = createFileRoute('/chat')({
  validateSearch: (search) => chatSearchSchema.parse(search),
  component: ChatPage,
})

function conversationName(conversation?: Conversation) {
  return conversation?.label || conversation?.table || '选择会话'
}

function qlogo(uin?: string) {
  return uin ? `https://q1.qlogo.cn/g?b=qq&nk=${encodeURIComponent(uin)}&s=100` : ''
}

function conversationAvatar(conversation?: Conversation) {
  if (!conversation) return ''
  return conversation.type === 'buddy' ? qlogo(conversation.id) : conversation.group_avatar_url || ''
}

function Avatar({ src, label, className }: { src?: string; label: string; className: string }) {
  const [failedSrc, setFailedSrc] = useState<string>()
  return (
    <span className={className} aria-hidden="true" data-testid="avatar">
      {src && failedSrc !== src
        ? <img src={src} alt="" loading="lazy" onError={() => setFailedSrc(src)} />
        : label.trim().slice(0, 2) || '?'}
    </span>
  )
}

function ChatPage() {
  const search = Route.useSearch()
  const navigate = Route.useNavigate()
  const mode = search.mode ?? 'paged'
  const cursor = search.rowid ?? search.offset ?? 1
  const [queryInput, setQueryInput] = useState(search.q ?? '')
  const [pageInput, setPageInput] = useState(1)
  const [progressPreview, setProgressPreview] = useState<number>()

  const updateSearch = useCallback((patch: Partial<z.infer<typeof chatSearchSchema>>, replace = true) => {
    void navigate({ search: (previous) => ({ ...previous, ...patch }), replace })
  }, [navigate])

  useEffect(() => setQueryInput(search.q ?? ''), [search.q])
  useEffect(() => {
    const timer = window.setTimeout(() => {
      const value = queryInput.trim() || undefined
      if (value !== search.q) updateSearch({ q: value })
    }, 220)
    return () => window.clearTimeout(timer)
  }, [queryInput, search.q, updateSearch])

  const conversations = useQuery({
    queryKey: ['conversations', search.type, search.q],
    queryFn: () => api(
      `/api/conversations${queryString({ limit: 200, type: search.type, q: search.q })}`,
      conversationsSchema,
    ),
  })
  const conversationItems = useMemo(() => conversations.data?.items ?? [], [conversations.data?.items])
  const conversationParent = useRef<HTMLDivElement>(null)
  const conversationVirtualizer = useVirtualizer({
    count: conversationItems.length,
    getScrollElement: () => conversationParent.current,
    estimateSize: () => 66,
    overscan: 7,
  })
  const conversationVirtualItems = conversationVirtualizer.getVirtualItems()
  const detailTables = useMemo(() => {
    const values = conversationVirtualItems
      .map((item) => conversationItems[item.index]?.table)
      .filter((table): table is string => Boolean(table))
    if (search.table) values.push(search.table)
    return [...new Set(values)].slice(0, 40)
  }, [conversationItems, conversationVirtualItems, search.table])
  const details = useQuery({
    queryKey: ['conversation-details', detailTables.join(',')],
    enabled: detailTables.length > 0,
    queryFn: () => api(
      `/api/conversation_details${queryString({ tables: detailTables.join(',') })}`,
      conversationDetailsSchema,
    ),
    staleTime: Number.POSITIVE_INFINITY,
  })
  const detailMap = useMemo(
    () => new Map(details.data?.items.map((item) => [item.table, item]) ?? []),
    [details.data?.items],
  )
  const selectedLight = conversationItems.find((item) => item.table === search.table)
  const selected = detailMap.get(search.table ?? '') ?? selectedLight

  const pagedMessages = useQuery({
    queryKey: ['messages-page', search.table, cursor],
    enabled: Boolean(search.table) && mode === 'paged',
    queryFn: () => fetchMessages(search.table!, { cursor, before: false }),
    // Keep the previous page's max_rowid/total (and rendered messages) while a
    // cursor change is in flight. Without this, the range control briefly saw
    // the schema defaults and flashed to 0% on every navigation.
    placeholderData: (previousData, previousQuery) => (
      previousQuery?.queryKey[1] === search.table ? previousData : undefined
    ),
  })
  const streamMessages = useInfiniteQuery({
    queryKey: ['messages-stream', search.table, cursor],
    enabled: Boolean(search.table) && mode === 'stream',
    initialPageParam: { cursor, before: false } as PageParam,
    queryFn: ({ pageParam }) => fetchMessages(search.table!, pageParam),
    getPreviousPageParam: (first) => first.has_prev && first.first_rowid > 0
      ? { cursor: first.first_rowid, before: true }
      : undefined,
    getNextPageParam: (last) => last.has_next && last.last_rowid > 0
      ? { cursor: last.next_offset, before: false }
      : undefined,
    maxPages: STREAM_PAGE_CAP,
  })

  const currentPage = mode === 'paged' ? pagedMessages.data : streamMessages.data?.pages[0]
  const messageItems = useMemo(() => {
    const items = mode === 'paged'
      ? pagedMessages.data?.items ?? []
      : streamMessages.data?.pages.flatMap((page) => page.items) ?? []
    const unique = new Map(items.map((item) => [item.rowid, item]))
    return [...unique.values()].sort((left, right) => left.rowid - right.rowid)
  }, [mode, pagedMessages.data?.items, streamMessages.data?.pages])

  const messageParent = useRef<HTMLDivElement>(null)
  const prependSnapshot = useRef<{ height: number; top: number } | undefined>(undefined)
  const messageVirtualizer = useVirtualizer({
    count: messageItems.length,
    getScrollElement: () => messageParent.current,
    estimateSize: () => 150,
    overscan: 5,
    getItemKey: (index) => messageItems[index]?.rowid ?? index,
  })

  const loadOlder = async () => {
    const element = messageParent.current
    if (!element || !streamMessages.hasPreviousPage || streamMessages.isFetchingPreviousPage) return
    prependSnapshot.current = { height: element.scrollHeight, top: element.scrollTop }
    await streamMessages.fetchPreviousPage()
  }

  useLayoutEffect(() => {
    const snapshot = prependSnapshot.current
    const element = messageParent.current
    if (!snapshot || !element || streamMessages.isFetchingPreviousPage) return
    element.scrollTop = snapshot.top + Math.max(0, element.scrollHeight - snapshot.height)
    prependSnapshot.current = undefined
  }, [messageItems.length, streamMessages.isFetchingPreviousPage])

  useEffect(() => {
    if (!search.rowid || !messageItems.length) return
    const index = messageItems.findIndex((message) => message.rowid === search.rowid)
    if (index >= 0) requestAnimationFrame(() => messageVirtualizer.scrollToIndex(index, { align: 'center' }))
  }, [messageItems, messageVirtualizer, search.rowid])

  const goCursor = (nextCursor: number) => {
    setProgressPreview(undefined)
    updateSearch({ offset: Math.max(1, Math.floor(nextCursor)), rowid: undefined })
    requestAnimationFrame(() => messageParent.current?.scrollTo({ top: 0 }))
  }
  const maxRowid = pagedMessages.data?.max_rowid ?? currentPage?.max_rowid ?? 1
  const total = pagedMessages.data?.total ?? currentPage?.total ?? 0
  const lastPageCursor = Math.max(1, maxRowid - PAGE_SIZE + 1)
  const pageCount = Math.max(1, Math.ceil(total / PAGE_SIZE))
  const progressValue = progressPreview
    ?? Math.round(((Math.min(cursor, lastPageCursor) - 1) / Math.max(1, lastPageCursor - 1)) * 1000)
  const commitProgress = (value: number) => {
    const position = Math.max(0, Math.min(1000, value))
    goCursor(Math.floor((position / 1000) * Math.max(0, lastPageCursor - 1)) + 1)
  }

  useEffect(() => {
    const derived = cursor >= lastPageCursor
      ? pageCount
      : Math.max(1, Math.floor((cursor - 1) / PAGE_SIZE) + 1)
    setPageInput(derived)
  }, [cursor, lastPageCursor, pageCount])
  const pending = mode === 'paged' ? pagedMessages.isPending : streamMessages.isPending
  const error = mode === 'paged' ? pagedMessages.error : streamMessages.error

  return (
    <main className={styles.layout}>
      <aside className={styles.sidebar}>
        <div className={styles.sidebarTitle}>
          <strong>会话</strong>
          <span>{conversations.data ? `${conversationItems.length}/${conversations.data.total}` : '…'}</span>
        </div>
        <div className={styles.conversationFilters}>
          <label><Search size={15} /><input aria-label="搜索会话" value={queryInput} placeholder="搜索会话" onChange={(event) => setQueryInput(event.target.value)} /></label>
          <div role="group" aria-label="会话类型">
            {[
              ['', '全部'],
              ['buddy', '私聊'],
              ['group', '群聊'],
              ['discuss', '讨论组'],
            ].map(([value, label]) => (
              <button
                type="button"
                key={value}
                className={(search.type ?? '') === value ? styles.active : ''}
                onClick={() => updateSearch({ type: (value || undefined) as z.infer<typeof chatSearchSchema>['type'] })}
              >{label}</button>
            ))}
          </div>
        </div>
        {conversations.isPending && <State icon={<LoaderCircle className={styles.spin} />} text="正在读取会话索引…" />}
        {conversations.isError && <State icon={<AlertCircle />} text={conversations.error.message} danger />}
        <div ref={conversationParent} className={styles.conversationScroll}>
          <div style={{ height: conversationVirtualizer.getTotalSize(), position: 'relative' }}>
            {conversationVirtualItems.map((virtual) => {
              const light = conversationItems[virtual.index]
              const item = detailMap.get(light.table) ?? light
              const label = conversationName(item)
              return (
                <button
                  type="button"
                  key={item.table}
                  className={`${styles.conversation} ${item.table === search.table ? styles.selected : ''}`}
                  style={{ transform: `translateY(${virtual.start}px)` }}
                  onClick={() => updateSearch({ table: item.table, offset: 1, rowid: undefined }, false)}
                >
                  <Avatar src={conversationAvatar(item)} label={label} className={styles.conversationAvatar} />
                  <span className={styles.conversationText}>
                    <strong>{label}</strong>
                    <small>{item.type} {item.id} · {item.rows_is_estimate ? '≈' : ''}{item.rows?.toLocaleString() ?? 0} 条 · {item.last_datetime}</small>
                  </span>
                  <ChevronRight size={15} />
                </button>
              )
            })}
          </div>
        </div>
      </aside>

      <section className={styles.thread} aria-label="Chat messages">
        <header className={styles.threadHeader}>
          <Avatar src={conversationAvatar(selected)} label={conversationName(selected)} className={styles.titleAvatar} />
          <div>
            <strong>{conversationName(selected)}</strong>
            <small>{search.table ? `${search.table} · ${total.toLocaleString()} 条${selected?.group_memo ? ` · ${selected.group_memo}` : ''}` : '从左侧选择会话'}</small>
          </div>
        </header>

        {search.table && (
          <nav className={styles.messageToolbar} aria-label="消息导航">
            <div className={styles.modeSwitch}>
              <button type="button" className={mode === 'paged' ? styles.active : ''} onClick={() => updateSearch({ mode: 'paged' })}>分页</button>
              <button type="button" className={mode === 'stream' ? styles.active : ''} onClick={() => updateSearch({ mode: 'stream' })}>连续</button>
            </div>
            <button type="button" title="首页" onClick={() => goCursor(1)}><ChevronsLeft size={16} /></button>
            <button type="button" title="上一页" disabled={cursor <= 1} onClick={() => goCursor(cursor - PAGE_SIZE)}><ChevronLeft size={16} /></button>
            <form onSubmit={(event) => {
              event.preventDefault()
              const requested = (Math.min(pageCount, Math.max(1, pageInput)) - 1) * PAGE_SIZE + 1
              goCursor(Math.min(lastPageCursor, requested))
            }}>
              <input aria-label="页码" type="number" min={1} max={pageCount} value={pageInput} onChange={(event) => setPageInput(Number(event.target.value))} />
              <span>/ {pageCount}</span>
            </form>
            <button type="button" title="下一页" disabled={cursor >= lastPageCursor} onClick={() => goCursor(Math.min(lastPageCursor, cursor + PAGE_SIZE))}><ChevronRight size={16} /></button>
            <button type="button" title="末页" onClick={() => goCursor(lastPageCursor)}><ChevronsRight size={16} /></button>
            <input
              className={styles.progress}
              aria-label="消息进度"
              type="range"
              min={0}
              max={1000}
              value={progressValue}
              onChange={(event) => setProgressPreview(Number(event.target.value))}
              onPointerUp={(event) => commitProgress(Number(event.currentTarget.value))}
              onKeyUp={(event) => commitProgress(Number(event.currentTarget.value))}
              onPointerCancel={() => setProgressPreview(undefined)}
            />
            <span>{Math.round(progressValue / 10)}%</span>
          </nav>
        )}

        {!search.table && <State text="请选择一个会话" />}
        {pending && <State icon={<LoaderCircle className={styles.spin} />} text="正在加载消息…" />}
        {error && <State icon={<AlertCircle />} text={error.message} danger />}
        {!pending && !error && search.table && !messageItems.length && <State text="这个位置没有消息" />}
        {messageItems.length > 0 && (
          <div
            ref={messageParent}
            className={styles.messageScroll}
            aria-label="消息列表"
            onScroll={(event) => {
              if (mode !== 'stream') return
              const element = event.currentTarget
              if (element.scrollTop < 520) void loadOlder()
              if (element.scrollTop + element.clientHeight > element.scrollHeight - 520
                && streamMessages.hasNextPage && !streamMessages.isFetchingNextPage) {
                void streamMessages.fetchNextPage()
              }
            }}
          >
            {mode === 'stream' && streamMessages.hasPreviousPage && (
              <button type="button" className={styles.loadEdge} onClick={() => void loadOlder()}>
                <ArrowUp size={15} /> {streamMessages.isFetchingPreviousPage ? '加载中…' : '加载更早消息'}
              </button>
            )}
            <div className={styles.messageCanvas} style={{ height: messageVirtualizer.getTotalSize() }}>
              {messageVirtualizer.getVirtualItems().map((virtual) => {
                const message = messageItems[virtual.index]
                return (
                  <MessageRow
                    message={message}
                    key={message.rowid}
                    index={virtual.index}
                    start={virtual.start}
                    measure={messageVirtualizer.measureElement}
                  />
                )
              })}
            </div>
            {mode === 'stream' && streamMessages.hasNextPage && (
              <button type="button" className={styles.loadEdge} onClick={() => void streamMessages.fetchNextPage()}>
                <ArrowDown size={15} /> {streamMessages.isFetchingNextPage ? '加载中…' : '加载更新消息'}
              </button>
            )}
          </div>
        )}
      </section>
    </main>
  )
}

async function fetchMessages(table: string, page: PageParam): Promise<MessagesPage> {
  return api(
    `/api/messages${queryString({ table, offset: page.cursor, limit: PAGE_SIZE, before: page.before ? 1 : undefined })}`,
    messagesSchema,
  )
}

function MessageRow({
  message,
  index,
  start,
  measure,
}: {
  message: ChatMessage
  index: number
  start: number
  measure: (node: Element | null) => void
}) {
  const sender = message.display_sender_line || message.display_sender || message.sender_uin || '未知发送者'
  const styleId = Number(message.style_meta?.style_id ?? 0)
  const styled = Boolean(message.style_meta?.has_style)
  return (
    <article
      id={`row-${message.rowid}`}
      ref={measure}
      data-index={index}
      className={`${styles.message} ${message.is_self ? styles.self : ''}`}
      style={{ transform: `translateY(${start}px)` }}
    >
      <Avatar src={message.avatar_url} label={sender} className={styles.messageAvatar} />
      <div className={`${styles.bubble} ${styled ? styles.styled : ''}`} style={styled ? { '--style-hue': styleId % 360 } as React.CSSProperties : undefined}>
        <div className={styles.meta}>
          <strong>{sender}</strong>
          {message.sender_identity_note && <span>{message.sender_identity_note}</span>}
          <time>{message.datetime}</time>
          <a href={`/chat?table=${encodeURIComponent(message.table)}&rowid=${message.rowid}#row-${message.rowid}`}>row {message.rowid}</a>
        </div>
        <RichMessage
          nodes={message.rich_nodes as RichNode[] | undefined}
          assets={message.assets as MediaAsset[] | undefined}
          fallback={message.display_text}
          mediaKind={message.media_kind}
          mediaLabel={message.media_label}
          unmatchedReason={message.unmatched_reason}
        />
        <MessageDiagnostics table={message.table} rowid={message.rowid} />
      </div>
    </article>
  )
}

function MessageDiagnostics({ table, rowid }: { table: string; rowid: number }) {
  const [open, setOpen] = useState(false)
  const detail = useQuery({
    queryKey: ['message-detail', table, rowid],
    enabled: open,
    queryFn: () => api(`/api/message_detail${queryString({ table, rowid })}`, z.record(z.string(), z.unknown())),
    staleTime: Number.POSITIVE_INFINITY,
  })
  return (
    <details className={styles.diagnostics} onToggle={(event) => setOpen(event.currentTarget.open)}>
      <summary>诊断视图</summary>
      {detail.isPending && <span>加载中…</span>}
      {detail.isError && <span>{detail.error.message}</span>}
      {detail.data && <pre>{JSON.stringify(detail.data, null, 2)}</pre>}
    </details>
  )
}

function State({ icon, text, danger = false }: { icon?: React.ReactNode; text: string; danger?: boolean }) {
  return <div className={`${styles.state} ${danger ? styles.danger : ''}`}>{icon}{text}</div>
}
