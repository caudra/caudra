import { expect, test } from '@playwright/test';
import { HEADLINE, METHOD_NOTE } from '../../src/data/home';
import { COPY_STATUS, INSTALL_COMMAND } from '../../src/data/install';

const VIEWPORT_WIDTHS = [320, 390, 768, 1024, 1280, 1600];
const SCROLLBAR_COLORS = {
  dark: 'rgb(104, 108, 124) rgb(20, 21, 26)',
  light: 'rgb(133, 136, 150) rgb(245, 245, 242)',
};

test('homepage, responsive docs, deep links and copy stay local', async ({ page, baseURL }, testInfo) => {
  const external: string[] = [];
  const errors: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('request', (request) => { if (!request.url().startsWith(baseURL!) && !request.url().startsWith('data:')) external.push(request.url()); });
  await page.goto('/');
  await expect(page.locator('html')).toHaveCSS('scrollbar-width', 'auto');
  await expect(page.locator('html')).toHaveCSS('scrollbar-color', SCROLLBAR_COLORS.dark);
  await expect(page.locator('html')).toHaveCSS('color-scheme', 'dark');
  await expect(page.getByRole('heading', { level: 1 })).toHaveText(HEADLINE);
  await page.keyboard.press('Tab');
  await expect(page.getByRole('link', { name: 'Skip to content' })).toBeFocused();
  const steer = page.locator('#steer');
  await expect(steer.getByRole('button', { name: 'Guide a subagent' })).toHaveAttribute('aria-pressed', 'true');
  await expect(page.locator('#recording-btw')).toBeHidden();
  await steer.getByRole('button', { name: 'Ask /btw' }).click();
  await expect(page.locator('#recording-btw')).toBeVisible();
  await expect(page.locator('#recording-steer')).toBeHidden();
  await expect(steer.getByRole('button', { name: 'Ask /btw' })).toHaveAttribute('aria-pressed', 'true');
  await steer.getByRole('button', { name: 'Guide a subagent' }).focus();
  await page.keyboard.press('Enter');
  await expect(page.locator('#recording-steer')).toBeVisible();
  const heroInstall = page.locator('#hero-install');
  await heroInstall.getByRole('button', { name: 'Copy install command' }).click();
  await expect(heroInstall.getByRole('status')).toHaveText(COPY_STATUS.copied);
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(INSTALL_COMMAND);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.evaluate(() => window.scrollTo({ top: 0, behavior: 'instant' }));
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
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Nothing at this address.');
  expect((await page.request.get('/docs/not-a-file.example.toml')).status()).toBe(404);
  expect(external).toEqual([]);
  expect(errors).toEqual([]);
});

test('homepage handles narrow screens, zoom, motion preferences, and copy failure', async ({ page }, testInfo) => {
  await page.goto('/');
  await page.evaluate(() => Object.defineProperty(navigator.clipboard, 'writeText', { value: () => Promise.reject(new Error('Clipboard unavailable')) }));
  const heroInstall = page.locator('#hero-install');
  await heroInstall.getByRole('button', { name: 'Copy install command' }).click();
  await expect(heroInstall.getByRole('status')).toHaveText(COPY_STATUS.failed);
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await expect(page.locator('html')).toHaveCSS('scroll-behavior', 'auto');
  for (const width of VIEWPORT_WIDTHS) {
    await page.setViewportSize({ width, height: 900 });
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `horizontal overflow at ${width}px`).toBe(true);
  }
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.locator('html').evaluate((element) => { element.style.fontSize = '200%'; });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'horizontal overflow at 200% text zoom').toBe(true);
  await page.locator('html').evaluate((element) => { element.style.fontSize = ''; });
  await page.setViewportSize({ width: 768, height: 1024 });
  await page.evaluate(() => window.scrollTo({ top: 0, behavior: 'instant' }));
  await page.screenshot({ path: testInfo.outputPath('homepage-tablet.png'), fullPage: true });
  await page.emulateMedia({ forcedColors: 'active' });
  await expect(page.locator('html')).toHaveCSS('scrollbar-color', 'auto');
});

test('homepage remains useful without JavaScript', async ({ browser, baseURL }) => {
  const context = await browser.newContext({ javaScriptEnabled: false });
  const page = await context.newPage();
  await page.goto(baseURL!);
  await expect(page.getByRole('heading', { level: 1 })).toHaveText(HEADLINE);
  await expect(page.locator('#hero-install [data-copy-source]')).toHaveText(INSTALL_COMMAND);
  await expect(page.locator('[data-copy]')).toHaveCount(2);
  for (const button of await page.locator('[data-copy]').all()) await expect(button).toBeHidden();
  await expect(page.locator('#steer .clip-tab-list')).toBeHidden();
  await expect(page.locator('#recording-steer')).toBeVisible();
  await expect(page.locator('#recording-btw')).toBeVisible();
  await expect(page.locator('#method')).toContainText(METHOD_NOTE);
  await page.getByRole('link', { name: 'Read the quick start' }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Quick Start');
  await context.close();
});
