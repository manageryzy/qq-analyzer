import { chatSearchSchema } from './chat'

test('chat URL state preserves row navigation, mode, and filters', () => {
  expect(chatSearchSchema.parse({
    table: 'group_20001',
    rowid: '42',
    mode: 'stream',
    q: '测试',
    type: 'group',
  })).toEqual({
    table: 'group_20001',
    rowid: 42,
    mode: 'stream',
    q: '测试',
    type: 'group',
  })
})

test('chat URL state rejects unsafe enum values without breaking the route', () => {
  const parsed = chatSearchSchema.parse({ rowid: '-1', mode: 'all-at-once', type: 'unknown' })
  expect(parsed.rowid).toBeUndefined()
  expect(parsed.mode).toBeUndefined()
  expect(parsed.type).toBeUndefined()
})
