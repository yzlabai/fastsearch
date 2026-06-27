/**
 * Output types, mirroring the docparse JSON contract. Field names are the JSON
 * (snake_case) names exactly, so a parsed response is already this shape.
 *
 * These are hand-written for the common formats; the *authoritative* schemas
 * ship in the repo (`schemas/*.json`, draft 2020-12) and from a running server
 * (`GET /openapi.json`, `GET /schema/{name}`). To generate full types for every
 * field, run e.g. `quicktype` over those schemas.
 */

/** Axis-aligned bounding box in PDF user space (origin bottom-left, y up, pt). */
export interface BBox {
  x0: number;
  y0: number;
  x1: number;
  y1: number;
}

export type ChunkKind = 'heading' | 'paragraph' | 'table' | 'code' | 'list_item';

/** One retrieval chunk (element of the `chunks` format) — `-f chunks`. */
export interface Chunk {
  /** Stable sequential id within the document. */
  id: number;
  kind: ChunkKind;
  text: string;
  /** Source page (1-based). */
  page: number;
  /** Union bbox of the source content on `page`. */
  bbox: BBox;
  /** Enclosing heading breadcrumb, outermost first. */
  heading_path: string[];
  /** Id of the enclosing structure-tree section (matches `Section.id`). */
  section_id: number;
  char_len: number;
}

/** A node of the document structure tree — `-f outline`. */
export interface Section {
  id: number;
  title: string;
  level: number;
  page: number;
  bbox: BBox;
  children?: Section[];
}

/** Provenance recorded on a parsed document. */
export interface Provenance {
  schema_version: string;
  parser: string;
  parser_version: string;
}

/**
 * The full IR (`-f json`). Typed loosely below `pages` — use the JSON Schema
 * (`schemas/document.json`) for the exhaustive element union if you need it.
 */
export interface DocparseDocument {
  source: string;
  provenance?: Provenance | null;
  pages: Array<{
    number: number;
    width: number;
    height: number;
    elements: Array<Record<string, unknown>>;
  }>;
}

/** Formats whose response is decoded JSON. */
export type JsonFormat = 'json' | 'chunks' | 'outline';
/** Formats whose response is a plain string. */
export type TextFormat = 'markdown' | 'text';
export type Format = JsonFormat | TextFormat;

/** Enhancement toggles. All default off; a model-backed pass only runs when on. */
export interface EnhanceOptions {
  ocr?: boolean;
  layout?: boolean;
  /** Subprocess client: a UniRec model directory. HTTP client: a boolean. */
  tableModel?: boolean | string;
  formulaModel?: boolean | string;
}
