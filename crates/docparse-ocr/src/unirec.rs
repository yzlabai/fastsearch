//! UniRec-0.1B inference (G3-R): unified text / formula / TABLE recognition
//! on `tract` — the embedded table-structure model that revives G3 after
//! SLANet (ONNX `Loop`) and TATR (export) died. Spike-validated 2026-06-11
//! (docs/refer/openocr-0.1b-evaluation.md): both models optimize in tract
//! with symbolic dims, greedy decode is token-exact vs onnxruntime, and the
//! tract-0.23 kernels reach ~169 tok/s (≈2.5 s for a 316-token table).
//!
//! Pipeline: preprocess (aspect-preserving resize to ≤960×1408, /64-aligned,
//! `(x/255 − 0.5)/0.5`) → encoder once (emits fixed cross-attention K/V) →
//! host-driven greedy decode with per-layer KV cache (the autoregressive
//! loop lives HERE, not in the graph — that is what sidesteps `Loop`) →
//! detokenize (literal vocab strings, `Ġ`→space, `Ċ`→newline, cleanup).
//!
//! Models are external files (models/unirec/, ~700 MB, gitignored), located
//! via the same `find_file` convention as PP-OCR.

use crate::{find_file, resize_bilinear};
use anyhow::{Context, Result};
use std::path::Path;
use tract_onnx::prelude::*;

type Runnable = std::sync::Arc<TypedRunnableModel>;

/// Decoder layer count / heads / head-dim, fixed by the published model.
const LAYERS: usize = 6;
const HEADS: usize = 6;
const HEAD_DIM: usize = 128;
/// Max input canvas (width, height) — the model's native-resolution cap.
const MAX_SIDE: (usize, usize) = (960, 1408);
/// Spatial alignment required by the encoder.
const DIV: usize = 64;
/// M2M100-style positions start after the padding index.
const PADDING_IDX: i64 = 1;
/// Runaway-salvage loop guard (B2): on a decode that hit the token cap without
/// EOS, find the repeating tail block (period ≤[`LOOP_MAX_PERIOD`]) once it has
/// repeated this many times and trim it. Applied only to confirmed runaways, so
/// a low threshold is safe — a self-terminating table is never inspected.
const LOOP_MIN_REPEATS: usize = 4;
/// Longest repeating block (in tokens) the loop guard scans for.
const LOOP_MAX_PERIOD: usize = 24;

pub struct UniRec {
    encoder: Runnable,
    decoder: Runnable,
    vocab: Vec<String>,
    bos: i64,
    eos: i64,
}

impl UniRec {
    /// Load encoder/decoder/tokenizer from a directory (exact names first,
    /// then substring fallback — same drop-in convention as PP-OCR dirs).
    pub fn new(dir: &Path) -> Result<Self> {
        let enc_path = find_file(dir, &["unirec_encoder.onnx"], "encoder", "onnx")?;
        let dec_path = find_file(dir, &["unirec_decoder.onnx"], "decoder", "onnx")?;
        let map_path = find_file(dir, &["unirec_tokenizer_mapping.json"], "tokenizer", "json")?;

        let encoder = tract_onnx::onnx()
            .model_for_path(&enc_path)
            .with_context(|| format!("load {}", enc_path.display()))?
            .into_optimized()?
            .into_runnable()?;
        let decoder = tract_onnx::onnx()
            .model_for_path(&dec_path)
            .with_context(|| format!("load {}", dec_path.display()))?
            .into_optimized()?
            .into_runnable()?;

        let mapping: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&map_path)?).context("tokenizer mapping json")?;
        let vocab_size = mapping["vocab_size"]
            .as_u64()
            .context("vocab_size missing")? as usize;
        let mut vocab = vec![String::new(); vocab_size];
        let id_to_token = mapping["id_to_token"]
            .as_object()
            .context("id_to_token missing")?;
        for (k, v) in id_to_token {
            let id: usize = k.parse().context("token id")?;
            if let (Some(slot), Some(tok)) = (vocab.get_mut(id), v.as_str()) {
                *slot = tok.to_string();
            }
        }
        let bos = mapping["special_tokens"]["bos_token_id"]
            .as_i64()
            .unwrap_or(0);
        let eos = mapping["special_tokens"]["eos_token_id"]
            .as_i64()
            .unwrap_or(2);

        Ok(Self {
            encoder,
            decoder,
            vocab,
            bos,
            eos,
        })
    }

    /// Recognize an RGB region: returns the cleaned text (plain text,
    /// LaTeX-style formulas, or an HTML `<table>` — whatever the content is).
    pub fn recognize(&self, rgb: &[u8], w: usize, h: usize, max_tokens: usize) -> Result<String> {
        let (tw, th) = target_size(w, h);
        let small = resize_bilinear(rgb, w, h, tw, th);

        // NCHW, (x/255 - 0.5)/0.5.
        let mut pixel = Tensor::zero::<f32>(&[1, 3, th, tw])?;
        {
            let mut view = pixel.to_plain_array_view_mut::<f32>()?;
            let s = view.as_slice_mut().context("contiguous tensor")?;
            for c in 0..3 {
                for y in 0..th {
                    for x in 0..tw {
                        let v = small[(y * tw + x) * 3 + c] as f32 / 255.0;
                        s[c * th * tw + y * tw + x] = (v - 0.5) / 0.5;
                    }
                }
            }
        }

        let enc_out = self.encoder.run(tvec!(pixel.into()))?;
        let cross_k = enc_out[1].clone();
        let cross_v = enc_out[2].clone();

        let mut past: Vec<(Tensor, Tensor)> = (0..LAYERS)
            .map(|_| {
                (
                    Tensor::zero::<f32>(&[1, HEADS, 0, HEAD_DIM]).unwrap(),
                    Tensor::zero::<f32>(&[1, HEADS, 0, HEAD_DIM]).unwrap(),
                )
            })
            .collect();

        let mut token = self.bos;
        let mut ids = Vec::with_capacity(max_tokens);
        let mut hit_eos = false;
        for step in 0..max_tokens {
            let input_ids = Tensor::from_shape(&[1, 1], &[token])?;
            let pos = Tensor::from_shape(&[1, 1], &[PADDING_IDX + 1 + step as i64])?;
            let mut inputs: TVec<TValue> = tvec!(
                input_ids.into(),
                pos.into(),
                cross_k.clone(),
                cross_v.clone()
            );
            for (k, v) in &past {
                inputs.push(k.clone().into());
                inputs.push(v.clone().into());
            }
            let out = self.decoder.run(inputs)?;

            let logits = out[0].clone().into_tensor();
            let vocab_n = *logits.shape().last().context("logits shape")?;
            let view = logits.to_plain_array_view::<f32>()?;
            let flat: Vec<f32> = view.iter().copied().collect();
            let last = &flat[flat.len() - vocab_n..];
            let mut best = 0usize;
            let mut bv = f32::MIN;
            for (i, &v) in last.iter().enumerate() {
                if v > bv {
                    bv = v;
                    best = i;
                }
            }
            token = best as i64;
            if token == self.eos {
                hit_eos = true;
                break;
            }
            ids.push(token);
            for (i, slot) in past.iter_mut().enumerate() {
                *slot = (
                    out[1 + i * 2].clone().into_tensor(),
                    out[2 + i * 2].clone().into_tensor(),
                );
            }
        }

        // Salvage autoregressive degeneration, but ONLY on a runaway: a decode
        // that reached the token cap WITHOUT emitting EOS is the model stuck in
        // a repetition loop (B2). A self-terminating answer (hit_eos) is trusted
        // verbatim — so a well-formed table, even one with a column of identical
        // cells, is never trimmed. When runaway, cut the repeating tail and keep
        // the good prefix instead of letting `looks_degenerate` drop the whole
        // answer (which scored these academic tables at ~0).
        if !hit_eos {
            if let Some(keep) = loop_trim_len(&ids, LOOP_MIN_REPEATS) {
                ids.truncate(keep);
            }
        }

        Ok(self.detokenize(&ids))
    }

    /// Vocab strings are literal; `Ġ` marks a space, `Ċ` a newline. Cleanup
    /// rules ported from OpenOCR's `clean_special_tokens` (order matters for
    /// the soft-newline variants).
    fn detokenize(&self, ids: &[i64]) -> String {
        let mut s = String::new();
        for &id in ids {
            match self.vocab.get(id as usize) {
                Some(t) if !t.is_empty() => s.push_str(t),
                _ => {} // unknown id: drop (never panic on model output)
            }
        }
        let s = s
            .replace('Ġ', " ")
            .replace('Ċ', "\n")
            .replace("<|bos|>", "")
            .replace("<|eos|>", "")
            .replace("<|pad|>", "")
            .replace("-<|sn|>", "")
            .replace(" <|sn|>", " ")
            .replace("<|sn|>", " ")
            .replace("<|unk|>", "")
            .replace("<s>", "")
            .replace("</s>", "")
            .replace('\u{ffff}', "");
        let s = collapse_runs(&s, '_', 3);
        collapse_runs(&s, '.', 3)
    }
}

/// Autoregressive degeneration check: out-of-domain input (e.g. Korean fed
/// to this Chinese/English model) collapses into repetition loops — long
/// stretches of one phrase. Detect a substring of ≥6 chars repeated ≥4
/// times consecutively; callers must reject such output (hallucinated
/// volume defeats char-count gates).
pub fn looks_degenerate(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n < 48 {
        return false;
    }
    for period in 6..=64usize {
        if period * 4 > n {
            break;
        }
        let mut run = 0usize; // consecutive positions where s[i] == s[i+period]
        for i in 0..n - period {
            if chars[i] == chars[i + period] {
                run += 1;
                if run >= period * 3 {
                    return true; // ≥4 consecutive copies of a period-length phrase
                }
            } else {
                run = 0;
            }
        }
    }
    false
}

/// Detect a sustained verbatim repetition loop at the tail of the generated
/// token sequence. For each period `p` (1..=[`LOOP_MAX_PERIOD`]), count how
/// many identical `p`-token blocks end the sequence; if that count reaches
/// `min_repeats`, return the length to KEEP — the prefix plus exactly ONE copy
/// of the block (the runaway repeats are dropped). `None` when no loop is found.
///
/// Requiring many *identical* consecutive blocks is what makes this safe for
/// structured HTML output: a normal table's `</td><td>` repeats structurally
/// but the cell contents between the tags differ, so its blocks are not
/// identical and never accumulate to `min_repeats`.
fn loop_trim_len(ids: &[i64], min_repeats: usize) -> Option<usize> {
    let n = ids.len();
    for p in 1..=LOOP_MAX_PERIOD {
        if p * min_repeats > n {
            break;
        }
        let last = &ids[n - p..];
        let mut reps = 1usize;
        while (reps + 1) * p <= n && &ids[n - (reps + 1) * p..n - reps * p] == last {
            reps += 1;
        }
        if reps >= min_repeats {
            return Some(n - (reps - 1) * p);
        }
    }
    None
}

/// Aspect-preserving fit into [`MAX_SIDE`], floored to /64 alignment (≥64).
fn target_size(w: usize, h: usize) -> (usize, usize) {
    let (max_w, max_h) = MAX_SIDE;
    let (mut nw, mut nh) = (w as f64, h as f64);
    if w > max_w || h > max_h {
        let ar = w as f64 / h as f64;
        if (max_w as f64 / max_h as f64) >= ar {
            nh = max_h as f64;
            nw = nh * ar;
        } else {
            nw = max_w as f64;
            nh = nw / ar;
        }
    }
    let fw = ((nw as usize) / DIV * DIV).max(DIV);
    let fh = ((nh as usize) / DIV * DIV).max(DIV);
    (fw, fh)
}

/// Collapse runs of `c` longer than `keep` down to `keep` repetitions.
fn collapse_runs(s: &str, c: char, keep: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = 0usize;
    for ch in s.chars() {
        if ch == c {
            run += 1;
            if run <= keep {
                out.push(ch);
            }
        } else {
            run = 0;
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degeneration_detector() {
        let looping = "玉晶尼库仑硅产品最高层2.5-2.6元/片/".repeat(8);
        assert!(looks_degenerate(&looping));
        assert!(!looks_degenerate(
            "本公司成立于2020年，是一家专注于人工智能技术研发的高科技企业。公司总部位于北京，在上海、深圳设有分支机构。主要业务领域包括自然语言处理、计算机视觉与机器学习平台。"
        ));
        assert!(!looks_degenerate("short"));
    }

    #[test]
    fn loop_trim_rescues_degenerate_tail() {
        // Good prefix then a 3-token block repeated 10× → keep prefix + 1 copy.
        let mut ids: Vec<i64> = vec![10, 11, 12, 13, 14];
        for _ in 0..10 {
            ids.extend_from_slice(&[7, 8, 9]);
        }
        let keep = loop_trim_len(&ids, LOOP_MIN_REPEATS).expect("loop detected");
        assert_eq!(&ids[..keep], &[10, 11, 12, 13, 14, 7, 8, 9]);

        // A real table's blocks differ between cells (only the tags repeat) →
        // no verbatim block reaches the repeat threshold → not trimmed.
        let table: Vec<i64> = (0..200)
            .map(|i| [1i64, 2, (i % 37) + 100][i as usize % 3])
            .collect();
        assert!(loop_trim_len(&table, LOOP_MIN_REPEATS).is_none());

        // Fewer than min_repeats copies is left alone (legit short repetition).
        let mut few: Vec<i64> = vec![1, 2, 3];
        for _ in 0..3 {
            few.extend_from_slice(&[4, 5]);
        }
        assert!(loop_trim_len(&few, LOOP_MIN_REPEATS).is_none());
    }

    #[test]
    fn target_size_matches_reference() {
        // Python: 1247x520 -> fit under (960,1408) -> floor /64.
        assert_eq!(target_size(1247, 520), (960, 384));
        assert_eq!(target_size(100, 100), (64, 64));
        assert_eq!(target_size(960, 1408), (960, 1408));
        assert_eq!(target_size(2000, 4000), (704, 1408));
    }

    #[test]
    fn collapse_and_markers() {
        assert_eq!(collapse_runs("a____b", '_', 3), "a___b");
        assert_eq!(collapse_runs("a..b", '.', 3), "a..b");
        let u = UniRec {
            encoder: dummy_runnable(),
            decoder: dummy_runnable(),
            vocab: vec![
                "<|bos|>".into(),
                "Hello".into(),
                "Ġworld".into(),
                "Ċ".into(),
            ],
            bos: 0,
            eos: 2,
        };
        assert_eq!(u.detokenize(&[1, 2, 3]), "Hello world\n");
    }

    fn dummy_runnable() -> Runnable {
        // The smallest valid typed model: a single identity over a scalar.
        let mut m = TypedModel::default();
        let x = m.add_source("x", f32::fact([1])).unwrap();
        m.select_output_outlets(&[x]).unwrap();
        m.into_runnable().unwrap()
    }
}
