use crate::axes::Axis;
use crate::internal::*;
use ndarray::*;
use tract_linalg::block_quant::{BlockQuantValue, PackedBlockQuantFact, PackedBlockQuantFormat};
use tract_linalg::mmm::{EagerPackedInput, MMMInputFormat, MMMInputValue, PackedOpaqueFact};
use tract_linalg::pack::PackedFormat;

use super::ModePicker;

#[derive(Debug, Clone, Hash)]
pub struct StaticMatMulPack {
    pub(crate) format: Box<dyn MMMInputFormat>,
    pub(crate) k_axis: usize,
    pub(crate) mn_axis: usize,
}

impl Op for StaticMatMulPack {
    fn name(&self) -> Cow<'_ str> {
        "StaticMatMulPack".into()
    }

    fn info(&self) -> TractResult<Vec<String>> {
        Ok(vec![format!("{:?}. k axis: {}, mn axis: {}", self.format, self.k_axis, self.mn_axis)])
    }

    op_as_typed_op!();
    impl_op_same_as!();
}

impl PartialEq for StaticMatMulPack {
    fn eq(&self, other: &Self) -> bool {
        self.format.same_as(&*other.format)
            && self.k_axis == other.k_axis
            && self.mn_axis == other.mn_axis
    }
}

impl EvalOp for StaticMatMulPack {
    fn is_stateless(&self) -> bool {
        true
    }

    fn eval_with_session(
        &self,
        session: &SessionState,
        mut inputs: TVec<TValue>,
    ) -> TractResult<TVec<TValue>> {
        self.do_eval(session, inputs.remove(0))
    }
}

impl TypedOp for StaticMatMulPack {
    fn output_facts(&self, inputs: &[&TypedFact]) -> TractResult<TVec<TypedFact>> {
        let k = inputs[0].shape[self.k_axis].to_usize()?;
        let mn = inputs[0].shape[self.mn_axis].clone();
        let fact = PackedOpaqueFact { format: self.format.clone(), mn, k };
        Ok(tvec!(Opaque::fact(self.output_shape(&inputs[0].shape)).with_opaque_fact(fact)))
    }

    as_op!();
}

impl StaticMatMulPack {
    fn do_eval(&self, _session: &SessionState, input: TValue) -> TractResult<TVec<TValue>> {
        let value = self.format.prepare_tensor(&input, self.k_axis, self.mn_axis)?;
        Ok(tvec!(value.into_tvalue()))
    }

    pub fn output_shape<D: DimLike>(&self, input: &[D]) -> TVec<D> {
        let mut packed_shape: TVec<D> = input.into();
        packed_shape.remove(self.mn_axis.max(self.k_axis));
        packed_shape.remove(self.mn_axis.min(self.k_axis));
        packed_shape
    }
}

#[derive(Hash, Clone, Debug, PartialEq, Eq)]
pub struct DynPackedOpaqueFact {
    pub k: TDim,
    pub mn: TDim,
    pub packers: Vec<PackedFormat>,
}

impl OpaqueFact for DynPackedOpaqueFact {
    fn mem_size(&self) -> TDim {
        self.k.clone() * &self.mn * self.packers[0].dt.size_of()
    }

    fn same_as(&self, other: &dyn OpaqueFact) -> bool {
        other.downcast_ref::<Self>().is_some_and(|o| o == self)
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct OptSimpleMatMulPack {
    pub(crate) packed_format: PackedBlockQuantFormat,
    pub(crate) k: usize,
    pub(crate) m: usize,
}

impl Op for OptSimpleMatMulPack {
    fn name(&self) -> Cow<'_ str> {
        "OptSimpleMatMulPack".into()
    }
    op_as_typed_op!();
}

impl EvalOp for OptSimpleMatMulPack {
    fn is_stateless(&self) -> bool {
        true
    }

    fn state(
        &self,
        _session: &mut SessionState,
        _node_id: usize,
    ) -> TractResult<Option<Box<dyn OpState>>> {
        Ok(None)
    }

    fn eval(&self, inputs: TVec<TValue>) -> TractResult<TVec<TValue>> {
        let input = args_1!(inputs);
        let mut output = tensor1(
            &input
                .as_slice::<Opaque>()?
                .iter()
                .map(|i| {
                    let i = i.downcast_ref::<BlockQuantValue>().unwrap();
                    let iv: Box<dyn MMMInputValue> =
                        Box::new(self.packed_format.pack(&i.value, i.fact.k())?);
                    Ok(Opaque(Arc::new(iv)))
                })
                .collect::<TractResult<Vec<_>>>()?,
        );
        output.set_shape(input.shape())?;
        Ok(tvec!(output.into_tvalue()))
    }
}

impl TypedOp for OptSimpleMatMulPack {
    fn output_facts(&self, inputs: &[&TypedFact]) -> TractResult<TVec<TypedFact>> {
        let fact = Opaque::fact(inputs[0].shape.clone()).with_opaque_fact(PackedBlockQuantFact {
            format: self.packed_format.clone(),
            shape: tvec!(self.m, self.k),
        });
        Ok(tvec!(fact))
    }

    as_op!();
}
