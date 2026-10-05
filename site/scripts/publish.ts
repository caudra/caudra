import { copyFile, mkdir, writeFile } from 'node:fs/promises';
import type { AstroIntegration } from 'astro';
import { SITE, pageMarkdown, pagePath, readDocs } from '../src/data/docs';

export default function publish(): AstroIntegration {
  return {
    name: 'caudra-publish',
    hooks: {
      'astro:build:done': async ({ dir }) => {
        const { pages, navigation, bySlug } = await readDocs();
        for (const page of pages) {
          const directory = new URL(`.${pagePath(page.slug)}`, dir);
          await mkdir(directory, { recursive: true });
          await writeFile(new URL('index.md', directory), pageMarkdown(page));
        }
        const index = [
          '# Caudra', '', `> ${pages[0].description}`, '',
          `Full documentation: ${SITE}/llms-full.txt`, '',
          `- [Overview](${SITE}/docs/index.md): ${pages[0].description}`,
          ...navigation.groups.flatMap((group) => ['', `## ${group.label}`, '', ...group.pages.map((slug) => {
            const page = bySlug.get(slug)!;
            return `- [${page.title}](${SITE}${pagePath(slug)}index.md): ${page.description}`;
          })]), '',
        ].join('\n');
        const full = pages.map(pageMarkdown).join('\n---\n\n');
        for (const prefix of ['', 'docs/']) {
          await writeFile(new URL(`${prefix}llms.txt`, dir), index);
          await writeFile(new URL(`${prefix}llms-full.txt`, dir), full);
        }
        for (const name of ['install.sh', 'install.ps1']) {
          await copyFile(new URL(`../../${name}`, import.meta.url), new URL(name, dir));
        }
      },
    },
  };
}
