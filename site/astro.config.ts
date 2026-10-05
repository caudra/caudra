import { defineConfig } from 'astro/config';
import { unified } from '@astrojs/markdown-remark';
import starlight from '@astrojs/starlight';
import { readDocs, SITE } from './src/data/docs';
import compatibility from './src/markdown/compatibility';
import diagrams from './src/markdown/diagrams';
import publish from './scripts/publish';
import theme from './src/styles/caudra-theme.json';

const { navigation } = await readDocs();

export default defineConfig({
  site: SITE,
  output: 'static',
  trailingSlash: 'always',
  markdown: { processor: unified({ smartypants: false, remarkPlugins: [compatibility, diagrams] }) },
  integrations: [
    starlight({
      title: 'caudra',
      description: 'Context into effective action. Documentation for the Caudra terminal coding agent.',
      logo: { src: './public/caudra-mark.svg', replacesTitle: false },
      favicon: '/caudra-mark.svg',
      disable404Route: true,
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/caudra/caudra' }],
      sidebar: [
        { label: 'Overview', slug: 'docs' },
        ...navigation.groups.map(({ label, pages }) => ({ label, items: pages.map((slug) => ({ slug: `docs/${slug}` })) })),
      ],
      customCss: ['./src/styles/brand.css', './src/styles/docs.css'],
      components: { PageTitle: './src/components/PageTitle.astro', MarkdownContent: './src/components/DocsContent.astro' },
      head: [
        { tag: 'link', attrs: { rel: 'manifest', href: '/site.webmanifest' } },
        { tag: 'link', attrs: { rel: 'apple-touch-icon', href: '/apple-touch-icon.png' } },
        { tag: 'link', attrs: { rel: 'preload', href: '/fonts/jetbrains-mono-latin.woff2', as: 'font', type: 'font/woff2', crossorigin: 'anonymous' } },
        { tag: 'meta', attrs: { property: 'og:image', content: `${SITE}/social-card.png` } },
        { tag: 'meta', attrs: { property: 'og:image:alt', content: 'Caudra turns context into effective action' } },
        { tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
        { tag: 'meta', attrs: { name: 'theme-color', content: '#09254d' } },
      ],
      expressiveCode: {
        themes: [{ ...theme, type: 'dark' }],
        shiki: { langAlias: { rhai: 'text', tmux: 'text' } },
        styleOverrides: { borderRadius: '0', codeFontFamily: 'JetBrains Mono, monospace', codeBackground: '#09254d' },
      },
    }),
    publish(),
  ],
});
