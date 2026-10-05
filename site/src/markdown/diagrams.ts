import type { Root, RootContent } from 'mdast';

export default function diagrams() {
  return (tree: Root) => {
    function transform(nodes: RootContent[]) {
      for (let index = 0; index < nodes.length; index++) {
        const node = nodes[index];
        if (node.type === 'code' && node.lang === 'mermaid') {
          const source = node.value.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
          nodes[index] = { type: 'html', value: `<figure class="diagram"><div class="diagram-render" aria-label="Flow diagram"></div><details open><summary>Diagram source</summary><pre class="diagram-source"><code>${source}</code></pre></details></figure>` };
        } else if ('children' in node) transform(node.children as RootContent[]);
      }
    }
    transform(tree.children);
  };
}
