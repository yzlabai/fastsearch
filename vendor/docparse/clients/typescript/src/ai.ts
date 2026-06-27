/**
 * Vercel AI SDK tools over the docparse client — hand them to `generateText` /
 * `streamText` so a model can parse, chunk, and navigate local documents.
 *
 *     import { docparseTools } from 'docparse-client/ai';
 *     const result = await generateText({
 *       model,
 *       tools: docparseTools(),
 *       prompt: 'Summarize report.pdf with page citations.',
 *     });
 *
 * `ai` and `zod` are lazily-imported optional peers (AI SDK v3/v4 `parameters`
 * style), so the base package stays dependency-free. Pass a configured client
 * to control the transport (subprocess vs a running `docparse serve`).
 */

import { DocparseClient, DocparseHttpClient } from './index.js';

type AnyClient = DocparseClient | DocparseHttpClient;

/**
 * Build the docparse tool set. Returns an object you can spread into the AI
 * SDK's `tools` field: `get_chunks` (RAG chunks with citations), `outline`
 * (structure tree), and `parse_markdown` (whole document as Markdown).
 */
export async function docparseTools(client: AnyClient = new DocparseClient()) {
  // Optional peers: the `as string` specifier keeps TS from resolving them at
  // build time (they're not installed in the base package).
  let tool: (def: unknown) => unknown;
  let z: any;
  try {
    ({ tool } = (await import('ai' as string)) as { tool: (def: unknown) => unknown });
    ({ z } = (await import('zod' as string)) as { z: unknown });
  } catch {
    throw new Error('the `ai` and `zod` packages are required: npm install ai zod');
  }

  const pathArg = z.object({
    path: z.string().describe('Local file path to the document'),
    ocr: z.boolean().optional().describe('OCR scanned pages (default false)'),
  });

  return {
    get_chunks: tool({
      description:
        'Parse a local document into retrieval chunks. Each chunk has text, ' +
        'page, bbox (citable), heading_path, and section_id.',
      parameters: pathArg,
      execute: async ({ path, ocr }: { path: string; ocr?: boolean }) =>
        client.chunks(path, { ocr }),
    }),
    outline: tool({
      description:
        'Parse a local document into its structure tree (nested sections with ' +
        'title/level/page/bbox). Use it to navigate long documents.',
      parameters: pathArg,
      execute: async ({ path, ocr }: { path: string; ocr?: boolean }) =>
        client.outline(path, { ocr }),
    }),
    parse_markdown: tool({
      description: 'Parse a local document into Markdown (reading order, headings, tables).',
      parameters: pathArg,
      execute: async ({ path, ocr }: { path: string; ocr?: boolean }) =>
        client.parse(path, { format: 'markdown', ocr }),
    }),
  };
}
