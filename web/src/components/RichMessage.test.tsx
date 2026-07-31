import { fireEvent, render, screen } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider } from '@tanstack/react-router'
import { RichMessage } from './RichMessage'

test('renders nested records and indexed-image action', async () => {
  const root = createRootRoute()
  const index = createRoute({
    getParentRoute: () => root,
    path: '/',
    component: () => <RichMessage fallback="" nodes={[{ type: 'record', label: 'Forwarded', children: [{ type: 'image', assets: [{ kind: 'image', href: '/asset/x', image_index_id: 9 }] }] }]} />,
  })
  const router = createRouter({ routeTree: root.addChildren([index]), history: createMemoryHistory({ initialEntries: ['/'] }) })
  render(<QueryClientProvider client={new QueryClient()}><RouterProvider router={router} /></QueryClientProvider>)
  expect(await screen.findByText('Forwarded')).toBeInTheDocument()
  expect(screen.getAllByRole('link', { name: /相似/ })).toHaveLength(2)
})

test('preserves forwarded preview strings and expanded message metadata', async () => {
  const root = createRootRoute()
  const index = createRoute({
    getParentRoute: () => root,
    path: '/',
    component: () => <RichMessage fallback="" nodes={[{
      type: 'multi_msg',
      text: '[聊天记录] 群聊的聊天记录',
      item_count: 2,
      items: ['预览字符串', { type: 'multi_item', text: 'Alice: 第二条' }],
      items_expanded: [
        { type: 'mmp_item', display_sender_line: 'Alice(10001)', datetime: '2024-10-04 14:47:43', msg_seq: 40552, rich_nodes: [{ type: 'text', text: '完整正文' }] },
        { type: 'mmp_item', display_sender: 'Bob', datetime: '2024-10-04 14:48:00', msg_seq: 40553, text: '无子节点正文' },
      ],
    }]} />,
  })
  const router = createRouter({ routeTree: root.addChildren([index]), history: createMemoryHistory({ initialEntries: ['/'] }) })
  render(<QueryClientProvider client={new QueryClient()}><RouterProvider router={router} /></QueryClientProvider>)

  expect(await screen.findByText(/预览字符串/)).toBeInTheDocument()
  fireEvent.click(screen.getByText('[聊天记录] 群聊的聊天记录'))
  expect(screen.getByText('Alice(10001)')).toBeInTheDocument()
  expect(screen.getByText('seq 40552')).toBeInTheDocument()
  expect(screen.getByText('完整正文')).toBeInTheDocument()
  expect(screen.getByText('无子节点正文')).toBeInTheDocument()
})

test('recursively renders a forwarded record inside an embedded message', async () => {
  const root = createRootRoute()
  const index = createRoute({
    getParentRoute: () => root,
    path: '/',
    component: () => <RichMessage fallback="" nodes={[{
      type: 'multi_msg',
      text: '第一层聊天记录',
      item_count: 1,
      items: [{ type: 'multi_item', text: 'Alice: 第二层聊天记录' }],
      items_expanded: [{
        type: 'mmp_item',
        display_sender_line: 'Alice(10001)',
        rich_nodes: [{
          type: 'multi_msg',
          text: '第二层聊天记录',
          item_count: 1,
          items: [{ type: 'multi_item', text: 'Bob: 内层图片' }],
          items_expanded: [{
            type: 'mmp_item',
            display_sender_line: 'Bob(10002)',
            rich_nodes: [{
              type: 'image',
              text: '[图片]',
              asset: { kind: 'image', href: '/asset/nested', image_index_id: 77 },
            }],
          }],
        }],
      }],
    }]} />,
  })
  const router = createRouter({ routeTree: root.addChildren([index]), history: createMemoryHistory({ initialEntries: ['/'] }) })
  render(<QueryClientProvider client={new QueryClient()}><RouterProvider router={router} /></QueryClientProvider>)

  fireEvent.click(await screen.findByText('第一层聊天记录'))
  expect(screen.getByText('Alice(10001)')).toBeInTheDocument()
  fireEvent.click(screen.getByText('第二层聊天记录'))
  expect(screen.getByText('Bob(10002)')).toBeInTheDocument()
  expect(screen.getAllByRole('link', { name: /相似/ })).toHaveLength(2)
})
