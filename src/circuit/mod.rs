#![allow(clippy::too_many_arguments)]
use ff::PrimeField;

use crate::eval::IO;
use crate::store::{ScalarPointer, Store};

#[macro_use]
mod gadgets;
mod circuit_frame;
pub(crate) use circuit_frame::*;

pub trait ToInputs<F: PrimeField> {
    fn to_inputs(&self, store: &Store<F>) -> Vec<F>;
    fn input_size() -> usize;
}

impl<F: PrimeField, T: ToInputs<F>> ToInputs<F> for Option<T> {
    fn to_inputs(&self, store: &Store<F>) -> Vec<F> {
        if let Some(t) = self {
            t.to_inputs(store)
        } else {
            panic!("no inputs for None");
        }
    }
    fn input_size() -> usize {
        unimplemented!();
    }
}

impl<F: PrimeField> ToInputs<F> for IO<F> {
    fn to_inputs(&self, store: &Store<F>) -> Vec<F> {
        let expr = store.hash_expr(&self.expr).unwrap();
        let env = store.hash_expr(&self.env).unwrap();
        let cont = store.hash_cont(&self.cont).unwrap();
        let public_inputs = vec![
            *expr.tag(),
            *expr.value(),
            *env.tag(),
            *env.value(),
            *cont.tag(),
            *cont.value(),
        ];

        // This ensures `public_input_size` is kept in sync with any changes.
        assert_eq!(Self::input_size(), public_inputs.len());
        public_inputs
    }
    fn input_size() -> usize {
        6
    }
}