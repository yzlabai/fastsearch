//! Eval-only harness (not shipped): run PP-DocLayoutV2 in tract on raw f32
//! inputs dumped by scripts/spike/ppv2/golden.py and print boxes>0.5 sorted by
//! reading order, for numeric comparison vs the ONNX-Runtime golden
//! (scripts/spike/ppv2/compare.py). Guards the vendored tract patches +
//! the model export against regressions. See vendor/README.md + docs/analysis/.
//! Usage: cargo run --release -p docparse-ocr --example ppv2_run -- <model.onnx> [spike_dir]
use tract_onnx::prelude::*;

fn read_f32(p: &str) -> Vec<f32> {
    std::fs::read(p)
        .unwrap_or_else(|e| panic!("read {p}: {e}"))
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn main() -> TractResult<()> {
    let onnx = std::env::args()
        .nth(1)
        .expect("usage: ppv2_run <model.onnx> [spike_dir]");
    let dir = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "/tmp/ppv2_spike".into());
    let img = tensor1(&read_f32(&format!("{dir}/in_image.f32"))).into_shape(&[1, 3, 800, 800])?;
    let imshape = tensor1(&read_f32(&format!("{dir}/in_imshape.f32"))).into_shape(&[1, 2])?;
    let scale = tensor1(&read_f32(&format!("{dir}/in_scale.f32"))).into_shape(&[1, 2])?;

    let model = tract_onnx::onnx()
        .model_for_read(&mut &std::fs::read(&onnx)?[..])?
        .into_optimized()?
        .into_runnable()?;

    // ONNX input order: [im_shape, image, scale_factor].
    let mk = || {
        tvec!(
            imshape.clone().into(),
            img.clone().into(),
            scale.clone().into()
        )
    };
    if std::env::var_os("PPV2_TIME").is_some() {
        let _ = model.run(mk())?; // warmup
        let t = std::time::Instant::now();
        let iters = 5;
        for _ in 0..iters {
            let _ = model.run(mk())?;
        }
        eprintln!(
            "[time] tract run avg = {:.0} ms ({} iters)",
            t.elapsed().as_secs_f64() * 1000.0 / iters as f64,
            iters
        );
    }
    let out = model.run(mk())?;
    let b = out[0].to_plain_array_view::<f32>()?;
    let shape = b.shape().to_vec();
    let b = b.as_slice().unwrap();
    let (n, k) = (shape[0], shape[1]);
    // Dump full [N,k] output for numeric diff vs ORT (scripts/spike/ppv2/compare.py).
    let mut blob = (n as u32).to_le_bytes().to_vec();
    blob.extend((k as u32).to_le_bytes());
    for &v in b {
        blob.extend(v.to_le_bytes());
    }
    std::fs::write(format!("{dir}/tract_boxes.bin"), &blob)?;
    let mut rows: Vec<&[f32]> = (0..n)
        .map(|i| &b[i * k..(i + 1) * k])
        .filter(|r| r[1] > 0.5)
        .collect();
    rows.sort_by(|a, c| a[6].partial_cmp(&c[6]).unwrap());
    eprintln!("tract boxes>0.5: {}", rows.len());
    for r in rows.iter().take(10) {
        eprintln!(
            "  cls={:2} score={:.3} box=[{:.1},{:.1},{:.1},{:.1}] order={:.1}",
            r[0] as i32, r[1], r[2], r[3], r[4], r[5], r[6]
        );
    }
    Ok(())
}
