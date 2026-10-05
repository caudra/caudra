import { readdir, readFile } from 'node:fs/promises';
import { z } from 'astro/zod';
import { parse } from 'yaml';

export const SITE = 'https://caudra.ai';
export const docsDirectory = new URL('../content/docs/', import.meta.url);
export const pageMetadata = z.object({ title: z.string().min(1), description: z.string().min(1) }).strict();
const navigationSchema = z.object({
  groups: z.array(z.object({ label: z.string().min(1), pages: z.array(z.string().regex(/^[a-z0-9-]+$/)).min(1) }).strict()).min(1),
}).strict();

export interface DocPage {
  slug: string;
  title: string;
  description: string;
  body: string;
}

export function parsePage(source: string, slug: string): DocPage {
  const match = /^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)/.exec(source);
  if (!match) throw new Error(`${slug}: expected YAML title and description`);
  const result = pageMetadata.safeParse(parse(match[1]));
  if (!result.success) throw new Error(`${slug}: ${result.error.message}`);
  const body = source.slice(match[0].length).trim();
  if (!body) throw new Error(`${slug}: empty documentation body`);
  if (/^#\s/.test(body)) throw new Error(`${slug}: the title belongs in frontmatter, not a body H1`);
  return { slug, ...result.data, body };
}

export function validateNavigation(input: unknown, pages: DocPage[]) {
  const navigation = navigationSchema.parse(input);
  const remaining = new Set(pages.map(({ slug }) => slug));
  if (!remaining.delete('index')) throw new Error('Missing docs/index.md overview');
  const labels = new Set<string>();
  for (const group of navigation.groups) {
    if (labels.has(group.label)) throw new Error(`Duplicate documentation group: ${group.label}`);
    labels.add(group.label);
    for (const slug of group.pages) {
      if (!remaining.delete(slug)) throw new Error(`${group.label}: unknown or repeated documentation page ${slug}`);
    }
  }
  if (remaining.size) throw new Error(`Documentation pages missing from navigation: ${[...remaining].join(', ')}`);
  return navigation;
}

export async function readDocs() {
  const files = (await readdir(docsDirectory)).sort();
  if (files.some((name) => !/^[a-z0-9-]+\.md$/.test(name))) throw new Error('Documentation must be flat portable .md files');
  const pages = await Promise.all(files.map(async (file) => parsePage(await readFile(new URL(file, docsDirectory), 'utf8'), file.slice(0, -3))));
  const navigation = validateNavigation(JSON.parse(await readFile(new URL('./docs-navigation.json', import.meta.url), 'utf8')), pages);
  const bySlug = new Map(pages.map((page) => [page.slug, page]));
  const ordered = ['index', ...navigation.groups.flatMap(({ pages }) => pages)].map((slug) => bySlug.get(slug)!);
  return { pages: ordered, navigation, bySlug };
}

export function pagePath(slug: string) {
  return slug === 'index' ? '/docs/' : `/docs/${slug}/`;
}

export function pageMarkdown(page: DocPage) {
  return `# ${page.title}\n\n${page.body}\n`;
}

export function estimatedTokens(markdown: string) {
  return Math.ceil([...markdown].length / 4);
}
