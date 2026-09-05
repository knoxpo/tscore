import { CodeBlock, Pre } from 'fumadocs-ui/components/codeblock';
import { renderMermaidSVG } from 'beautiful-mermaid';

// Server-rendered at build time: no client bundle, theme via fumadocs CSS vars.
export async function Mermaid({ chart }: { chart: string }) {
  try {
    const svg = renderMermaidSVG(chart, {
      bg: 'var(--color-fd-background)',
      fg: 'var(--color-fd-foreground)',
      interactive: true,
      transparent: true,
    });
    return <div className="my-6 overflow-x-auto [&>svg]:mx-auto [&>svg]:h-auto [&>svg]:w-auto [&>svg]:max-w-none [&>svg]:max-h-[500px]" dangerouslySetInnerHTML={{ __html: svg }} />;
  } catch {
    return (
      <CodeBlock title="Mermaid (render failed)">
        <Pre>{chart}</Pre>
      </CodeBlock>
    );
  }
}
