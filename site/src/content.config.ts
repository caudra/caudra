import { defineCollection } from 'astro:content';
import { z } from 'astro/zod';
import { docsLoader } from '@astrojs/starlight/loaders';
import { docsSchema } from '@astrojs/starlight/schema';

export const collections = {
  docs: defineCollection({
    loader: docsLoader({ generateId: ({ entry }) => entry === 'index.md' ? 'docs' : `docs/${entry.replace(/\.md$/, '')}` }),
    schema: docsSchema({ extend: z.object({ description: z.string().min(1) }) }),
  }),
};
