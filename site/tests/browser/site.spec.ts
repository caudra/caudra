import { expect, test } from '@playwright/test';

const SCROLLBAR_COLORS = {
  dark: 'rgb(101, 118, 142) rgb(9, 37, 77)',
  light: 'rgb(138, 129, 115) rgb(242, 235, 221)',
};

test('homepage, responsive docs, deep links and copy stay local', async ({ page, baseURL }, testInfo) => {
  const external: string[] = [];
  const errors: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('request', (request) => { if (!request.url().startsWith(baseURL!) && !request.url().startsWith('data:')) external.push(request.url()); });
  await page.goto('/');
  await expect(page.locator('html')).toHaveCSS('scrollbar-width', 'auto');
  await expect(page.locator('html')).toHaveCSS('scrollbar-color', SCROLLBAR_COLORS.light);
  await expect(page.locator('html')).toHaveCSS('color-scheme', 'light');
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Context into effective action.');
  await page.keyboard.press('Tab');
  await expect(page.getByRole('link', { name: 'Skip to content' })).toBeFocused();
  await page.getByRole('button', { name: 'Copy install command' }).click();
  await expect(page.getByRole('status')).toHaveText('Install command copied.');
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe('curl -fsSL https://caudra.ai/install.sh | sh');
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.screenshot({ path: testInfo.outputPath('homepage.png'), fullPage: true });

  await page.goto('/docs/tools/#file_read');
  const theme = testInfo.project.name.endsWith('dark') ? 'dark' : 'light';
  await expect(page.locator('html')).toHaveCSS('scrollbar-width', 'auto');
  await expect(page.locator('html')).toHaveCSS('scrollbar-color', SCROLLBAR_COLORS[theme]);
  await expect(page.locator('html')).toHaveCSS('color-scheme', theme);
  await expect(page.locator('body')).toHaveCSS('scrollbar-color', 'auto');
  await expect(page.locator('.sidebar-pane')).toHaveCSS('scrollbar-width', 'thin');
  await expect(page.locator('.sidebar-pane')).toHaveCSS('scrollbar-color', SCROLLBAR_COLORS[theme]);
  await expect(page.locator('h3#file_read')).toBeInViewport();
  const mobile = testInfo.project.name.startsWith('mobile');
  if (mobile) await page.locator('#starlight__mobile-toc summary').click();
  const toc = mobile ? page.locator('mobile-starlight-toc') : page.locator('starlight-toc');
  await toc.locator('a[href="#shell"]').click();
  await expect(page.locator('h3#shell')).toBeInViewport();
  await expect(page.locator('html')).toHaveAttribute('data-theme', theme);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.getByRole('button', { name: 'Copy page Markdown' }).click();
  await expect(page.locator('.copy-status')).toHaveText('Copied');
  expect(await page.evaluate(() => navigator.clipboard.readText())).toMatch(/^# Tools\n/);
  await page.screenshot({ path: testInfo.outputPath('docs.png'), fullPage: false });
  await page.goto('/docs/quick-start/');
  const copy = page.locator('.expressive-code .copy button').first();
  await copy.focus();
  await copy.click();
  expect((await page.evaluate(() => navigator.clipboard.readText())).length).toBeGreaterThan(0);
  await page.goto('/docs/configuration/');
  await expect(page.locator('.sl-markdown-content table').first()).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.emulateMedia({ reducedMotion: 'reduce' });
  expect(await page.evaluate(() => getComputedStyle(document.documentElement).scrollBehavior)).toBe('auto');
  await page.emulateMedia({ forcedColors: 'active' });
  await expect(page.locator('html')).toHaveCSS('scrollbar-color', 'auto');
  expect(external).toEqual([]);
  expect(errors).toEqual([]);
});

test('Starlight search, theme, mobile navigation and diagrams work', async ({ page, baseURL }, testInfo) => {
  const external: string[] = [];
  const errors: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('request', (request) => { if (!request.url().startsWith(baseURL!) && !request.url().startsWith('data:')) external.push(request.url()); });
  await page.goto('/docs/');
  await page.getByRole('button', { name: /Search/ }).click();
  await page.getByRole('textbox', { name: 'Search', exact: true }).fill('permissions');
  await expect(page.locator('.pagefind-ui__result-link').first()).toBeVisible();
  await page.keyboard.press('Escape');
  if (testInfo.project.name.startsWith('mobile')) await page.getByRole('button', { name: 'Menu' }).click();
  await page.getByRole('combobox', { name: 'Select theme' }).selectOption('dark');
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  await expect(page.locator('html')).toHaveCSS('scrollbar-color', SCROLLBAR_COLORS.dark);
  await expect(page.locator('html')).toHaveCSS('color-scheme', 'dark');
  if (testInfo.project.name.startsWith('mobile')) await page.getByRole('button', { name: 'Menu' }).click();
  await page.goto('/docs/messaging/');
  await expect(page.locator('.diagram-render svg')).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.goto('/docs/markdown/');
  await expect(page.locator('.diagram')).toHaveCount(0);
  const response = await page.goto('/this-page-does-not-exist/');
  expect(response?.status()).toBe(404);
  await expect(page.getByRole('heading', { level: 1 })).toContainText('404');
  expect((await page.request.get('/docs/not-a-file.example.toml')).status()).toBe(404);
  expect(external).toEqual([]);
  expect(errors).toEqual([]);
});
