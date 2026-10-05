import type { Nodes, Root } from 'mdast';

export function slugify(text: string) {
  return text.replace(/[^A-Za-z0-9]+/g, '-').toLowerCase().replace(/^-|-$/g, '');
}

function visibleText(node: Nodes): string {
  if (node.type === 'html') return /^<br\s*\/?\s*>$/i.test(node.value) ? ' ' : '';
  if ('children' in node) return node.children.map(visibleText).join('');
  return 'value' in node ? node.value : node.type === 'image' ? node.alt || '' : '';
}

export default function compatibility() {
  return (tree: Root, file: { data: { astro?: { frontmatter?: { title?: string } } } }) => {
    const title = file.data.astro?.frontmatter?.title;
    const taken = new Set(title ? [slugify(title)] : []);
    function walk(node: Nodes) {
      if (node.type === 'heading') {
        const last = node.children.at(-1);
        const explicit = last?.type === 'text' ? /\s*\{#([^}]*)\}$/.exec(last.value) : null;
        if (explicit && last?.type === 'text') last.value = last.value.slice(0, -explicit[0].length);
        const base = explicit?.[1] ?? slugify(visibleText(node));
        let id = base;
        if (!explicit) for (let suffix = 1; taken.has(id); suffix++) id = `${base}-${suffix}`;
        taken.add(id);
        node.data = Object.assign(node.data ?? {}, { hProperties: { id } });
      }
      if ('children' in node) node.children.forEach(walk);
    }
    walk(tree);
  };
}
