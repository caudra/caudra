import { expect, test as base } from '@playwright/test';

export { expect, type Page } from '@playwright/test';

const scriptUrl = process.env.VITE_UMAMI_SCRIPT_URL?.trim();
export const analyticsEnabled = Boolean(scriptUrl && process.env.VITE_UMAMI_WEBSITE_ID?.trim());
export const ANALYTICS_STUB = `
  window.analyticsEvents = ['pageview:' + location.pathname + location.search + (document.currentScript.dataset.excludeHash === 'true' ? '' : location.hash)];
  window.analyticsEventData = [];
  window.umami = { track: (event, data) => {
    window.analyticsEvents.push(event);
    if (data) window.analyticsEventData.push({ event, data });
  } };
  document.addEventListener('click', (event) => {
    const target = event.target instanceof Element ? event.target.closest('[data-umami-event]') : null;
    if (!target) return;
    const prefix = 'data-umami-event-';
    const data = Object.fromEntries([...target.attributes]
      .filter(({ name }) => name.startsWith(prefix))
      .map(({ name, value }) => [name.slice(prefix.length), value]));
    window.umami.track(target.getAttribute('data-umami-event'), Object.keys(data).length ? data : undefined);
  });
`;

export function isAnalyticsRequest(url: string, baseURL: string) {
  return analyticsEnabled && url === new URL(scriptUrl!, baseURL).href;
}

export const test = base.extend({
  page: async ({ page, baseURL }, use) => {
    const external: string[] = [];
    await page.route('**/*', (route) => {
      const url = route.request().url();
      if (isAnalyticsRequest(url, baseURL!)) return route.fulfill({ contentType: 'text/javascript', body: ANALYTICS_STUB });
      if (new URL(url).origin === new URL(baseURL!).origin) return route.continue();
      external.push(url);
      return route.abort();
    });
    await use(page);
    expect(external).toEqual([]);
  },
});
