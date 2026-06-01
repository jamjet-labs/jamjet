import { test, expect } from '@playwright/test'

test('cost story: detect waste, prevent with cache_inject, refund→approve→audit', async ({ page }) => {
  await page.goto('/')
  await expect(page.getByTestId('mode-badge')).toBeVisible()
  await page.getByTestId('preset-ask5').click()
  await expect(page.getByTestId('waste-alert').first()).toBeVisible({ timeout: 30000 })   // detection
  await page.getByTestId('preset-enableCache').click()
  await expect(page.getByTestId('cache-saved').first()).toBeVisible({ timeout: 30000 })    // prevention
  await page.getByTestId('preset-refund').click()
  await expect(page.getByTestId('approval-card')).toBeVisible({ timeout: 30000 })
  await page.getByTestId('approve-btn').click()
  await expect(page.getByTestId('audit-entry').first()).toBeVisible({ timeout: 30000 })    // governance
})
