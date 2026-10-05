import { expect, test } from 'bun:test';
import { readFile, readdir, stat } from 'node:fs/promises';
import { gzipSync } from 'node:zlib';
import { load, type CheerioAPI } from 'cheerio';
import { createMarkdownProcessor } from '@astrojs/markdown-remark';
import { SITE, estimatedTokens, pageMarkdown, pagePath, readDocs } from '../../src/data/docs';
import compatibility from '../../src/markdown/compatibility';
import { CLAUDE_NOTE, GROK_BUILD, HEADLINE, HERO_EYEBROW, METHOD_NOTE, NAME_SOURCES, NINFER, capabilityGroups, heroClip, inspirations, metrics, stories } from '../../src/data/home';
import { INSTALL_COMMAND } from '../../src/data/install';

const ILLUSTRATION = 'Illustration';
const EXPERIMENTAL = 'Experimental';
const MAINTAINER_ATTRIBUTION = 'Measured by the maintainer in daily use';
const MIN_CAPABILITIES = 40;
const FOUNDER_FIGURES = /20 ?(?:B\b|billion)|9[47]%|under a second|<1 s|[12] GB|10k-turn/;
const OTHER_AGENTS = /OpenCode|Claude Code|Codex|Cursor|Aider|Gemini CLI|Grok Build/i;
const BRAIN_REFERENCES = /\bbrain\b|caudate/i;
const NAME_STORY = 'name';
const dist = new URL('../../dist/', import.meta.url);
const { pages, navigation } = await readDocs();
const renderer = await createMarkdownProcessor({ smartypants: false, remarkPlugins: [compatibility], syntaxHighlight: false });
const documents = new Map(await Promise.all(['/', '/404.html', ...pages.map((page) => pagePath(page.slug))].map(async (path) => {
  const file = path.endsWith('/') ? `${path}index.html` : path;
  return [path, load(await readFile(new URL(`.${file}`, dist), 'utf8'))] as const;
})));

function homeText($: CheerioAPI, selector: string) {
  return $(selector).text().replace(/\s+/g, ' ').trim();
}

function nativeAddresses(markdown: string) {
  const addresses: string[] = [];
  let fence: { marker: string; length: number } | undefined;
  for (const line of markdown.split('\n')) {
    const marker = /^\s*(`{3,}|~{3,})(.*)$/.exec(line);
    if (fence) {
      if (marker && marker[1][0] === fence.marker && marker[1].length >= fence.length && !marker[2].trim()) fence = undefined;
      continue;
    }
    if (marker) { fence = { marker: marker[1][0], length: marker[1].length }; continue; }
    const heading = /^#{1,6}(?:[ \t]+(.*)|$)/.exec(line);
    if (!heading) continue;
    const text = heading[1]?.trim() ?? '';
    const explicit = /\{#([^}]*)\}$/.exec(text);
    if (explicit) { addresses.push(explicit[1]); continue; }
    const plain = text.replace(/`+([^`]+)`+/g, '$1').replace(/!?\[([^\]]*)\]\([^)]*\)/g, '$1').replace(/<br\s*\/?\s*>/gi, ' ').replace(/<[^>]+>/g, '').replaceAll('**', '').replace(/\\([\x21-\x2f\x3a-\x40\x5b-\x60\x7b-\x7e])/g, '$1');
    const base = plain.replace(/[^A-Za-z0-9]+/g, '-').replace(/^-|-$/g, '').toLowerCase();
    let address = base;
    for (let suffix = 1; addresses.includes(address); suffix++) address = `${base}-${suffix}`;
    addresses.push(address);
  }
  return addresses;
}

test('every canonical document has one title, description, canonical, source copy and native section addresses', async () => {
  for (const page of pages) {
    const path = pagePath(page.slug);
    const $ = documents.get(path)!;
    expect($('h1').map((_, node) => $(node).text()).get()).toEqual([page.title]);
    expect($('meta[name="description"]').attr('content')).toBe(page.description);
    expect($('link[rel="canonical"]').attr('href')).toBe(`${SITE}${path}`);
    expect($('[data-copy-markdown]').attr('data-copy-markdown')).toBe(`${path}index.md`);
    const markdown = await readFile(new URL(`.${path}index.md`, dist), 'utf8');
    expect(markdown).toBe(pageMarkdown(page));
    expect($('.page-actions').text()).toContain(`~${estimatedTokens(markdown).toLocaleString('en-US')} tokens`);
    const { metadata } = await renderer.render(markdown);
    expect(metadata.headings.map(({ slug }) => slug), `${path}: native address contract`).toEqual(nativeAddresses(markdown));
    const actual = new Set($('[id]').map((_, node) => $(node).attr('id')).get());
    for (const heading of metadata.headings) expect(actual.has(heading.slug), `${path}#${heading.slug}`).toBe(true);
    expect($('.sl-markdown-content :is(h2,h3,h4,h5,h6)').text()).not.toContain('{#');
    const ids = $('[id]').map((_, node) => $(node).attr('id')).get();
    expect(ids.length, `${path}: duplicate HTML IDs`).toBe(new Set(ids).size);
  }
});

test('all internal HTML, asset, download and fragment links resolve', async () => {
  const checked = new Set<string>();
  for (const [path, $] of documents) {
    for (const element of $('[href], [src]').toArray()) {
      const destination = $(element).attr('href') ?? $(element).attr('src')!;
      const url = new URL(destination, `${SITE}${path}`);
      if (url.origin !== SITE || checked.has(url.href)) continue;
      checked.add(url.href);
      const target = documents.get(url.pathname);
      if (url.hash && target) {
        expect(target('[id]').toArray().some((node) => target(node).attr('id') === decodeURIComponent(url.hash.slice(1))), `${path} → ${url.pathname}${url.hash}`).toBe(true);
      }
      const file = url.pathname.endsWith('/') ? `${url.pathname}index.html` : url.pathname;
      expect(await stat(new URL(`.${file}`, dist)).then((s) => s.isFile()).catch(() => false), `${path} → ${file}`).toBe(true);
    }
  }
});

test('navigation and LLM exports derive from the full canonical corpus', async () => {
  const $ = documents.get('/docs/')!;
  expect($('.docs-directory li a').map((_, node) => $(node).attr('href')).get()).toEqual(navigation.groups.flatMap((group) => group.pages.map(pagePath)));
  const index = await readFile(new URL('llms.txt', dist), 'utf8');
  const full = await readFile(new URL('llms-full.txt', dist), 'utf8');
  expect(full).toBe(pages.map(pageMarkdown).join('\n---\n\n'));
  for (const page of pages) expect(index).toContain(`${SITE}${pagePath(page.slug)}index.md`);
  expect(await readFile(new URL('docs/llms.txt', dist), 'utf8')).toBe(index);
  expect(await readFile(new URL('docs/llms-full.txt', dist), 'utf8')).toBe(full);
});

test('installers and examples are byte-for-byte copies, with search and SEO outputs present', async () => {
  for (const name of ['install.sh', 'install.ps1']) {
    expect(await readFile(new URL(name, dist))).toEqual(await readFile(new URL(`../../../${name}`, import.meta.url)));
  }
  const examples = (await readdir(new URL('../../public/docs/', import.meta.url))).filter((name) => name.endsWith('.example.toml'));
  expect(examples.length).toBeGreaterThan(0);
  for (const name of examples) expect(await readFile(new URL(`docs/${name}`, dist))).toEqual(await readFile(new URL(`../../public/docs/${name}`, import.meta.url)));
  for (const name of ['pagefind/pagefind.js', 'sitemap-index.xml', 'sitemap-0.xml', 'robots.txt', 'site.webmanifest', 'social-card.png', '404.html']) {
    expect((await stat(new URL(name, dist))).size, name).toBeGreaterThan(0);
  }
  const sitemap = await readFile(new URL('sitemap-0.xml', dist), 'utf8');
  for (const page of pages) expect(sitemap).toContain(`${SITE}${pagePath(page.slug)}`);
  expect(sitemap).not.toContain('/404');
});

test('runtime assets are self-hosted and source art is not deployed', async () => {
  for (const [path, $] of documents) {
    for (const node of $('script[src], img[src], link[rel="stylesheet"], link[rel="preload"], link[rel="modulepreload"]').toArray()) {
      const source = $(node).attr('src') ?? $(node).attr('href')!;
      expect(new URL(source, SITE).origin, `${path}: ${source}`).toBe(SITE);
    }
  }
  expect(await stat(new URL('caudra-hero-v1.png', dist)).then(() => true).catch(() => false)).toBe(false);
  expect(await stat(new URL('docs/search.json', dist)).then(() => true).catch(() => false)).toBe(false);
});

test('homepage founder figures are attributed and explained by the method note', () => {
  const $ = documents.get('/')!;
  expect($('.proof-list strong').map((_, node) => $(node).text()).get()).toEqual(metrics.map((metric) => metric.value));
  expect(homeText($, '.proof')).toContain(MAINTAINER_ATTRIBUTION);
  expect($('.proof a[href="#method"]').length).toBe(1);
  expect(homeText($, '#method')).toContain(METHOD_NOTE);
  for (const section of $('main section').toArray()) {
    const text = $(section).text().replace(/\s+/g, ' ');
    if (FOUNDER_FIGURES.test(text)) expect(text, `#${$(section).attr('id')}: unattributed founder figure`).toContain('maintainer');
  }
  expect(homeText($, '#why')).not.toMatch(OTHER_AGENTS);
});

test('homepage keeps brain references in the cited name story', () => {
  const $ = documents.get('/')!;
  for (const section of $('main section').toArray()) {
    if (BRAIN_REFERENCES.test($(section).text())) expect($(section).attr('id')).toBe(NAME_STORY);
  }
  for (const source of NAME_SOURCES) expect($(`#${NAME_STORY} a[href="${source.href}"]`).text()).toBe(source.name);
});

test('homepage credits ideas, labels experiments, and makes no automation claims', () => {
  const $ = documents.get('/')!;
  expect(homeText($, '.claude-note')).toBe(CLAUDE_NOTE);
  for (const credit of inspirations) expect($(`#why a[href="${credit.href}"]`).text()).toBe(credit.name);
  expect($(`#lab a[href="${GROK_BUILD.href}"]`).text()).toBe(GROK_BUILD.name);
  expect($(`#privacy a[href="${NINFER.href}"]`).text()).toBe(NINFER.name);
  const items = capabilityGroups.flatMap((group) => group.items);
  expect(items.length).toBeGreaterThanOrEqual(MIN_CAPABILITIES);
  expect($('.capability-wall a').map((_, node) => $(node).attr('href')).get()).toEqual(items.map((item) => item.href));
  for (const item of items) expect($(`.capability-wall a[href="${item.href}"] .tag`).text()).toBe(item.experimental ? EXPERIMENTAL : '');
  for (const node of $('.experiment-list li, .sandbox').toArray()) expect($(node).find('.tag').first().text()).toBe(EXPERIMENTAL);
  expect(homeText($, '#lab')).toContain('off by default');
  expect(homeText($, '#savings')).toContain('RTK-style shell output filtering');
  expect(homeText($, '#lab')).toContain('JEV decision engine');
  expect(homeText($, '#sleep')).toContain('needs a running session');
  expect(homeText($, 'main')).not.toMatch(/\bautomations?\b|\bschedul|\bcron\b/i);
});

test('homepage has an honest product story, working destinations, and a small entry script', async () => {
  const $ = documents.get('/')!;
  expect($('h1').length).toBe(1);
  expect(homeText($, 'h1')).toBe(HEADLINE);
  expect(homeText($, '.hero-eyebrow')).toBe(HERO_EYEBROW);
  expect($('[data-copy-source]').map((_, node) => $(node).text()).get()).toEqual([INSTALL_COMMAND, INSTALL_COMMAND]);
  expect($('.hero #hero-install').length).toBe(1);
  expect($('.recording').length).toBe([heroClip, ...stories.flatMap((story) => story.clips)].length);
  expect(new Set($('.recording-label').map((_, node) => $(node).text()).get())).toEqual(new Set([ILLUSTRATION]));
  expect($('iframe').length).toBe(0);
  const ids = $('[id]').map((_, node) => $(node).attr('id')).get();
  expect(new Set(ids).size).toBe(ids.length);
  let initialBytes = 0;
  for (const script of $('script[src]').toArray()) {
    const path = $(script).attr('src')!;
    initialBytes += gzipSync(await readFile(new URL(`.${path}`, dist))).length;
  }
  expect(initialBytes).toBeLessThan(25 * 1024);
});
