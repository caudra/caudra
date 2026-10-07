import { ANALYTICS_STUB, analyticsEnabled, expect, isAnalyticsRequest, test, type Page } from './fixtures';
import { COPY_STATUS } from '../../src/data/install';

const HERO_EVENT = 'section:hero';
const COPY_EVENT = 'install:copy-click-hero-install';
const DOCS_COPY_EVENT = 'docs:copy-markdown-click';
const DOCS_SECTION_EVENT = 'docs:section';
const CODE_COPY_EVENT = 'docs:copy-code-click';
const DOWNLOAD_EVENT = 'docs:download-click';
const PROVIDERS_PAGE = '/docs/providers/';
const REFERENCE_CONFIGS_PAGE = '/docs/reference-configs/';

function events(page: Page) {
  return page.evaluate(() => (window as unknown as { analyticsEvents: string[] }).analyticsEvents);
}

function eventData(page: Page) {
  return page.evaluate(() => (window as unknown as { analyticsEventData: { event: string; data: Record<string, string> }[] }).analyticsEventData);
}

async function changeFragment(page: Page, hash: string) {
  await page.evaluate((hash) => new Promise<void>((resolve) => {
    window.addEventListener('hashchange', () => resolve(), { once: true });
    location.hash = hash;
  }), hash);
}

test.afterEach(async ({ page }) => {
  expect(await page.pageErrors()).toEqual([]);
});

test('unconfigured analytics stays inert even if a tracker global exists', async ({ page }) => {
  test.skip(analyticsEnabled);
  await page.addInitScript(() => {
    const recorded: string[] = [];
    Object.assign(window, { analyticsEvents: recorded, umami: { track: (event: string) => recorded.push(event) } });
  });
  for (const path of ['/', '/docs/quick-start/', REFERENCE_CONFIGS_PAGE, `${PROVIDERS_PAGE}#mistral`, '/404.html']) {
    await page.goto(path);
    await expect(page.locator('script[data-umami-script]')).toHaveCount(0);
    await expect(page.locator('a[data-umami-event^="link:"]')).toHaveCount(0);
    await expect(page.locator('a[data-umami-event-destination]')).toHaveCount(0);
    await expect(page.locator(`[data-umami-event="${CODE_COPY_EVENT}"], [data-umami-event="${DOWNLOAD_EVENT}"]`)).toHaveCount(0);
    expect(await events(page)).toEqual([]);
  }
});

test.describe('configured analytics', () => {
  test.skip(!analyticsEnabled);

  test('tracker loads once per page and explicit click events stay distinct', async ({ page, baseURL }) => {
    const requests: string[] = [];
    page.on('request', (request) => { if (isAnalyticsRequest(request.url(), baseURL!)) requests.push(request.url()); });
    await page.addInitScript(() => document.addEventListener('click', (event) => {
      if (event.target instanceof Element && event.target.closest('a')) event.preventDefault();
    }, true));
    await page.goto('/');
    await expect.poll(() => events(page)).toContain(HERO_EVENT);
    await page.locator('#hero-install [data-copy]').click();
    await page.locator('[data-umami-event="nav:github"]').click();
    await page.locator('[data-umami-event="github:footer-example-config"]').click();
    const homeEvents = await events(page);
    expect(homeEvents.filter((event) => event === COPY_EVENT)).toHaveLength(1);
    expect(homeEvents.filter((event) => event === 'nav:github')).toHaveLength(1);
    expect(homeEvents).not.toContain('link:github.com');
    expect(await eventData(page)).toEqual([
      { event: 'nav:github', data: { destination: 'https://github.com/caudra/caudra' } },
      { event: 'github:footer-example-config', data: { destination: 'https://github.com/caudra/config' } },
    ]);
    expect(homeEvents.filter((event) => event.startsWith('pageview:'))).toEqual(['pageview:/']);

    await page.goto('/docs/quick-start/');
    await page.locator('[data-copy-markdown]').click();
    await page.locator('[data-umami-event="docs:view-markdown"]').click();
    const docsEvents = await events(page);
    expect(docsEvents).toEqual(['pageview:/docs/quick-start/', DOCS_COPY_EVENT, 'docs:view-markdown']);
    const outbound = page.locator('.sl-markdown-content a[data-umami-event^="link:"]').first();
    await expect(outbound).toHaveAttribute('data-umami-event', /^link:/);
    const outboundEvent = await outbound.getAttribute('data-umami-event');
    const outboundUrl = new URL((await outbound.getAttribute('href'))!);
    await outbound.click();
    expect((await events(page)).filter((event) => event === outboundEvent)).toHaveLength(1);
    expect(await eventData(page)).toEqual([{ event: outboundEvent, data: { destination: `${outboundUrl.origin}${outboundUrl.pathname}` } }]);

    await page.goto('/404.html');
    await page.locator('[data-umami-event="404:home"]').click();
    expect(await events(page)).toEqual(['pageview:/404.html', '404:home']);
    expect(requests).toHaveLength(3);
  });

  test('outbound events include sanitized destinations and leave internal and special links alone', async ({ page }) => {
    await page.addInitScript(() => document.addEventListener('click', (event) => {
      if (event.target instanceof Element && event.target.closest('a')) event.preventDefault();
    }, true));
    await page.route('**/docs/quick-start/', async (route) => {
      const response = await route.fetch();
      const body = (await response.text()).replace('</main>', `<nav id="analytics-links">
        <a href="https://www.example.test/path?private=value#fragment"><span>External</span></a>
        <a href="https://username:password@example.test:8443/another/path?private=value#fragment" data-umami-event="resource:explicit">Explicit</a>
        <a href="/docs/">Relative</a><a href="https://caudra.ai/docs/">Canonical</a>
        <a href="#top">Anchor</a><a href="mailto:test@example.test">Email</a>
        <a href="tel:123">Phone</a><a href="http://[">Malformed</a>
      </nav></main>`);
      await route.fulfill({ response, body });
    });
    await page.goto('/docs/quick-start/');
    const links = page.locator('#analytics-links a');
    await expect(links.nth(0)).toHaveAttribute('data-umami-event', 'link:example.test');
    await expect(links.nth(0)).toHaveAttribute('data-umami-event-destination', 'https://www.example.test/path');
    await expect(links.nth(0)).toHaveAttribute('href', 'https://www.example.test/path?private=value#fragment');
    await expect(links.nth(0)).not.toHaveAttribute('target');
    await expect(links.nth(1)).toHaveAttribute('data-umami-event', 'resource:explicit');
    await expect(links.nth(1)).toHaveAttribute('data-umami-event-destination', 'https://example.test:8443/another/path');
    for (const link of (await links.all()).slice(2)) {
      await expect(link).not.toHaveAttribute('data-umami-event');
      await expect(link).not.toHaveAttribute('data-umami-event-destination');
    }
    await links.nth(0).locator('span').click();
    await links.nth(1).click();
    expect(await events(page)).toEqual(['pageview:/docs/quick-start/', 'link:example.test', 'resource:explicit']);
    expect(await eventData(page)).toEqual([
      { event: 'link:example.test', data: { destination: 'https://www.example.test/path' } },
      { event: 'resource:explicit', data: { destination: 'https://example.test:8443/another/path' } },
    ]);
  });

  test('homepage sections are visible on mobile and tracked only once', async ({ page }) => {
    await page.goto('/');
    await expect.poll(() => events(page)).toContain(HERO_EVENT);
    await page.locator('#why h2').scrollIntoViewIfNeeded();
    await expect.poll(() => events(page)).toContain('section:why');
    await page.locator('#hero h1').scrollIntoViewIfNeeded();
    await expect(page.locator('#hero h1')).toBeInViewport();
    await page.locator('#why h2').scrollIntoViewIfNeeded();
    await expect(page.locator('#why h2')).toBeInViewport();
    const recorded = await events(page);
    expect(recorded.filter((event) => event === HERO_EVENT)).toHaveLength(1);
    expect(recorded.filter((event) => event === 'section:why')).toHaveLength(1);
  });

  test('code copy clicks identify the page and block without collecting code', async ({ page }) => {
    await page.goto('/docs/quick-start/');
    const buttons = page.locator('.sl-markdown-content .expressive-code .copy button');
    await buttons.nth(0).focus();
    await page.keyboard.press('Enter');
    expect((await page.evaluate(() => navigator.clipboard.readText())).length).toBeGreaterThan(0);
    await buttons.nth(1).focus();
    await buttons.nth(1).locator('div').click();
    expect((await events(page)).filter((event) => event === CODE_COPY_EVENT)).toHaveLength(2);
    expect(await eventData(page)).toEqual([
      { event: CODE_COPY_EVENT, data: { page: '/docs/quick-start/', block: '1' } },
      { event: CODE_COPY_EVENT, data: { page: '/docs/quick-start/', block: '2' } },
    ]);
  });

  test('docs downloads identify the local file and preserve explicit events', async ({ page }) => {
    await page.addInitScript(() => document.addEventListener('click', (event) => {
      if (event.target instanceof Element && event.target.closest('a')) event.preventDefault();
    }, true));
    await page.route(`**${REFERENCE_CONFIGS_PAGE}`, async (route) => {
      const response = await route.fetch();
      const body = (await response.text()).replace('</main>', `<div class="sl-markdown-content" id="download-fixtures">
        <a href="/docs/example.json?private=value#fragment">JSON</a>
        <a href="https://caudra.ai/docs/example.YAML">YAML</a>
        <a href="/docs/example.yml">YML</a>
        <a href="/docs/archive.zip?private=value" download data-umami-event="docs:archive-click">Archive</a>
        <a href="/docs/providers/">Regular page</a>
        <a href="/docs/missing.toml/not-a-file">Not a config file</a>
      </div></main>`);
      await route.fulfill({ response, body });
    });
    await page.goto(REFERENCE_CONFIGS_PAGE);
    const configs = page.locator('.sl-markdown-content a[href$=".example.toml"]');
    await expect(configs.first()).toHaveAttribute('data-umami-event', DOWNLOAD_EVENT);
    for (const config of await configs.all()) {
      await expect(config).toHaveAttribute('data-umami-event', DOWNLOAD_EVENT);
      await expect(config).toHaveAttribute('data-umami-event-destination', (await config.getAttribute('href'))!);
    }
    const configPath = (await configs.first().getAttribute('href'))!;
    await configs.first().click();
    const expected = [{ event: DOWNLOAD_EVENT, data: { page: REFERENCE_CONFIGS_PAGE, destination: configPath } }];
    const samples = page.locator('#download-fixtures a');
    for (const [index, destination] of ['/docs/example.json', '/docs/example.YAML', '/docs/example.yml', '/docs/archive.zip'].entries()) {
      const event = index === 3 ? 'docs:archive-click' : DOWNLOAD_EVENT;
      await expect(samples.nth(index)).toHaveAttribute('data-umami-event', event);
      await samples.nth(index).click();
      expected.push({ event, data: { page: REFERENCE_CONFIGS_PAGE, destination } });
    }
    for (const link of (await samples.all()).slice(4)) await expect(link).not.toHaveAttribute('data-umami-event');
    expect(await eventData(page)).toEqual(expected);
    expect(await events(page)).toEqual([`pageview:${REFERENCE_CONFIGS_PAGE}`, ...expected.map(({ event }) => event)]);
  });

  test('docs deep links and heading navigation emit one section event per heading', async ({ page }) => {
    await page.goto(`${PROVIDERS_PAGE}#mistral`);
    const expected = [{ event: DOCS_SECTION_EVENT, data: { page: PROVIDERS_PAGE, section: 'mistral' } }];
    await expect.poll(() => eventData(page)).toEqual(expected);
    expect((await events(page)).filter((event) => event.startsWith('pageview:'))).toEqual([`pageview:${PROVIDERS_PAGE}`]);

    await page.locator('.sl-markdown-content a[href="#openai"]').click();
    expected.push({ event: DOCS_SECTION_EVENT, data: { page: PROVIDERS_PAGE, section: 'openai' } });
    await expect.poll(() => eventData(page)).toEqual(expected);
    await page.goBack();
    await expect(page).toHaveURL(/#mistral$/);
    expect(await eventData(page)).toEqual(expected);
    await changeFragment(page, '%6Distral');
    expect(await eventData(page)).toEqual(expected);

    await page.locator('.sl-markdown-content #anthropic').scrollIntoViewIfNeeded();
    await expect(page.locator('.sl-markdown-content #anthropic')).toBeInViewport();
    expect(await eventData(page)).toEqual(expected);
    expect((await events(page)).filter((event) => event.startsWith('pageview:'))).toEqual([`pageview:${PROVIDERS_PAGE}`]);
  });

  test('docs ignore unknown, malformed, empty, and non-content fragments', async ({ page }) => {
    await page.goto(`${PROVIDERS_PAGE}?test=ignored#unrecognized-value`);
    expect(await eventData(page)).toEqual([]);
    for (const hash of ['%E0%A4%A', '_top', 'starlight__on-this-page', '']) {
      await changeFragment(page, hash);
      expect(await eventData(page)).toEqual([]);
    }
    await changeFragment(page, '%6Distral');
    expect(await eventData(page)).toEqual([{ event: DOCS_SECTION_EVENT, data: { page: PROVIDERS_PAGE, section: 'mistral' } }]);
  });

  for (const [path, event] of [['/', HERO_EVENT], [`${PROVIDERS_PAGE}#mistral`, DOCS_SECTION_EVENT]]) {
    test(`a slow tracker starts section tracking after it loads on ${path}`, async ({ page, baseURL }) => {
      let release!: () => void;
      const ready = new Promise<void>((resolve) => { release = resolve; });
      await page.route((url) => isAnalyticsRequest(url.href, baseURL!), async (route) => {
        await ready;
        await route.fulfill({ contentType: 'text/javascript', body: ANALYTICS_STUB });
      });
      await page.goto(path, { waitUntil: 'commit' });
      try {
        await expect(page.locator('h1')).toBeAttached();
        await expect(page.locator('a[data-umami-event^="link:"]').first()).toHaveAttribute('data-umami-event', /^link:/);
        expect(await page.evaluate(() => window.umami)).toBeUndefined();
      } finally {
        release();
      }
      await page.waitForLoadState('load');
      await expect.poll(() => events(page)).toContain(event);
      expect((await events(page)).filter((recorded) => recorded === event)).toHaveLength(1);
      if (event === DOCS_SECTION_EVENT) {
        expect(await eventData(page)).toEqual([{ event, data: { page: PROVIDERS_PAGE, section: 'mistral' } }]);
      }
    });
  }

  test('section events require the visibility threshold and ignore repeated entries', async ({ page }) => {
    await page.addInitScript(`
      window.IntersectionObserver = class {
        targets = new Set();
        constructor(callback, options) {
          this.callback = callback;
          this.threshold = options?.threshold;
          if (this.threshold === 0.3) window.analyticsObserver = this;
        }
        observe(target) { this.targets.add(target); }
        unobserve(target) { this.targets.delete(target); }
        disconnect() { this.targets.clear(); }
        emit(ratio, intersecting = true) {
          const entries = [...this.targets].map(target => ({ target, intersectionRatio: ratio, isIntersecting: intersecting }));
          this.callback([...entries, ...entries], this);
        }
      };
    `);
    await page.goto('/');
    expect(await page.evaluate('window.analyticsObserver.threshold')).toBe(0.3);
    await page.evaluate('window.analyticsObserver.emit(0.29)');
    await page.evaluate('window.analyticsObserver.emit(0.3, false)');
    expect(await events(page)).toEqual(['pageview:/']);
    await page.evaluate('window.analyticsObserver.emit(0.3)');
    await page.evaluate('window.analyticsObserver.emit(1)');
    const recorded = (await events(page)).filter((event) => event.startsWith('section:'));
    const sections = await page.locator('main > section[id]').evaluateAll((nodes) => nodes.map((node) => `section:${node.id}`));
    expect(recorded).toEqual(sections);
  });

  test('blocked tracking leaves homepage and docs interactions working', async ({ page, baseURL }) => {
    await page.route((url) => isAnalyticsRequest(url.href, baseURL!), (route) => route.abort());
    await page.goto('/');
    await page.locator('#hero-install [data-copy]').click();
    await expect(page.locator('#hero-install [data-copy-status]')).toHaveText(COPY_STATUS.copied);
    await page.locator('[data-umami-event="nav:docs"]').click();
    await expect(page).toHaveURL(/\/docs\/$/);
    await page.locator('[data-copy-markdown]').click();
    await expect(page.locator('.copy-status')).toHaveText('Copied');
    await page.goto('/docs/quick-start/');
    const codeCopy = page.locator('.expressive-code .copy button').first();
    await codeCopy.focus();
    await codeCopy.click();
    expect((await page.evaluate(() => navigator.clipboard.readText())).length).toBeGreaterThan(0);
    await page.goto(`${PROVIDERS_PAGE}#mistral`);
    await changeFragment(page, 'openai');
    await expect(page).toHaveURL(/#openai$/);
    expect(await page.evaluate(() => window.umami)).toBeUndefined();
  });
});
