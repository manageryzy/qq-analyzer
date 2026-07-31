import { z } from 'zod'

const errorSchema = z.object({
  error: z.union([
    z.string(),
    z.object({ code: z.string().optional(), message: z.string() }),
  ]),
})

export class ApiResponseError extends Error {
  constructor(message: string, readonly status: number, readonly code?: string) {
    super(message)
    this.name = 'ApiResponseError'
  }
}

export async function api<T>(path: string, schema: z.ZodType<T>, init?: RequestInit): Promise<T> {
  const response = await fetch(path, init)
  const payload: unknown = await response.json().catch(() => ({
    error: { message: `${response.status} ${response.statusText}` },
  }))
  if (!response.ok) {
    const parsed = errorSchema.safeParse(payload)
    const error = parsed.success ? parsed.data.error : `Request failed (${response.status})`
    throw new ApiResponseError(
      typeof error === 'string' ? error : error.message,
      response.status,
      typeof error === 'string' ? undefined : error.code,
    )
  }
  return schema.parse(payload)
}

export const distributionSchema = z.object({
  key: z.string(),
  count: z.number(),
  bytes: z.number(),
})

export const overviewSchema = z.object({
  health: z.object({ assets: z.number(), stale: z.number(), errors: z.number() }),
  coverage: z.object({
    embedding_rows: z.number(),
    clip_assets: z.number(),
    sscd_assets: z.number(),
    referenced_assets: z.number(),
    occurrences: z.number(),
  }),
  exact_copies: z.object({
    groups: z.number(),
    duplicate_groups: z.number(),
    duplicate_files: z.number(),
    potential_savings_bytes: z.number(),
  }),
  distributions: z.object({
    format: z.array(distributionSchema),
    source: z.array(distributionSchema),
    quality: z.array(distributionSchema),
    size: z.array(distributionSchema),
  }),
})

export const assetSchema = z.object({
  id: z.number(),
  file_size: z.number(),
  width: z.number().nullable(),
  height: z.number().nullable(),
  quality_flags: z.string(),
  source: z.string(),
  format: z.string(),
  copy_count: z.number(),
  variant_count: z.number().default(1),
  embedding_count: z.number(),
  reference_count: z.number(),
  content_url: z.string(),
  thumbnail_url: z.string(),
  error: z.string().nullable().optional(),
})

export const assetsPageSchema = z.object({
  items: z.array(assetSchema),
  next_cursor: z.string().nullable(),
  limit: z.number(),
})

export const searchResultSchema = z.object({
  id: z.number(),
  name: z.string().optional(),
  score: z.number(),
  distance: z.number().nullable().optional(),
  match_kind: z.string(),
  match_source: z.string().optional(),
  width: z.number().nullable().optional(),
  height: z.number().nullable().optional(),
  sha256: z.string().nullable().optional(),
  phash: z.string().nullable().optional(),
  quality_flags: z.string().optional(),
  source_class: z.string().optional(),
  exact_representative_id: z.number().optional(),
  copy_count: z.number().optional(),
  embedding_model: z.string().nullable().optional(),
  matched_tile_count: z.number().nullable().optional(),
  thumbnail_url: z.string(),
  content_url: z.string(),
}).passthrough()

export const searchSchema = z.object({
  results: z.array(searchResultSchema),
  unavailable: z.array(z.object({ signal: z.string(), reason: z.string() })).default([]),
}).passthrough()

export const embeddingSchema = z.object({
  kind: z.string(),
  model: z.string(),
  dimensions: z.number(),
  updated_at: z.string(),
})

export const assetDetailSchema = z.object({
  id: z.number(),
  width: z.number().nullable(),
  height: z.number().nullable(),
  file_size: z.number(),
  format: z.string(),
  source: z.string(),
  sha256: z.string(),
  phash: z.string().nullable(),
  phash_algo: z.string(),
  blur_score: z.number().nullable(),
  blur_algo: z.string(),
  quality_flags: z.string(),
  fingerprint_version: z.string(),
  indexed_at: z.string(),
  error: z.string().nullable(),
  content_url: z.string(),
  thumbnail_url: z.string(),
  diagnostics: z.object({ path: z.string(), source_root: z.string() }),
  exact_copy: z.object({
    representative_id: z.number().nullable(),
    count: z.number(),
    total_bytes: z.number(),
    duplicate_bytes: z.number(),
  }),
  embeddings: z.array(embeddingSchema),
  features: z.object({ tile_hashes: z.number(), local_features: z.number() }),
  copies: z.array(z.object({
    id: z.number(),
    file_size: z.number(),
    width: z.number().nullable(),
    height: z.number().nullable(),
    representative: z.boolean(),
    thumbnail_url: z.string(),
    diagnostics: z.object({ path: z.string() }),
  })),
})

export const occurrenceSchema = z.object({
  items: z.array(z.object({
    table: z.string(),
    rowid: z.number(),
    linked_at: z.string(),
    chat_url: z.string(),
  })),
  next_cursor: z.string().nullable(),
  limit: z.number().optional(),
  scan: z.object({
    complete: z.boolean(),
    tables_indexed: z.number(),
    tables_incomplete: z.number(),
    pending_rowid_span: z.number(),
    updated_at: z.string(),
  }).optional(),
})

const referenceMessageSchema = z.object({
  table: z.string(),
  rowid: z.number(),
  time: z.number(),
  datetime: z.string(),
  sender_uin: z.string(),
  display_sender: z.string(),
  display_sender_line: z.string(),
  avatar_url: z.string(),
  is_self: z.boolean(),
  display_text: z.string(),
  rich_nodes: z.array(z.record(z.string(), z.unknown())),
  assets: z.array(z.record(z.string(), z.unknown())),
  media_kind: z.string(),
  media_label: z.string(),
  unmatched_reason: z.string(),
  conversation: z.object({
    table: z.string(),
    type: z.string(),
    id: z.string(),
    label: z.string(),
    group_avatar_url: z.string(),
  }).passthrough(),
}).passthrough()

export const referenceAnalysisSchema = z.object({
  asset_id: z.number(),
  same_image_status: z.enum(['warming', 'complete']),
  summary: z.object({
    same_image_assets: z.number(),
    indexed_occurrences: z.number(),
    resolved_occurrences: z.number(),
    missing_messages: z.number(),
    unique_senders: z.number(),
    unique_conversations: z.number(),
    first_datetime: z.string(),
    last_datetime: z.string(),
  }),
  same_image_asset_ids: z.array(z.number()),
  coverage: z.object({
    complete: z.boolean(),
    tables_indexed: z.number(),
    tables_incomplete: z.number(),
    pending_rowid_span: z.number(),
    updated_at: z.string(),
    rows_scanned: z.number().optional(),
    estimated_rowid_span: z.number().optional(),
    occurrences_linked: z.number().optional(),
  }),
  top_senders: z.array(z.object({
    uin: z.string(),
    label: z.string(),
    count: z.number(),
    selected_count: z.number(),
    is_self: z.boolean(),
    chat_url: z.string(),
  })),
  top_conversations: z.array(z.object({
    table: z.string(),
    id: z.string(),
    type: z.string(),
    label: z.string(),
    avatar_url: z.string(),
    count: z.number(),
    selected_count: z.number(),
    chat_url: z.string(),
  })),
  timeline: z.array(z.object({ period: z.string(), count: z.number(), selected_count: z.number() })),
  daily_timeline: z.array(z.object({ date: z.string(), count: z.number(), selected_count: z.number() })),
  filters: z.object({
    sender: z.string().nullable(),
    table: z.string().nullable(),
    period: z.string().nullable(),
    date_from: z.string().nullable(),
    date_to: z.string().nullable(),
  }),
  offset: z.number(),
  limit: z.number(),
  total: z.number(),
  has_more: z.boolean(),
  items: z.array(z.object({
    table: z.string(),
    rowid: z.number(),
    linked_at: z.string(),
    chat_url: z.string(),
    message: referenceMessageSchema,
    context_before: z.array(referenceMessageSchema),
    context_after: z.array(referenceMessageSchema),
  })),
})

export const maintenanceControlSchema = z.object({
  supported: z.boolean(),
  running: z.boolean(),
  stopping: z.boolean(),
  started_at: z.string().nullable(),
  last_error: z.string().nullable(),
})

export const maintenanceActionSchema = z.object({
  task_id: z.string(),
  control: maintenanceControlSchema,
})

export const maintenanceSchema = z.object({
  account: z.string(),
  tasks: z.array(z.object({
    id: z.string(),
    title: z.string(),
    status: z.enum(['not_started', 'pending', 'running', 'complete']),
    current: z.number(),
    total: z.number(),
    unit: z.string(),
    updated_at: z.string(),
    control: maintenanceControlSchema.optional(),
    details: z.object({
      tables_indexed: z.number(),
      tables_incomplete: z.number(),
      rows_scanned: z.number(),
      occurrences_linked: z.number(),
      phase: z.enum(['legacy_migration', 'message_facts', 'aggregating', 'complete']).optional(),
      occurrences_inspected: z.number().optional(),
      occurrences_with_message_facts: z.number().optional(),
    }).optional(),
  })),
  search_queue: z.object({
    active: z.boolean(),
    queued: z.number(),
    capacity: z.number(),
    completed: z.number(),
    failed: z.number(),
    cancelled: z.number().default(0),
  }),
  same_image_cache: z.object({
    groups: z.number(),
    members: z.number(),
    aliases: z.number(),
    hits: z.number(),
    misses: z.number(),
    hit_rate: z.number(),
    evictions: z.number(),
    invalidations: z.number(),
    inflight: z.number(),
    max_groups: z.number(),
    max_members: z.number(),
    max_parallel: z.number(),
    vector_index: z.object({
      backend: z.string(),
      phase: z.string(),
      processed: z.number(),
      total: z.number(),
      percent: z.number(),
      elapsed_ms: z.number(),
      error: z.string().nullable(),
      generation: z.number(),
      points: z.number(),
      model: z.string().nullable(),
      dim: z.number().nullable(),
      hnsw_m: z.number(),
      ef_construct: z.number(),
      ef_search: z.number(),
    }).optional(),
  }).optional(),
  ranking_tasks: z.object({
    active: z.number(),
    queued: z.number(),
    cached: z.number(),
    capacity: z.number(),
  }).optional(),
})

export const insightsOverviewSchema = z.object({
  account: z.string(),
  coverage: z.object({
    ready: z.boolean(),
    occurrences: z.number(),
    facts: z.number(),
    cube_rows: z.number(),
    fact_coverage: z.number(),
    updated_at: z.string(),
  }),
  range: z.object({ from: z.string(), to: z.string() }),
  summary: z.object({
    references: z.number(),
    images: z.number(),
    senders: z.number(),
    conversations: z.number(),
  }),
  timeline: z.array(z.object({
    date: z.string(),
    count: z.number(),
    selected_count: z.number(),
  })),
  senders: z.array(z.object({
    uin: z.string(),
    label: z.string(),
    count: z.number(),
    selected: z.boolean(),
  })),
  conversations: z.array(z.object({
    table: z.string(),
    id: z.string(),
    type: z.string(),
    label: z.string(),
    count: z.number(),
    selected: z.boolean(),
  })),
})

export const rankedAssetSchema = z.object({
  id: z.number(),
  exact_representative_ids: z.array(z.number()),
  reference_count: z.number(),
  sender_count: z.number(),
  conversation_count: z.number(),
  first_date: z.string(),
  peak_date: z.string(),
  last_date: z.string(),
  trend: z.array(z.object({ date: z.string(), count: z.number() })),
  same_image_members: z.number(),
  growth: z.number(),
  growth_rate: z.number().nullable(),
  thumbnail_url: z.string(),
  content_url: z.string(),
  detail_url: z.string(),
  references_url: z.string(),
})

export const insightsAssetsSchema = z.object({
  status: z.enum(['warming', 'complete', 'failed', 'cancelled']),
  ranking_scope: z.literal('candidate_pool'),
  candidate_limit: z.number(),
  exhaustive: z.boolean(),
  progress: z.object({
    processed: z.number(),
    total: z.number(),
    percent: z.number(),
  }),
  items: z.array(rankedAssetSchema),
  next_cursor: z.string().nullable(),
  total: z.number().optional(),
  error: z.string().nullable().optional(),
})

export const insightsCompareSchema = z.object({
  account: z.string(),
  by: z.enum(['sender', 'conversation']),
  items: z.array(z.object({
    key: z.string(),
    label: z.string(),
    type: z.string().nullable(),
    id: z.string().nullable(),
    references: z.number(),
    top_images: z.array(z.object({
      id: z.number(),
      references: z.number(),
      thumbnail_url: z.string(),
      detail_url: z.string(),
    })),
  })),
})

export type Asset = z.infer<typeof assetSchema>
export type AssetDetail = z.infer<typeof assetDetailSchema>
export type ImageSearchReport = z.infer<typeof searchSchema>
export type ReferenceAnalysis = z.infer<typeof referenceAnalysisSchema>
export type MaintenanceReport = z.infer<typeof maintenanceSchema>
export type InsightsOverview = z.infer<typeof insightsOverviewSchema>
export type InsightsAssets = z.infer<typeof insightsAssetsSchema>
export type RankedAsset = z.infer<typeof rankedAssetSchema>

export function queryString(values: Record<string, string | number | undefined>): string {
  const params = new URLSearchParams()
  for (const [key, value] of Object.entries(values)) {
    if (value !== undefined && value !== '') params.set(key, String(value))
  }
  const query = params.toString()
  return query ? `?${query}` : ''
}
