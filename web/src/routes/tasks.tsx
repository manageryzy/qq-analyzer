import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { createFileRoute } from '@tanstack/react-router'
import { Activity, AlertCircle, CheckCircle2, Clock3, Database, LoaderCircle, Pause, Play, RefreshCw, Search } from 'lucide-react'
import { api, maintenanceActionSchema, maintenanceSchema, type MaintenanceReport } from '../lib/api'
import styles from './tasks.module.css'

export const Route = createFileRoute('/tasks')({ component: TasksPage })

function TasksPage() {
  const queryClient = useQueryClient()
  const status = useQuery({
    queryKey: ['maintenance-tasks'],
    queryFn: () => api('/api/maintenance/tasks', maintenanceSchema),
    refetchInterval: 2_000,
  })
  const control = useMutation({
    mutationFn: ({ taskId, action }: { taskId: string; action: 'start' | 'pause' }) => api(
      `/api/maintenance/tasks/${encodeURIComponent(taskId)}/${action}`,
      maintenanceActionSchema,
      { method: 'POST' },
    ),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['maintenance-tasks'] }),
  })
  return <main className={styles.page}>
    <header className={styles.title}>
      <div><Activity /><span><h1>后台维护</h1><p>持久化索引任务与实时搜索队列</p></span></div>
      <button type="button" onClick={() => void status.refetch()} disabled={status.isFetching}><RefreshCw className={status.isFetching ? styles.spin : ''} /> 刷新</button>
    </header>

    {status.isPending && <div className={styles.state}><LoaderCircle className={styles.spin} /> 正在读取任务状态…</div>}
    {status.isError && <div className={styles.error}><AlertCircle /> {status.error.message}</div>}
    {status.data && <>
      <section className={styles.queue}>
        <div><Search /><span><strong>图像检索队列</strong><small>{status.data.search_queue.active ? '模型正在推理' : '空闲'}</small></span></div>
        <Metric value={status.data.search_queue.queued} label={`排队 / ${status.data.search_queue.capacity}`} />
        <Metric value={status.data.search_queue.completed} label="已完成" />
        <Metric value={status.data.search_queue.failed} label="失败" danger={status.data.search_queue.failed > 0} />
      </section>
      {(status.data.same_image_cache || status.data.ranking_tasks) && <section className={styles.runtimeGrid}>
        {status.data.same_image_cache && <div className={styles.runtimeCard}>
          <strong>同图 LRU</strong>
          <Metric value={status.data.same_image_cache.groups} label={`组 / ${formatNumber(status.data.same_image_cache.max_groups)}`} />
          <Metric value={status.data.same_image_cache.members} label={`成员 / ${formatNumber(status.data.same_image_cache.max_members)}`} />
          <Metric value={Math.round(status.data.same_image_cache.hit_rate * 100)} label="命中率 %" />
          <Metric value={status.data.same_image_cache.evictions} label="淘汰" />
        </div>}
        {status.data.same_image_cache?.vector_index?.backend === 'qdrant-segment-hnsw' && <div className={styles.runtimeCard}>
          <strong>SSCD Qdrant HNSW</strong>
          <Metric value={status.data.same_image_cache.vector_index.processed} label={`向量 / ${formatNumber(status.data.same_image_cache.vector_index.total)}`} />
          <Metric value={Math.round(status.data.same_image_cache.vector_index.percent * 100)} label={`${status.data.same_image_cache.vector_index.phase} / %`} />
          <Metric value={status.data.same_image_cache.vector_index.points} label="已就绪点" />
          <Metric value={Math.round(status.data.same_image_cache.vector_index.elapsed_ms / 1000)} label="耗时秒" danger={Boolean(status.data.same_image_cache.vector_index.error)} />
        </div>}
        {status.data.ranking_tasks && <div className={styles.runtimeCard}>
          <strong>候选榜任务</strong>
          <Metric value={status.data.ranking_tasks.active} label="活跃" />
          <Metric value={status.data.ranking_tasks.queued} label="排队" />
          <Metric value={status.data.ranking_tasks.cached} label={`已缓存 / ${status.data.ranking_tasks.capacity}`} />
        </div>}
      </section>}

      <section className={styles.tasks}>
        {status.data.tasks.map((task) => <TaskCard
          key={task.id}
          task={task}
          busy={control.isPending && control.variables?.taskId === task.id}
          onAction={(action) => control.mutate({ taskId: task.id, action })}
        />)}
      </section>
      {control.isError && <div className={styles.actionError}><AlertCircle /> {control.error.message}</div>}
      <p className={styles.note}>页面每 2 秒读取一次 SQLite 中已经提交的检查点；关闭服务或扫描程序不会丢失进度，下一次会从断点继续。</p>
    </>}
  </main>
}

function TaskCard({ task, busy, onAction }: { task: MaintenanceReport['tasks'][number]; busy: boolean; onAction: (action: 'start' | 'pause') => void }) {
  const fresh = isFresh(task.updated_at)
  const state = task.control?.stopping ? 'stopping' : task.control?.running ? 'running' : task.status === 'running' && !fresh ? 'paused' : task.status
  const rawPercent = task.total > 0 ? Math.min(100, Math.max(0, task.current / task.total * 100)) : 0
  const percent = task.control?.running && rawPercent >= 100 ? 99.9 : rawPercent
  const popularity = task.id === 'image-popularity-analysis'
  return <article className={styles.task}>
    <header><div><Database /><span><strong>{task.title}</strong><small>{task.id}</small></span></div><Status state={state} /></header>
    <div className={styles.progress} aria-label={`${task.title} ${percent.toFixed(1)}%`}><span style={{ width: `${percent}%` }} /></div>
    <div className={styles.counts}><strong>{formatNumber(task.current)}</strong><span>/ {formatNumber(task.total)} {task.unit}</span><em>{percent.toFixed(1)}%</em></div>
    {task.details && <dl>
      <div><dt>已记录会话表</dt><dd>{formatNumber(task.details.tables_indexed)}</dd></div>
      <div><dt>未完成会话表</dt><dd>{formatNumber(task.details.tables_incomplete)}</dd></div>
      <div><dt>{popularity ? '已检查引用' : '实际扫描消息'}</dt><dd>{formatNumber(task.details.occurrences_inspected ?? task.details.rows_scanned)}</dd></div>
      <div><dt>{popularity ? '已有消息事实' : '已链接引用'}</dt><dd>{formatNumber(task.details.occurrences_with_message_facts ?? task.details.occurrences_linked)}</dd></div>
      {popularity && task.details.phase === 'legacy_migration' && <div><dt>当前阶段</dt><dd>正在迁移旧消息事实</dd></div>}
      {popularity && task.details.phase === 'aggregating' && <div><dt>当前阶段</dt><dd>正在重建热门聚合</dd></div>}
    </dl>}
    {task.control?.last_error && <div className={styles.taskError}><AlertCircle /> {task.control.last_error}</div>}
    <footer><span><Clock3 /> {task.updated_at ? `最近提交 ${formatTime(task.updated_at)}` : '尚无检查点'}</span>{task.control?.supported && <button type="button" disabled={busy || task.control.stopping} onClick={() => onAction(task.control?.running ? 'pause' : 'start')}>{busy || task.control.stopping ? <LoaderCircle className={styles.spin} /> : task.control.running ? <Pause /> : <Play />}{task.control.stopping ? '正在暂停' : task.control.running ? '暂停' : '开始 / 继续'}</button>}</footer>
  </article>
}

function Status({ state }: { state: 'not_started' | 'pending' | 'running' | 'complete' | 'paused' | 'stopping' }) {
  if (state === 'complete') return <span className={styles.complete}><CheckCircle2 /> 已完成</span>
  if (state === 'running') return <span className={styles.running}><LoaderCircle className={styles.spin} /> 运行中</span>
  if (state === 'stopping') return <span className={styles.paused}><LoaderCircle className={styles.spin} /> 正在暂停</span>
  if (state === 'paused') return <span className={styles.paused}><Clock3 /> 已暂停 / 可续扫</span>
  return <span className={styles.pending}><Clock3 /> {state === 'not_started' ? '未开始' : '待处理'}</span>
}

function Metric({ value, label, danger = false }: { value: number; label: string; danger?: boolean }) { return <div className={danger ? styles.metricDanger : ''}><strong>{formatNumber(value)}</strong><span>{label}</span></div> }
function formatNumber(value: number) { return new Intl.NumberFormat('zh-CN').format(value) }
function parseSqliteTime(value: string) { return Date.parse(value.includes('T') ? value : `${value.replace(' ', 'T')}Z`) }
function isFresh(value: string) { const timestamp = parseSqliteTime(value); return Number.isFinite(timestamp) && Date.now() - timestamp < 15_000 }
function formatTime(value: string) { const timestamp = parseSqliteTime(value); return Number.isFinite(timestamp) ? new Date(timestamp).toLocaleString('zh-CN') : value }
