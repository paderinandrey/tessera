import { expect, test, type Page } from '@playwright/test'

const inspectedText = '🙂 Martina in Bern • 4111'
const browserErrors = new WeakMap<Page, string[]>()

test.beforeEach(async ({ page }) => {
  const errors: string[] = []
  browserErrors.set(page, errors)
  page.on('pageerror', (error) => errors.push(`pageerror: ${error.message}`))
  page.on('console', (message) => {
    if (message.type() === 'error') {
      errors.push(`console: ${message.text()}`)
    }
  })
})

test.afterEach(async ({ page }) => {
  expect(browserErrors.get(page) ?? []).toEqual([])
})

async function mockDetector(page: Page) {
  await page.route('**/api/health', (route) => route.fulfill({
    json: { status: 'ok', ner: true, ner_off_reason: null },
  }))
  await page.route('**/api/detect', async (route) => {
    expect(route.request().postDataJSON()).toEqual({ text: inspectedText })
    await route.fulfill({
      json: {
        spans: [
          {
            entity_type: 'PERSON', start: 2, end: 9, confidence: 0.91,
            recognizer: 'ner:gliner', tier: 2, boosted: false,
          },
          {
            entity_type: 'LOCATION', start: 13, end: 17, confidence: 0.88,
            recognizer: 'ner:gliner', tier: 2, boosted: false,
          },
          {
            entity_type: 'CREDIT_CARD', start: 20, end: 24, confidence: 1,
            recognizer: 'catalog:credit_card', tier: 1, boosted: false,
          },
        ],
        layers_run: ['deterministic', 'ner'],
        version: 'test-version',
      },
    })
  })
}

test('analyzes and clears text without persisting it', async ({ page }) => {
  await mockDetector(page)
  await page.goto('/')
  await page.getByLabel('Text to inspect').fill(inspectedText)
  await page.getByRole('button', { name: 'Analyze text' }).click()
  await expect(page.getByText('Model detected').first()).toBeVisible()
  expect(page.url()).not.toContain('Martina')
  expect(await page.evaluate(() => ({ ...localStorage }))).toEqual({})
  expect(await page.evaluate(() => ({ ...sessionStorage }))).toEqual({})
  await page.getByRole('button', { name: 'Clear' }).click()
  await expect(page.getByText('Martina', { exact: true })).toHaveCount(0)
})

test('uses the approved layer presentation and preserves visible mobile detail labels', async ({ page }) => {
  await mockDetector(page)
  await page.goto('/')
  await page.getByLabel('Text to inspect').fill(inspectedText)
  await page.getByRole('button', { name: 'Analyze text' }).click()

  const martina = page.locator('mark[aria-label="Martina — Model detected"]')
  await expect(martina).toBeVisible()
  await expect(martina).toHaveCSS('background-color', 'rgb(219, 234, 254)')

  const card = page.locator('mark[aria-label="4111 — Checksum verified"]')
  await expect(card).toBeVisible()
  await expect(card).toHaveCSS('background-color', 'rgb(220, 252, 231)')
  await expect(page.getByText('Checksum verified').first()).toBeVisible()

  if (page.viewportSize()?.width === 320) {
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
    for (const label of ['Fragment', 'Entity type', 'Layer', 'Confidence', 'Tier', 'Recognizer', 'Context']) {
      const labelPresentation = await page.locator(`td[data-label="${label}"]`).first().evaluate((cell) => {
        const labelStyle = getComputedStyle(cell, '::before')
        const cellBox = cell.getBoundingClientRect()
        return {
          content: labelStyle.content,
          display: labelStyle.display,
          fontSize: Number.parseFloat(labelStyle.fontSize),
          opacity: Number.parseFloat(labelStyle.opacity),
          position: labelStyle.position,
          visibility: labelStyle.visibility,
          width: cellBox.width,
          height: cellBox.height,
        }
      })
      expect(labelPresentation.content).toContain(label)
      expect(labelPresentation.display).not.toBe('none')
      expect(labelPresentation.visibility).toBe('visible')
      expect(labelPresentation.opacity).toBeGreaterThan(0)
      expect(labelPresentation.fontSize).toBeGreaterThan(0)
      expect(labelPresentation.position).toBe('static')
      expect(labelPresentation.width).toBeGreaterThan(0)
      expect(labelPresentation.height).toBeGreaterThan(0)
    }
  }
})

test('uses approved accessible evidence foreground and background pairs in dark mode', async ({ page }) => {
  await page.emulateMedia({ colorScheme: 'dark' })
  await mockDetector(page)
  await page.goto('/')
  await page.getByLabel('Text to inspect').fill(inspectedText)
  await page.getByRole('button', { name: 'Analyze text' }).click()

  const martina = page.locator('mark[aria-label="Martina — Model detected"]')
  await expect(martina).toHaveCSS('background-color', 'rgb(30, 58, 138)')
  await expect(martina).toHaveCSS('color', 'rgb(219, 234, 254)')

  const card = page.locator('mark[aria-label="4111 — Checksum verified"]')
  await expect(card).toHaveCSS('background-color', 'rgb(20, 83, 45)')
  await expect(card).toHaveCSS('color', 'rgb(220, 252, 231)')
})

test('preserves submitted whitespace and wraps an unbroken value at a narrow width', async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 720 })
  await page.route('**/api/health', (route) => route.fulfill({
    json: { status: 'ok', ner: true, ner_off_reason: null },
  }))
  await page.route('**/api/detect', (route) => route.fulfill({
    json: {
      spans: [{
        entity_type: 'TEXT', start: 0, end: 8, confidence: 0.91,
        recognizer: 'ner:gliner', tier: 2, boosted: false,
      }],
      layers_run: ['deterministic', 'ner'],
      version: 'test-version',
    },
  }))
  await page.goto('/')
  const text = `Line one\nLine two  ${'x'.repeat(400)}`
  await page.getByLabel('Text to inspect').fill(text)
  await page.getByRole('button', { name: 'Analyze text' }).click()

  const result = page.locator('.highlighted-result')
  await expect(result).toContainText(text)
  await expect(result).toHaveCSS('white-space', 'pre-wrap')
  await expect(result).toHaveCSS('overflow-wrap', 'anywhere')
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
})
