import { expect, test, analyticsEnabled, isAnalyticsRequest, type Page } from './fixtures';
import { heroClip, stories } from '../../src/data/home';

const ILLUSTRATION_LABEL = 'Illustration';
const CLIP_COUNT = [heroClip, ...stories.flatMap((story) => story.clips)].length;
const CAST_V2 = '{"version":2,"width":80,"height":24,"idle_time_limit":0.1}\n[0,"o","Test only\\r\\n"]\n[20,"o","Chapter two\\r\\n"]\n[30,"o","End\\r\\n"]\n';
const CAST_V3 = '{"version":3,"term":{"cols":80,"rows":24}}\n[0,"o","Test only\\r\\n"]\n[20,"o","Chapter two\\r\\n"]\n[10,"o","End\\r\\n"]\n';

test.afterEach(async ({ page }) => {
  expect(await page.pageErrors()).toEqual([]);
});

async function mountRecording(page: Page, id: string) {
  await page.waitForFunction(() => customElements.get('caudra-recording'));
  await page.evaluate((id) => {
    const figure = document.createElement('figure');
    figure.className = 'recording';
    figure.id = id;
    const scopes = document.querySelector('.recording')?.getAttributeNames().filter((name) => name.startsWith('data-astro-cid-')) ?? [];
    const classes = [...document.querySelector('.recording')!.classList].filter((name) => name.startsWith('astro-'));
    const element = document.createElement('caudra-recording');
    element.dataset.recordingId = id;
    element.dataset.recording = JSON.stringify({
      title: id, summary: 'Test only', src: `/recordings/${id}.cast`,
      poster: { src: '/recordings/test.png', alt: 'Test only', width: 800, height: 400 },
      cols: 80, rows: 24, duration: 30,
      chapters: [{ title: 'Second chapter', time: 20 }], maturity: 'stable',
    });
    element.innerHTML = `<div class="recording-stage" style="height:240px;position:relative">
      <div class="recording-poster">Test-only poster</div>
      <div class="recording-viewport" hidden><div class="recording-player"></div></div>
      </div><div class="recording-actions" hidden>
      <button data-action="play">Play recording</button><button data-action="replay" disabled>Replay</button>
      <button data-action="fullscreen" disabled>Fullscreen</button><button data-time="20" disabled>Second chapter</button>
      </div><p class="recording-status" role="status"></p>`;
    figure.append(element);
    for (const scope of scopes) for (const node of [figure, element, ...element.querySelectorAll('*')]) node.setAttribute(scope, '');
    for (const node of [figure, element, ...element.querySelectorAll('*')]) node.classList.add(...classes);
    document.body.prepend(figure);
    window.scrollTo(0, 0);
  }, id);
  return page.locator(`#${id}`);
}

async function expectRecordingEvents(page: Page, events: { event: string; data: Record<string, string> }[]) {
  const expected = analyticsEnabled ? events : [];
  const recorded = await page.evaluate(() => {
    const analytics = window as unknown as {
      analyticsEvents?: string[];
      analyticsEventData?: { event: string; data: Record<string, string> }[];
    };
    return {
      events: (analytics.analyticsEvents ?? []).filter((event) => event.startsWith('recording:')),
      data: (analytics.analyticsEventData ?? []).filter(({ event }) => event.startsWith('recording:')),
    };
  });
  expect(recorded.events).toEqual(expected.map(({ event }) => event));
  expect(recorded.data).toEqual(expected);
}

test('empty manifest shows honest illustrations without media or play controls', async ({ page, baseURL }) => {
  const requests: string[] = [];
  page.on('request', (request) => requests.push(request.url()));
  await page.goto('/');
  await expect(page.locator('.recording')).toHaveCount(CLIP_COUNT);
  for (const figure of await page.locator('.recording').all()) {
    await expect(figure.locator('.recording-label')).toHaveText(ILLUSTRATION_LABEL);
    await expect(figure.locator('[data-action="play"], video, audio, iframe, caudra-recording')).toHaveCount(0);
    await expect(figure.getByRole('button', { name: /play|watch/i })).toHaveCount(0);
  }
  expect(requests.filter((url) => /asciinema|\.cast(?:$|\?)/.test(url))).toEqual([]);
  expect(requests.filter((url) => !url.startsWith(baseURL!) && !isAnalyticsRequest(url, baseURL!))).toEqual([]);
});

test('illustration captions and summaries survive without JavaScript', async ({ browser, baseURL }) => {
  const context = await browser.newContext({ javaScriptEnabled: false });
  const page = await context.newPage();
  await page.goto(baseURL!);
  await expect(page.locator('.recording')).toHaveCount(CLIP_COUNT);
  for (const figure of await page.locator('.recording').all()) {
    await expect(figure).toBeVisible();
    await expect(figure.locator('.recording-label')).toHaveText(ILLUSTRATION_LABEL);
    await expect(figure.locator('.recording-summary')).not.toBeEmpty();
  }
  await context.close();
});

test('explicit keyboard playback loads local v2/v3 casts, chapters, replay and only one player', async ({ page, baseURL }) => {
  const requests: string[] = [];
  page.on('request', (request) => requests.push(request.url()));
  await page.route('**/recordings/*.cast', (route) => route.fulfill({ contentType: 'text/plain', body: route.request().url().endsWith('second.cast') ? CAST_V3 : CAST_V2 }));
  await page.goto('/');
  const first = await mountRecording(page, 'first');
  expect(requests.filter((url) => url.endsWith('.cast'))).toEqual([]);
  expect(requests.filter((url) => /asciinema/.test(url))).toEqual([]);
  for (const button of await first.locator('button:disabled').all()) {
    await button.dispatchEvent('click');
  }
  await expectRecordingEvents(page, []);
  await first.getByRole('button', { name: 'Play recording', exact: true }).focus();
  await page.keyboard.press('Enter');
  await expect(first.getByRole('button', { name: 'Pause recording', exact: true })).toBeVisible();
  await first.getByRole('button', { name: 'Second chapter' }).click();
  await expect(first.locator('.ap-time-elapsed')).toHaveText(/00:2/);
  await first.getByRole('button', { name: 'Replay', exact: true }).click();
  await expect(first.locator('.ap-time-elapsed')).toHaveText(/00:0/);
  await first.getByRole('button', { name: 'Pause recording', exact: true }).click();
  await expect(first.getByRole('button', { name: 'Play recording', exact: true })).toBeVisible();
  await first.getByRole('button', { name: 'Play recording', exact: true }).click();
  await expect(first.getByRole('button', { name: 'Pause recording', exact: true })).toBeVisible();
  const second = await mountRecording(page, 'second');
  await second.getByRole('button', { name: 'Play recording', exact: true }).click();
  await expect(second.getByRole('button', { name: 'Pause recording', exact: true })).toBeVisible();
  await expect(first.locator('.ap-wrapper')).toHaveCount(0);
  await expect(page.locator('.ap-wrapper')).toHaveCount(1);
  expect(requests.filter((url) => url.endsWith('.cast'))).toHaveLength(2);
  expect(requests.filter((url) => !url.startsWith(baseURL!) && !url.startsWith('data:') && !isAnalyticsRequest(url, baseURL!))).toEqual([]);
  await expectRecordingEvents(page, [
    { event: 'recording:play-click', data: { recording: 'first' } },
    { event: 'recording:chapter-click', data: { recording: 'first', time: '20' } },
    { event: 'recording:replay-click', data: { recording: 'first' } },
    { event: 'recording:pause-click', data: { recording: 'first' } },
    { event: 'recording:play-click', data: { recording: 'first' } },
    { event: 'recording:play-click', data: { recording: 'second' } },
  ]);
});

test('offscreen and hidden playback pauses without auto-resume and disconnect releases resources', async ({ page }) => {
  await page.route('**/recordings/*.cast', (route) => route.fulfill({ contentType: 'text/plain', body: CAST_V2 }));
  await page.goto('/');
  const recording = await mountRecording(page, 'lifecycle');
  await recording.getByRole('button', { name: 'Play recording', exact: true }).click();
  await expect(recording.getByRole('button', { name: 'Pause recording', exact: true })).toBeVisible();
  await page.evaluate(() => window.scrollTo(0, document.body.scrollHeight));
  await expect(recording.locator('[data-action="play"]')).toHaveText('Play recording');
  await recording.scrollIntoViewIfNeeded();
  await expect(recording.locator('[data-action="play"]')).toHaveText('Play recording');
  await recording.getByRole('button', { name: 'Play recording', exact: true }).click();
  await expect(recording.getByRole('button', { name: 'Pause recording', exact: true })).toBeVisible();
  await page.evaluate(() => {
    Object.defineProperty(document, 'hidden', { configurable: true, value: true });
    document.dispatchEvent(new Event('visibilitychange'));
  });
  await expect(recording.locator('[data-action="play"]')).toHaveText('Play recording');
  await page.evaluate(() => {
    Object.defineProperty(document, 'hidden', { configurable: true, value: false });
    document.dispatchEvent(new Event('visibilitychange'));
  });
  await expect(recording.locator('[data-action="play"]')).toHaveText('Play recording');
  await recording.evaluate((figure) => {
    const element = figure.querySelector('caudra-recording')!;
    element.remove();
    figure.append(element);
  });
  await expect(recording.locator('.ap-wrapper')).toHaveCount(0);
  await recording.getByRole('button', { name: 'Play recording', exact: true }).click();
  await expect(recording.getByRole('button', { name: 'Pause recording', exact: true })).toBeVisible();
  await expect(recording.locator('.ap-wrapper')).toHaveCount(1);
  await expectRecordingEvents(page, Array.from({ length: 3 }, () => ({
    event: 'recording:play-click', data: { recording: 'lifecycle' },
  })));
});

test('load and parse errors restore a useful poster and allow retry', async ({ page }) => {
  await page.route('**/recordings/*.cast', (route) => route.fulfill({ contentType: 'text/plain', body: 'not a cast' }));
  await page.goto('/');
  const recording = await mountRecording(page, 'failure');
  await recording.getByRole('button', { name: 'Play recording', exact: true }).click();
  await expect(recording.getByRole('status')).toContainText('Recording could not be loaded');
  await expect(recording.locator('.recording-poster')).toBeVisible();
  await expect(recording.locator('.ap-wrapper')).toHaveCount(0);
  await page.route('**/recordings/*.cast', (route) => route.fulfill({ contentType: 'text/plain', body: CAST_V2 }));
  await recording.getByRole('button', { name: 'Play recording', exact: true }).click();
  await expect(recording.getByRole('button', { name: 'Pause recording', exact: true })).toBeVisible();
});

for (const unavailable of ['script', 'global', 'throwing'] as const) {
  test(`playback works with ${unavailable} analytics unavailable`, async ({ page }) => {
    await page.route('**/recordings/*.cast', (route) => route.fulfill({ contentType: 'text/plain', body: CAST_V2 }));
    await page.goto('/');
    await page.evaluate((unavailable) => {
      if (unavailable === 'script') document.querySelector('script[data-umami-script]')?.remove();
      else if (unavailable === 'global') delete window.umami;
      else window.umami = { track: () => { throw new Error('Analytics unavailable'); } };
    }, unavailable);
    const recording = await mountRecording(page, 'untracked');
    await recording.getByRole('button', { name: 'Play recording', exact: true }).click();
    await expect(recording.getByRole('button', { name: 'Pause recording', exact: true })).toBeVisible();
    await recording.getByRole('button', { name: 'Pause recording', exact: true }).click();
    await expect(recording.getByRole('button', { name: 'Play recording', exact: true })).toBeVisible();
    await expectRecordingEvents(page, []);
  });
}

test('fullscreen is keyboard accessible and playback remains opt-in with reduced motion', async ({ page }) => {
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await page.route('**/recordings/*.cast', (route) => route.fulfill({ contentType: 'text/plain', body: CAST_V2 }));
  await page.goto('/');
  const recording = await mountRecording(page, 'accessible');
  await expect(recording.locator('.ap-wrapper')).toHaveCount(0);
  await recording.getByRole('button', { name: 'Play recording', exact: true }).click();
  await expect(recording.getByRole('button', { name: 'Pause recording', exact: true })).toBeVisible();
  const fullscreen = recording.getByRole('button', { name: 'Fullscreen', exact: true });
  const fullscreenEnabled = await page.evaluate(() => document.fullscreenEnabled);
  if (fullscreenEnabled) {
    await fullscreen.focus();
    await page.keyboard.press('Enter');
    await expect.poll(() => page.evaluate(() => document.fullscreenElement?.classList.contains('recording-stage'))).toBe(true);
    await page.evaluate(() => document.exitFullscreen());
  } else {
    await expect(fullscreen).toBeDisabled();
  }
  await page.emulateMedia({ forcedColors: 'active' });
  await recording.getByRole('button', { name: 'Pause recording', exact: true }).focus();
  await page.keyboard.press('Enter');
  await expect(recording.getByRole('button', { name: 'Play recording', exact: true })).toBeFocused();
  await expectRecordingEvents(page, [
    { event: 'recording:play-click', data: { recording: 'accessible' } },
    ...(fullscreenEnabled ? [{ event: 'recording:fullscreen-click', data: { recording: 'accessible' } }] : []),
    { event: 'recording:pause-click', data: { recording: 'accessible' } },
  ]);
});

test('a pending fetch cannot resume a recording hidden during loading', async ({ page }) => {
  let deliver = () => {};
  const response = new Promise<void>((resolve) => { deliver = resolve; });
  await page.route('**/recordings/*.cast', async (route) => {
    await response;
    await route.fulfill({ contentType: 'text/plain', body: CAST_V2 });
  });
  await page.goto('/');
  const recording = await mountRecording(page, 'pending');
  const request = page.waitForRequest('**/recordings/pending.cast');
  await recording.getByRole('button', { name: 'Play recording', exact: true }).click();
  await request;
  await expect(recording.locator('[data-action="play"]')).toBeDisabled();
  await recording.locator('[data-action="play"]').dispatchEvent('click');
  await page.evaluate(() => {
    Object.defineProperty(document, 'hidden', { configurable: true, value: true });
    document.dispatchEvent(new Event('visibilitychange'));
  });
  deliver();
  await expect(recording.locator('[data-action="play"]')).toBeEnabled();
  await expect(recording.locator('[data-action="play"]')).toHaveText('Play recording');
  await expect(recording.locator('.ap-playback-button')).toHaveAttribute('aria-label', 'Play');
  await expectRecordingEvents(page, [
    { event: 'recording:play-click', data: { recording: 'pending' } },
  ]);
});

test('stylesheet failure restores the poster and retry shares one loaded stylesheet', async ({ page }) => {
  let stylesheetRequests = 0;
  const casts: string[] = [];
  await page.route(/asciinema-player.*\.css/, (route) => {
    if (route.request().resourceType() === 'stylesheet') {
      stylesheetRequests++;
      if (stylesheetRequests === 1) return route.abort();
    }
    return route.continue();
  });
  await page.route('**/recordings/*.cast', (route) => {
    casts.push(route.request().url());
    return route.fulfill({ contentType: 'text/plain', body: CAST_V2 });
  });
  await page.goto('/');
  const first = await mountRecording(page, 'style-failure');
  expect(stylesheetRequests).toBe(0);
  await expect(page.locator('link[data-recording-styles]')).toHaveCount(0);
  await first.getByRole('button', { name: 'Play recording', exact: true }).click();
  await expect(first.getByRole('status')).toContainText('Recording could not be loaded');
  await expect(first.locator('.recording-poster')).toBeVisible();
  await expect(page.locator('link[data-recording-styles]')).toHaveCount(0);
  expect(casts).toEqual([]);
  await first.getByRole('button', { name: 'Play recording', exact: true }).click();
  await expect(first.getByRole('button', { name: 'Pause recording', exact: true })).toBeVisible();
  const second = await mountRecording(page, 'style-reuse');
  await second.getByRole('button', { name: 'Play recording', exact: true }).click();
  await expect(second.getByRole('button', { name: 'Pause recording', exact: true })).toBeVisible();
  await expect(page.locator('link[data-recording-styles]')).toHaveCount(1);
  expect(stylesheetRequests).toBe(2);
});

test('disconnect during stylesheet loading does not mount a stale player or duplicate the stylesheet', async ({ page }) => {
  let deliver = () => {};
  const response = new Promise<void>((resolve) => { deliver = resolve; });
  await page.route(/asciinema-player.*\.css/, async (route) => {
    if (route.request().resourceType() === 'stylesheet') await response;
    await route.continue();
  });
  await page.route('**/recordings/*.cast', (route) => route.fulfill({ contentType: 'text/plain', body: CAST_V2 }));
  await page.goto('/');
  const first = await mountRecording(page, 'style-pending');
  const request = page.waitForRequest((request) => request.resourceType() === 'stylesheet' && request.url().includes('asciinema-player'));
  await first.getByRole('button', { name: 'Play recording', exact: true }).click();
  await request;
  await first.evaluate((figure) => {
    const element = figure.querySelector('caudra-recording')!;
    element.remove();
    figure.append(element);
  });
  const second = await mountRecording(page, 'style-current');
  await second.getByRole('button', { name: 'Play recording', exact: true }).click();
  deliver();
  await expect(second.getByRole('button', { name: 'Pause recording', exact: true })).toBeVisible();
  await expect(first.locator('.ap-wrapper')).toHaveCount(0);
  await expect(first.locator('[data-action="play"]')).toHaveText('Play recording');
  await expect(page.locator('link[data-recording-styles]')).toHaveCount(1);
});
