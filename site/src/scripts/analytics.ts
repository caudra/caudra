const VISIBILITY_THRESHOLD = 0.3;
const CONFIG_DOWNLOAD_EXTENSION = /\.(?:toml|json|ya?ml)$/i;
const tracker = document.querySelector<HTMLScriptElement>('script[data-umami-script]');

if (tracker) {
  const docsPage = location.pathname.startsWith('/docs/');
  const siteOrigin = new URL(document.querySelector<HTMLMetaElement>('meta[property="og:url"]')?.content ?? location.href).origin;
  for (const link of document.querySelectorAll<HTMLAnchorElement>('a[href]')) {
    if (!URL.canParse(link.href)) continue;
    const url = new URL(link.href);
    if (!['http:', 'https:'].includes(url.protocol)) continue;
    if ([location.origin, siteOrigin].includes(url.origin)) {
      if (docsPage && link.closest('.sl-markdown-content') && (link.hasAttribute('download') || CONFIG_DOWNLOAD_EXTENSION.test(url.pathname))) {
        link.dataset.umamiEvent ??= 'docs:download-click';
        link.dataset.umamiEventPage = location.pathname;
        link.dataset.umamiEventDestination = url.pathname;
      }
      continue;
    }
    link.dataset.umamiEvent ??= `link:${url.hostname.replace(/^www\./, '')}`;
    link.dataset.umamiEventDestination = `${url.origin}${url.pathname}`;
  }

  if (docsPage) {
    document.querySelectorAll<HTMLElement>('.sl-markdown-content .expressive-code .copy button').forEach((button, index) => {
      button.dataset.umamiEvent ??= 'docs:copy-code-click';
      button.dataset.umamiEventPage = location.pathname;
      button.dataset.umamiEventBlock = String(index + 1);
    });
  }

  const trackSections = () => {
    const umami = window.umami;
    if (!umami) return;
    if (docsPage) {
      const headings = new Set([...document.querySelectorAll('.sl-markdown-content :is(h1, h2, h3, h4, h5, h6)[id]')].map((heading) => heading.id));
      const visited = new Set<string>();
      const trackFragment = () => {
        let section: string;
        try {
          section = decodeURIComponent(location.hash.slice(1));
        } catch {
          return;
        }
        if (!headings.has(section) || visited.has(section)) return;
        umami.track('docs:section', { page: location.pathname, section });
        visited.add(section);
      };
      window.addEventListener('hashchange', trackFragment);
      trackFragment();
      return;
    }
    if (location.pathname !== '/') return;
    const sections = new Map<Element, string>();
    const observer = new IntersectionObserver((entries) => {
      for (const entry of entries) {
        const id = sections.get(entry.target);
        if (!id || !entry.isIntersecting || entry.intersectionRatio < VISIBILITY_THRESHOLD) continue;
        umami.track(`section:${id}`);
        sections.delete(entry.target);
        observer.unobserve(entry.target);
      }
    }, { threshold: VISIBILITY_THRESHOLD });
    for (const section of document.querySelectorAll<HTMLElement>('main > section[id]')) {
      const target = section.getBoundingClientRect().height > innerHeight
        ? section.querySelector('h1, h2') ?? section
        : section;
      sections.set(target, section.id);
      observer.observe(target);
    }
  };

  if (window.umami) trackSections();
  else tracker.addEventListener('load', trackSections, { once: true });
}
