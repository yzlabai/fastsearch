/**
 * LangChain.js DocumentLoader over the docparse client.
 *
 *     import { DocparseLoader } from 'docparse-client/langchain';
 *     const docs = await new DocparseLoader('paper.pdf').load();
 *     docs[0].pageContent;  // chunk text
 *     docs[0].metadata;     // { source, page, bbox, heading_path, kind }
 *
 * Each docparse chunk becomes one LangChain `Document`; `page` + `bbox` make
 * every answer traceable back to a highlightable region of the source — the
 * metadata RAG citations need and most loaders drop.
 *
 * `@langchain/core` is a lazily-imported optional peer, so the base package
 * stays dependency-free.
 */

import { DocparseClient, DocparseHttpClient } from './index.js';
import type { EnhanceOptions } from './types.js';

export interface DocparseLoaderOptions extends EnhanceOptions {
  /** Subprocess transport: explicit binary path (else $DOCPARSE_BIN / PATH). */
  binary?: string;
  /** REST transport: base URL of a running `docparse serve`. Wins over binary. */
  url?: string;
}

export class DocparseLoader {
  private readonly client: DocparseClient | DocparseHttpClient;
  private readonly enhance: EnhanceOptions;

  constructor(
    private readonly filePath: string,
    opts: DocparseLoaderOptions = {},
  ) {
    const { binary, url, ...enhance } = opts;
    this.enhance = enhance;
    this.client = url
      ? new DocparseHttpClient({ baseUrl: url })
      : new DocparseClient({ binary });
  }

  async load(): Promise<unknown[]> {
    // Optional peer: the `as string` specifier keeps TS from resolving it at
    // build time (not installed in the base package).
    let Document: new (fields: { pageContent: string; metadata: Record<string, unknown> }) => unknown;
    try {
      ({ Document } = (await import('@langchain/core/documents' as string)) as {
        Document: typeof Document;
      });
    } catch {
      throw new Error('@langchain/core is required: npm install @langchain/core');
    }
    const chunks = await this.client.chunks(this.filePath, this.enhance);
    return chunks.map(
      (chunk) =>
        new Document({
          pageContent: chunk.text,
          metadata: {
            source: this.filePath,
            page: chunk.page,
            bbox: chunk.bbox,
            heading_path: chunk.heading_path ?? [],
            section_id: chunk.section_id,
            kind: chunk.kind,
          },
        }),
    );
  }
}
