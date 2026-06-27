/**
 * Thin TypeScript client for docparse — the pure-Rust document parser.
 *
 * Two transports, zero runtime dependencies:
 *
 * - {@link DocparseClient} spawns the `docparse` binary — simplest deployment
 *   (one binary + this package);
 * - {@link DocparseHttpClient} talks to a running `docparse serve` over REST
 *   using global `fetch` — for a long-running parser shared by many callers.
 *
 * Both return the same shapes: `parse(path, { format })` decodes JSON for
 * `json`/`chunks`/`outline` and returns a string for `markdown`/`text`.
 * Framework adapters live in `docparse-client/langchain` and
 * `docparse-client/ai` (hosts imported lazily, so the base stays dep-free).
 */

import { execFile } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { basename } from 'node:path';
import { promisify } from 'node:util';
import type {
  Chunk,
  DocparseDocument,
  EnhanceOptions,
  Format,
  JsonFormat,
  Section,
  TextFormat,
} from './types.js';

export * from './types.js';

const execFileAsync = promisify(execFile);

const JSON_FORMATS: readonly Format[] = ['json', 'chunks', 'outline'];
const TEXT_FORMATS: readonly Format[] = ['markdown', 'text'];
/** docparse responses can be tens of MB; lift execFile's 1MB stdout cap. */
const MAX_BUFFER = 256 * 1024 * 1024;

/** The parser refused the input, or the transport failed. */
export class DocparseError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'DocparseError';
  }
}

function assertFormat(format: Format): void {
  if (!JSON_FORMATS.includes(format) && !TEXT_FORMATS.includes(format)) {
    throw new DocparseError(`unknown format '${format}'`);
  }
}

/** Resolve the docparse binary: explicit arg > $DOCPARSE_BIN > 'docparse' on PATH. */
function findBinary(explicit?: string): string {
  for (const cand of [explicit, process.env.DOCPARSE_BIN]) {
    if (cand && existsSync(cand)) return cand;
  }
  // Fall back to the PATH name; spawn surfaces a clear error if it's absent.
  return explicit ?? process.env.DOCPARSE_BIN ?? 'docparse';
}

/** Subprocess transport around the `docparse` CLI. */
export class DocparseClient {
  private readonly binary: string;
  private readonly timeout: number;

  constructor(opts: { binary?: string; timeoutMs?: number } = {}) {
    this.binary = findBinary(opts.binary);
    this.timeout = opts.timeoutMs ?? 300_000;
  }

  parse(path: string, opts?: { format?: 'json' } & EnhanceOptions): Promise<DocparseDocument>;
  parse(path: string, opts: { format: 'chunks' } & EnhanceOptions): Promise<Chunk[]>;
  parse(path: string, opts: { format: 'outline' } & EnhanceOptions): Promise<Section>;
  parse(path: string, opts: { format: TextFormat } & EnhanceOptions): Promise<string>;
  async parse(
    path: string,
    opts: { format?: Format } & EnhanceOptions = {},
  ): Promise<unknown> {
    const format = opts.format ?? 'json';
    assertFormat(format);
    const args = [path, '-f', format];
    if (opts.ocr) args.push('--ocr');
    if (opts.layout) args.push('--layout');
    if (typeof opts.tableModel === 'string') args.push('--table-model', opts.tableModel);
    if (typeof opts.formulaModel === 'string') args.push('--formula-model', opts.formulaModel);

    let stdout: string;
    try {
      ({ stdout } = await execFileAsync(this.binary, args, {
        timeout: this.timeout,
        maxBuffer: MAX_BUFFER,
        encoding: 'utf8',
      }));
    } catch (e) {
      const err = e as { stderr?: string; message?: string };
      throw new DocparseError((err.stderr || err.message || String(e)).trim());
    }
    return JSON_FORMATS.includes(format) ? JSON.parse(stdout) : stdout;
  }

  /** RAG chunks: text + page + bbox + heading breadcrumb + section_id. */
  chunks(path: string, opts: EnhanceOptions = {}): Promise<Chunk[]> {
    return this.parse(path, { ...opts, format: 'chunks' });
  }

  /** Document structure tree (table of contents): nested citable sections. */
  outline(path: string, opts: EnhanceOptions = {}): Promise<Section> {
    return this.parse(path, { ...opts, format: 'outline' });
  }
}

/** REST transport for a running `docparse serve` instance. */
export class DocparseHttpClient {
  private readonly baseUrl: string;
  private readonly timeout: number;

  constructor(opts: { baseUrl?: string; timeoutMs?: number } = {}) {
    this.baseUrl = (opts.baseUrl ?? 'http://127.0.0.1:8642').replace(/\/+$/, '');
    this.timeout = opts.timeoutMs ?? 300_000;
  }

  parse(path: string, opts?: { format?: 'json' } & EnhanceOptions): Promise<DocparseDocument>;
  parse(path: string, opts: { format: 'chunks' } & EnhanceOptions): Promise<Chunk[]>;
  parse(path: string, opts: { format: 'outline' } & EnhanceOptions): Promise<Section>;
  parse(path: string, opts: { format: TextFormat } & EnhanceOptions): Promise<string>;
  async parse(
    path: string,
    opts: { format?: Format } & EnhanceOptions = {},
  ): Promise<unknown> {
    const format = opts.format ?? 'json';
    assertFormat(format);

    // Boolean enhancement flags become query params; the server must be started
    // with the matching model config (--ocr-models / --unirec-models / …).
    const params = new URLSearchParams({ format });
    if (opts.ocr) params.set('ocr', 'true');
    if (opts.layout) params.set('layout', 'true');
    if (opts.tableModel) params.set('table_model', 'true');
    if (opts.formulaModel) params.set('formula_model', 'true');

    const body = new FormData();
    body.append('file', new Blob([await readFile(path)]), basename(path));

    let resp: Response;
    try {
      resp = await fetch(`${this.baseUrl}/parse?${params}`, {
        method: 'POST',
        body,
        signal: AbortSignal.timeout(this.timeout),
      });
    } catch (e) {
      throw new DocparseError(`transport failed: ${(e as Error).message}`);
    }
    if (!resp.ok) {
      throw new DocparseError((await resp.text()).trim() || `HTTP ${resp.status}`);
    }
    return JSON_FORMATS.includes(format) ? resp.json() : resp.text();
  }

  chunks(path: string, opts: EnhanceOptions = {}): Promise<Chunk[]> {
    return this.parse(path, { ...opts, format: 'chunks' });
  }

  outline(path: string, opts: EnhanceOptions = {}): Promise<Section> {
    return this.parse(path, { ...opts, format: 'outline' });
  }

  /** Fetch one output JSON Schema (draft 2020-12) by name from the server. */
  async schema(name: JsonFormat | 'quality' | 'profile' | 'okf-bundle'): Promise<unknown> {
    const key = name === 'json' ? 'document' : name;
    const resp = await fetch(`${this.baseUrl}/schema/${key}`);
    if (!resp.ok) throw new DocparseError(`no schema '${key}' (HTTP ${resp.status})`);
    return resp.json();
  }
}
