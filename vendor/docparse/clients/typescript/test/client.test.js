// E2E smoke tests against the real binary. Build first, then:
//   cargo build --release
//   DOCPARSE_BIN=../../target/release/docparse npm test
// Runs on the built dist/. stdlib-only (node:test) — the client has no deps.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { DocparseClient, DocparseError } from '../dist/index.js';

const BIN = process.env.DOCPARSE_BIN || 'docparse';
const HAVE_BIN = BIN === 'docparse' || existsSync(BIN);

const SAMPLE = `# Title

A paragraph of body text for the client test.

- alpha
- beta
`;

function sample() {
  const dir = mkdtempSync(join(tmpdir(), 'docparse-ts-'));
  const p = join(dir, 'sample.md');
  writeFileSync(p, SAMPLE, 'utf8');
  return p;
}

test('json, chunks, outline, text round-trip', { skip: !HAVE_BIN }, async () => {
  const client = new DocparseClient({ binary: BIN });
  const p = sample();

  const doc = await client.parse(p);
  assert.equal(doc.pages[0].number, 1);

  const chunks = await client.chunks(p);
  assert.ok(chunks.some((c) => c.text.includes('body text')));
  assert.ok('bbox' in chunks[0] && 'page' in chunks[0]);

  const toc = await client.outline(p);
  assert.equal(toc.id, 0); // synthetic root
  assert.ok(Array.isArray(toc.children));

  const text = await client.parse(p, { format: 'text' });
  assert.ok(text.includes('Title'));
});

test('missing file is a clean DocparseError', { skip: !HAVE_BIN }, async () => {
  const client = new DocparseClient({ binary: BIN });
  await assert.rejects(() => client.parse('does-not-exist.pdf'), DocparseError);
});

test('unknown format is rejected', async () => {
  const client = new DocparseClient({ binary: BIN });
  await assert.rejects(() => client.parse('x.md', { format: 'yaml' }), DocparseError);
});
