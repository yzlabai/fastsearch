use crate::internal::*;

#[derive(Debug, Clone)]
struct BunchOfTensors(Vec<Tensor>);

#[derive(Debug, Clone)]
struct BunchofTensorsFact;

impl Opaque for BunchOfTensors {}
