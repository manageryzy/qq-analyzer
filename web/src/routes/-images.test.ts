import { imageSearchSchema } from './images.index'
import { referenceSearchSchema } from './images.$assetId_.references'
import { trendsSearchSchema } from './images.trends'
import { applySearchDuplicateMode } from '../lib/image-results'

test('image URL search state is validated and coerced', () => {
  expect(imageSearchSchema.parse({ similar: '42', duplicates: 'variants', sort: 'oldest', signal: 'semantic', strategy: 'fast' })).toEqual({ similar: 42, duplicates: 'variants', sort: 'oldest', signal: 'semantic', strategy: 'fast' })
  expect(imageSearchSchema.parse({ similar: '-1', embeddings: 'invalid', sort: 'largest', signal: 'invalid', strategy: 'slow' })).toMatchObject({ similar: undefined, embeddings: undefined, sort: undefined, signal: undefined, strategy: undefined })
})

test('reference analysis URL preserves composable sender, conversation, and month filters', () => {
  expect(referenceSearchSchema.parse({
    offset: '24',
    sender: '123456',
    table: 'group_987654',
    period: '2025-06',
  })).toEqual({ offset: 24, sender: '123456', table: 'group_987654', period: '2025-06' })
  expect(referenceSearchSchema.parse({ offset: '-1', period: 'June 2025' })).toMatchObject({
    offset: undefined,
    period: undefined,
  })
  expect(referenceSearchSchema.parse({ from: '2025-06-01', to: '2025-06-30' })).toMatchObject({
    from: '2025-06-01',
    to: '2025-06-30',
  })
})

test('search results obey exact-copy and thumbnail variant modes', () => {
  const results = [
    { id: 2913005, exact_representative_id: 430388, copy_count: 6, sha256: 'ABC', phash: 'same', quality_flags: '' },
    { id: 3690072, exact_representative_id: 999999, copy_count: 6, sha256: 'abc', phash: 'same', quality_flags: '' },
    { id: 80, exact_representative_id: 80, copy_count: 1, phash: 'other', quality_flags: '' },
    { id: 81, exact_representative_id: 81, copy_count: 1, phash: 'other', quality_flags: 'thumbnail' },
    { id: 82, exact_representative_id: 82, copy_count: 1, sha256: 'different', phash: 'other', quality_flags: '' },
  ]

  expect(applySearchDuplicateMode(results, 'all').map((item) => item.id)).toEqual([2913005, 3690072, 80, 81, 82])
  expect(applySearchDuplicateMode(results, 'collapsed').map((item) => item.id)).toEqual([2913005, 80, 81, 82])
  expect(applySearchDuplicateMode(results, 'variants').map((item) => item.id)).toEqual([2913005, 80])
  expect(applySearchDuplicateMode(results, 'duplicates').map((item) => item.id)).toEqual([2913005])
  expect(applySearchDuplicateMode(results, 'unique').map((item) => item.id)).toEqual([80, 81, 82])
})

test('SSCD scores at or above 98 percent collapse as one image variant', () => {
  const results = [
    { id: 1, match_kind: 'same_sscd', score: 1, phash: 'a' },
    { id: 2, match_kind: 'copy_sscd', score: 0.98, phash: 'b' },
    { id: 3, match_kind: 'copy_sscd', score: 0.979, phash: 'c' },
  ]
  expect(applySearchDuplicateMode(results, 'variants').map((item) => item.id)).toEqual([1, 3])
  expect(applySearchDuplicateMode(results, 'all').map((item) => item.id)).toEqual([1, 2, 3])
})

test('trends URL preserves multi-person, multi-conversation, date, and rank filters', () => {
  expect(trendsSearchSchema.parse({
    from: '2026-01-01',
    to: '2026-03-31',
    senders: '100,200',
    tables: 'group_1,group_2',
    conversation_type: 'group',
    rank: 'revival',
  })).toEqual({
    from: '2026-01-01',
    to: '2026-03-31',
    senders: '100,200',
    tables: 'group_1,group_2',
    conversation_type: 'group',
    rank: 'revival',
  })
  expect(trendsSearchSchema.parse({ rank: 'random' })).toMatchObject({ rank: undefined })
  expect(trendsSearchSchema.parse({ senders: '"1234567890"' })).toMatchObject({
    senders: '1234567890',
  })
})
