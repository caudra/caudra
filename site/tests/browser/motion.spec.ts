import { expect, test, type Page } from './fixtures';
import { nameStory } from '../../src/data/home';

const DEPTH_PREFIX = 'depth-';
const ANIMATED_PROPERTIES = new Set(['transform', 'translate', 'scale', 'rotate', 'opacity', 'clipPath', 'color']);
const KEYFRAME_META = new Set(['offset', 'computedOffset', 'easing', 'composite']);
const SCROLL_TIMELINES = new Set(['ScrollTimeline', 'ViewTimeline']);
const VIEWPORT_WIDTHS = [320, 390, 768, 1024, 1280, 1600];
const DESKTOP = { width: 1280, height: 900 };
const PHONE = { width: 390, height: 844 };
const PIN_STEP = 200;
const IDENTITY = new Set(['none', 'matrix(1, 0, 0, 1, 0, 0)']);

interface DepthAnimation { name: string; timeline: string; properties: string[] }

function depthAnimations(page: Page) {
  return page.evaluate((prefix) => document.getAnimations()
    .filter((animation): animation is CSSAnimation => animation instanceof CSSAnimation && animation.animationName.startsWith(prefix))
    .map((animation) => ({
      name: animation.animationName,
      timeline: animation.timeline?.constructor.name ?? '',
      properties: (animation.effect as KeyframeEffect).getKeyframes().flatMap((keyframe) => Object.keys(keyframe)),
    })), DEPTH_PREFIX);
}

function scrollTimelinesSupported(page: Page) {
  return page.evaluate(() => CSS.supports('animation-timeline: view()'));
}

async function scrollToSettled(page: Page, top: number) {
  await page.evaluate((y) => new Promise<void>((resolve) => {
    window.scrollTo({ top: y, behavior: 'instant' });
    requestAnimationFrame(() => requestAnimationFrame(() => resolve()));
  }), top);
}

test('homepage depth uses scroll timelines and compositor-friendly properties only', async ({ page }) => {
  await page.setViewportSize(DESKTOP);
  await page.goto('/');
  test.skip(!(await scrollTimelinesSupported(page)), 'scroll-driven animations unsupported');
  const animations: DepthAnimation[] = await depthAnimations(page);
  expect(animations.length).toBeGreaterThan(0);
  for (const animation of animations) {
    expect(SCROLL_TIMELINES.has(animation.timeline), `${animation.name} timeline ${animation.timeline}`).toBe(true);
    for (const property of animation.properties) {
      if (!KEYFRAME_META.has(property)) expect(ANIMATED_PROPERTIES.has(property), `${animation.name} animates ${property}`).toBe(true);
    }
  }
});

test('reduced motion removes every homepage depth animation', async ({ page }) => {
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await page.goto('/');
  expect(await depthAnimations(page)).toEqual([]);
});

test('homepage never overflows sideways while depth layers move', async ({ page }) => {
  await page.emulateMedia({ reducedMotion: 'no-preference' });
  await page.goto('/');
  for (const width of VIEWPORT_WIDTHS) {
    await page.setViewportSize({ width, height: DESKTOP.height });
    const overflowAt = await page.evaluate(async () => {
      const root = document.documentElement;
      const frame = () => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
      const offsets: number[] = [];
      for (let top = 0; top < root.scrollHeight; top += innerHeight / 2) {
        window.scrollTo({ top, behavior: 'instant' });
        await frame();
        if (root.scrollWidth > innerWidth) offsets.push(top);
      }
      return offsets;
    });
    expect(overflowAt, `horizontal overflow at ${width}px`).toEqual([]);
  }
});

test('the name loop pins on wide screens and lights its steps in order', async ({ page }, testInfo) => {
  test.skip(testInfo.project.name.startsWith('mobile'), 'pinning is desktop only');
  await page.setViewportSize(DESKTOP);
  await page.goto('/');
  test.skip(!(await scrollTimelinesSupported(page)), 'scroll-driven animations unsupported');
  const loop = page.locator('.name-loop');
  const gridTop = await page.locator('.name-grid').evaluate((node) => node.getBoundingClientRect().top + scrollY);
  await scrollToSettled(page, gridTop);
  const pinnedTop = await loop.evaluate((node) => node.getBoundingClientRect().top);
  const mutedColor = await loop.locator('figcaption').evaluate((node) => getComputedStyle(node).color);
  await expect(loop.locator('li').last()).toHaveCSS('color', mutedColor);
  await scrollToSettled(page, gridTop + PIN_STEP);
  expect(await loop.evaluate((node) => node.getBoundingClientRect().top)).toBeCloseTo(pinnedTop, 0);
  const sectionEnd = await page.locator('#name').evaluate((node) => node.getBoundingClientRect().bottom + scrollY - innerHeight);
  await scrollToSettled(page, sectionEnd);
  const textColor = await page.locator('#name').evaluate((node) => getComputedStyle(node).color);
  await expect.poll(() => loop.locator('li').evaluateAll((items) => items.map((item) => getComputedStyle(item).color))).toEqual(nameStory.loop.map(() => textColor));
});

test('phones keep the name loop in the page flow', async ({ page }) => {
  await page.setViewportSize(PHONE);
  await page.goto('/');
  await expect(page.locator('.name-loop')).toHaveCSS('position', 'static');
});

test('depth layers are decorative and leave forced colors alone', async ({ page }) => {
  await page.goto('/');
  const layers = page.locator('.story-numeral, .hero-mark');
  expect(await layers.count()).toBeGreaterThan(1);
  for (const layer of await layers.all()) await expect(layer).toHaveAttribute('aria-hidden', 'true');
  await page.emulateMedia({ forcedColors: 'active' });
  for (const layer of await layers.all()) await expect(layer).toHaveCSS('display', 'none');
});

test('the headline is at rest and fully opaque at the top of the page', async ({ page }) => {
  await page.goto('/');
  await scrollToSettled(page, 0);
  const headline = page.getByRole('heading', { level: 1 });
  await expect(headline).toHaveCSS('opacity', '1');
  expect(IDENTITY.has(await headline.evaluate((node) => getComputedStyle(node).transform))).toBe(true);
});
