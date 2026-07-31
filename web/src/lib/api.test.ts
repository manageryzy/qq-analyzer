import { z } from 'zod'
import { api, insightsAssetsSchema, queryString } from './api'

test('API responses are contract validated', async () => {
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ value: 7 }) }))
  await expect(api('/fixture', z.object({ value: z.number() }))).resolves.toEqual({ value: 7 })
})

test('queryString omits empty values', () => {
  expect(queryString({ q: 'cat', cursor: undefined, source: '' })).toBe('?q=cat')
})

test('candidate-pool ranking contract keeps warming progress monotonic fields explicit', () => {
  expect(insightsAssetsSchema.parse({
    status: 'warming',
    ranking_scope: 'candidate_pool',
    candidate_limit: 2000,
    exhaustive: false,
    progress: { processed: 40, total: 2000, percent: 0.02 },
    items: [],
    next_cursor: null,
  })).toMatchObject({
    status: 'warming',
    candidate_limit: 2000,
    exhaustive: false,
    progress: { processed: 40, total: 2000 },
  })
})
