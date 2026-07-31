import { expect, test } from '@playwright/test'

test('gallery, detail, chat, and SPA refresh remain usable', async ({ page }) => {
  await page.goto('/images')
  await expect(page.getByRole('heading', { name: '图像图库' })).toBeVisible()
  await expect(page.getByLabel('可搜索图像图库')).toBeVisible()

  const nextPage = page.waitForResponse((response) => {
    const url = new URL(response.url())
    return url.pathname === '/api/image-index/assets' && url.searchParams.has('cursor')
  })
  const imageList = page.getByLabel('图像列表')
  await imageList.evaluate((element) => { element.scrollTop = element.scrollHeight })
  await nextPage
  await expect(page.getByText(/125 已载入/)).toBeVisible()
  await imageList.evaluate((element) => { element.scrollTop = 0 })

  const preview = page.getByRole('button', { name: '查看图片 125 的相似图片与聊天位置' })
  await preview.click()
  const inspector = page.getByLabel('图片详情与聊天位置')
  await expect(inspector).toBeVisible()
  await expect(inspector.getByText('像素与结构')).toBeVisible()
  await expect(inspector.getByText('语义 CLIP')).toBeVisible()
  const occurrence = inspector.getByRole('link', { name: /群聊 20001.*row 42/ })
  await expect(occurrence).toBeVisible()
  await occurrence.click()
  await expect(page.getByRole('link', { name: 'row 42', exact: true })).toBeVisible()

  await page.goto('/images/125')
  await expect(page.getByText('图片 #125')).toBeVisible()
  await page.reload()
  await expect(page.getByRole('heading', { name: /PNG/ })).toBeVisible()

  await page.getByRole('link', { name: 'Chat' }).click()
  await expect(page.getByLabel('Chat messages')).toBeVisible()
  await page.reload()
  await expect(page.getByText('会话', { exact: true })).toBeVisible()
})

test('chat restores avatars, rowid paging, continuous loading, and direct-row links', async ({ page, isMobile }) => {
  await page.goto('/chat')
  await page.getByRole('button', { name: /20001/ }).click()
  await expect(page.getByRole('link', { name: 'row 1', exact: true })).toBeVisible()
  await expect(page.getByTestId('avatar').nth(1)).toBeVisible()

  const messageList = page.getByLabel('消息列表')
  await messageList.hover()
  await page.mouse.wheel(0, 500)
  await expect.poll(() => messageList.evaluate((element) => element.scrollTop)).toBeGreaterThan(0)

  if (!isMobile) {
    const progress = page.getByLabel('消息进度')
    await page.route('**/api/messages?**', async (route) => {
      const url = new URL(route.request().url())
      if (url.searchParams.get('offset') === '31') {
        await new Promise((resolve) => setTimeout(resolve, 300))
      }
      await route.continue()
    })
    await progress.fill('500')
    await progress.dispatchEvent('pointerup')
    await page.waitForTimeout(100)
    await expect(progress).toHaveValue('500')
    await expect(page.getByRole('link', { name: 'row 31', exact: true })).toBeVisible()
    await expect(progress).toHaveValue('500')
    await page.getByTitle('首页').click()
    await expect(page.getByRole('link', { name: 'row 1', exact: true })).toBeVisible()
  }

  await page.getByTitle('下一页').click()
  await expect(page.getByRole('link', { name: 'row 21', exact: true })).toBeVisible()

  await page.getByRole('button', { name: '连续' }).click()
  const heightBeforeLoad = await messageList.evaluate((element) => element.scrollHeight)
  const nextPage = page.waitForResponse((response) => {
    const url = new URL(response.url())
    return url.pathname === '/api/messages' && url.searchParams.get('offset') === '41'
  })
  const loadNewer = page.getByRole('button', { name: /加载更新消息/ })
  await loadNewer.click()
  await nextPage
  await expect(loadNewer).toHaveText(/加载更新消息/)
  await expect.poll(() => messageList.evaluate((element) => element.scrollHeight)).toBeGreaterThan(heightBeforeLoad)

  await page.goto('/chat?table=group_20001&rowid=42&mode=stream#row-42')
  await expect(page.getByRole('link', { name: 'row 42', exact: true })).toBeVisible()
})

test('primary controls are keyboard reachable without overlap', async ({ page }) => {
  await page.goto('/images')
  const brand = page.getByRole('link', { name: 'QQ Archive' })
  for (let step = 0; step < 4 && !(await brand.evaluate((element) => element === document.activeElement)); step += 1) {
    await page.keyboard.press('Tab')
  }
  await expect(brand).toBeFocused()
  const viewport = page.viewportSize()
  const toolbar = await page.getByLabel('图像搜索与筛选').boundingBox()
  expect(toolbar?.width).toBeLessThanOrEqual(viewport?.width ?? Number.MAX_SAFE_INTEGER)
})

test('reference analysis filters rankings and timeline while keeping context compact', async ({ page }) => {
  await page.goto('/images/125/references')
  await expect(page.getByRole('heading', { name: '引用人与聊天上下文' })).toBeVisible()

  const reference = page.getByLabel(/引用消息 row 42/)
  await expect(reference).toBeVisible()
  await expect(page.getByLabel(/上下文消息 row/)).toHaveCount(0)
  await expect(page.locator('a[href*="table=group_20001"][href*="rowid=42"]')).toBeVisible()
  await page.getByRole('button', { name: /展开前后消息/ }).first().click()
  await expect(page.getByLabel(/上下文消息 row/)).toHaveCount(10)
  await expect(page.getByRole('button', { name: '收起上下文' })).toBeVisible()

  const chart = page.getByRole('img', { name: /每日引用直方图/ })
  await expect(chart).toBeVisible()
  await expect(chart.locator('canvas')).toBeVisible()
  await chart.hover()
  const box = await chart.boundingBox()
  expect(box).not.toBeNull()
  await page.mouse.wheel(0, -500)
  const filtered = page.waitForResponse((response) => response.url().includes('reference-analysis') && response.url().includes('date_from='))
  await page.mouse.move((box?.x ?? 0) + (box?.width ?? 0) * .35, (box?.y ?? 0) + 100)
  await page.mouse.down()
  await page.mouse.move((box?.x ?? 0) + (box?.width ?? 0) * .65, (box?.y ?? 0) + 100, { steps: 8 })
  await page.mouse.up()
  await filtered
  await expect(page).toHaveURL(/from=\d{4}-\d{2}-\d{2}/)
  await expect(page.getByRole('button', { name: /日期：/ })).toBeVisible()
})

test('popular trends warms candidate ranking, links filters, and fits desktop and mobile', async ({ page }) => {
  const start = await page.request.post('/api/maintenance/tasks/image-popularity-analysis/start')
  expect(start.ok()).toBeTruthy()
  await expect.poll(async () => {
    const response = await page.request.get('/api/image-index/insights/overview')
    const payload = await response.json()
    return payload.coverage?.ready
  }, { timeout: 20_000 }).toBe(true)

  await page.goto('/images/trends')
  await expect(page.getByRole('heading', { name: '热门图片与传播趋势' })).toBeVisible()
  const chart = page.getByRole('img', { name: '图片引用传播时间线' })
  await expect(chart.locator('canvas')).toBeVisible()
  await expect(page.getByRole('article').getByText('3 次引用')).toBeVisible({ timeout: 20_000 })
  await expect(page.getByText('候选池', { exact: false })).toBeVisible()

  await page.getByLabel('开始日期').fill('2023-12-01')
  await expect(page.getByRole('article').getByText('3 次引用')).toBeVisible({ timeout: 20_000 })
  const context = page.getByRole('link', { name: /引用上下文/ }).first()
  await expect(context).toHaveAttribute('href', /from=2023-12-01/)
  await context.click()
  await expect(page.getByRole('heading', { name: '引用人与聊天上下文' })).toBeVisible()
  await expect(page).toHaveURL(/from=2023-12-01/)

  await page.goto('/images/trends')
  const viewport = page.viewportSize()
  const hero = await page.getByRole('heading', { name: '热门图片与传播趋势' }).boundingBox()
  expect((hero?.x ?? 0) + (hero?.width ?? 0)).toBeLessThanOrEqual(viewport?.width ?? Number.MAX_SAFE_INTEGER)
})
