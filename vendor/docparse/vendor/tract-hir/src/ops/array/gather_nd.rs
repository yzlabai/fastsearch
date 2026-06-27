use crate::internal::*;
pub use tract_core::ops::array::GatherNd;

impl InferenceRulesOp for GatherNd {
    fn rules<'r, 'p: 'r, 's: 'r>(
        &'s self,
        s: &mut Solver<'r>,
        inputs: &'p [TensorProxy],
        outputs: &'p [TensorProxy],
    ) -> InferenceResult {
        check_input_arity(inputs, 2)?;
        check_output_arity(outputs, 1)?;
        s.equals(&outputs[0].datum_type, &inputs[0].datum_type)?;
        // docparse PATCH (see vendor/PATCHES.md #1): upstream 0.23.1 constrained
        // the output's trailing dims against `inputs[1]` (indices) — must be
        // `inputs[0]` (data). ONNX GatherND: out = indices.shape[:-1] ++
        // data.shape[n + batch_dims..]. The trailing dims come from DATA.
        let batch_dims = self.batch_dims;
        s.given(&inputs[1].rank, move |s, indices_rank| {
            let indices_rank = indices_rank as usize;
            for i in 0..(indices_rank - 1) {
                s.equals(&outputs[0].shape[i], &inputs[1].shape[i])?;
            }
            s.given_2(
                &inputs[1].shape[indices_rank - 1],
                &inputs[0].rank,
                move |s, n, data_rank| {
                    if let Ok(n) = n.to_i64() {
                        let n = n as usize;
                        for i in 0..(data_rank as usize).saturating_sub(n + batch_dims) {
                            s.equals(
                                &outputs[0].shape[indices_rank - 1 + i],
                                &inputs[0].shape[n + batch_dims + i],
                            )?;
                        }
                    }
                    Ok(())
                },
            )
        })
    }

    as_op!();
    to_typed!();
}
