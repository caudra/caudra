import { describe, expect, test } from 'bun:test';
import { createMarkdownProcessor } from '@astrojs/markdown-remark';
import { load } from 'cheerio';
import { estimatedTokens, pageMarkdown, parsePage, readDocs, validateNavigation } from '../../src/data/docs';
import compatibility, { slugify } from '../../src/markdown/compatibility';
import diagrams from '../../src/markdown/diagrams';

const renderer = await createMarkdownProcessor({ smartypants: false, remarkPlugins: [compatibility, diagrams], syntaxHighlight: false });

describe('portable documentation contract', () => {
  test('every real page has metadata and appears in navigation exactly once', async () => {
    const { pages, navigation } = await readDocs();
    expect(pages.length).toBeGreaterThan(30);
    expect(navigation.groups.flatMap((group) => group.pages)).toEqual(pages.slice(1).map((page) => page.slug));
    for (const page of pages) {
      const { code } = await renderer.render(pageMarkdown(page));
      expect(load(code)('h1').map((_, element) => load(code)(element).text()).get()).toEqual([page.title]);
    }
  });

  test('rejects incomplete metadata and duplicate body titles', () => {
    expect(() => parsePage('---\ntitle: Test\n---\nBody', 'test')).toThrow('test:');
    expect(() => parsePage('---\ntitle: Test\ndescription: Guide\n---\n# Test', 'test')).toThrow('body H1');
  });

  test('rejects missing, unknown, duplicate and unlisted pages', async () => {
    const { pages } = await readDocs();
    expect(() => validateNavigation({ groups: [{ label: 'Guides', pages: ['unknown'] }] }, pages)).toThrow('unknown');
    expect(() => validateNavigation({ groups: [{ label: 'Guides', pages: ['quick-start', 'quick-start'] }] }, pages)).toThrow('repeated');
    expect(() => validateNavigation({ groups: [{ label: 'Guides', pages: ['quick-start'] }] }, pages)).toThrow('missing from navigation');
    expect(() => validateNavigation({ groups: [{ label: 'Guides', pages: ['quick-start'] }] }, pages.slice(1))).toThrow('overview');
  });

  test('estimates Unicode characters instead of UTF-16 units', () => {
    expect(estimatedTokens('a𝛂bc')).toBe(1);
    expect(estimatedTokens('abcde')).toBe(2);
  });
});

describe('native section addresses in real HTML', () => {
  test('ASCII slug rules preserve the native punctuation contract', () => {
    expect(slugify('`file_read` & API (v2)')).toBe('file-read-api-v2');
    expect(slugify('Ä → café')).toBe('caf');
    expect(slugify('İ K ABC')).toBe('abc');
    expect(slugify('foo___bar')).toBe('foo-bar');
  });

  test('explicit IDs, repeats and badges survive Markdown rendering', async () => {
    const { code, metadata } = await renderer.render('## `file_read` {#file_read}\n\n## A_B!\n\n## A B\n\n## A B {#A_B}\n\n## A B\n\n## `tool` <span class="badge">on demand</span>\n\n```md\n## ignored {#ignored}\n```');
    const $ = load(code);
    expect($('h2').map((_, node) => $(node).attr('id')).get()).toEqual(['file_read', 'a-b', 'a-b-1', 'A_B', 'a-b-2', 'tool-on-demand']);
    expect($('h2').text()).not.toContain('{#');
    expect(metadata.headings.map((heading) => heading.slug)).toEqual(['file_read', 'a-b', 'a-b-1', 'A_B', 'a-b-2', 'tool-on-demand']);
  });

  test('reserves the synthesized title address', async () => {
    const { code } = await renderer.render('## Example\n\n## Example', { frontmatter: { title: 'Example' } });
    expect(load(code)('h2').map((_, node) => load(code)(node).attr('id')).get()).toEqual(['example-1', 'example-2']);
  });

  test('only actual Mermaid fences become diagrams', async () => {
    const { code } = await renderer.render('```mermaid\nflowchart LR\nA --> B\n```\n\n````markdown\n```mermaid\nflowchart LR\nA --> B\n```\n````');
    const $ = load(code);
    expect($('.diagram').length).toBe(1);
    expect($('.diagram-source').text()).toContain('A --> B');
    expect($('code').last().text()).toContain('```mermaid');
  });
});
