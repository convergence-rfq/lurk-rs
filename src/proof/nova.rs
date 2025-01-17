#![allow(non_snake_case)]

use std::marker::PhantomData;

use bellperson::{Circuit, ConstraintSystem, SynthesisError};
use merlin::Transcript;
use nova::{
    bellperson::{
        r1cs::{NovaShape, NovaWitness},
        shape_cs::ShapeCS,
        solver::SatisfyingAssignment,
    },
    errors::NovaError,
    r1cs::{
        R1CSGens, R1CSInstance, R1CSShape, R1CSWitness, RelaxedR1CSInstance, RelaxedR1CSWitness,
    },
    traits::Group,
    FinalSNARK, StepSNARK,
};
use pasta_curves::pallas;

use crate::circuit::MultiFrame;
use crate::eval::{Evaluator, Frame, Witness, IO};

use crate::field::LurkField;
use crate::proof::Prover;
use crate::store::{Ptr, Store};

type PallasPoint = pallas::Point;

pub struct Proof<G: Group> {
    pub step_proofs: Vec<StepSNARK<G>>,
    pub final_proof: FinalSNARK<G>,
    pub final_instance: RelaxedR1CSInstance<G>,
}

impl<G: Group> Proof<G> {
    pub fn verify(
        &self,
        shape_and_gens: &(R1CSShape<G>, R1CSGens<G>),
        instance: &RelaxedR1CSInstance<G>,
    ) -> bool {
        self.final_proof
            .verify(&shape_and_gens.1, &shape_and_gens.0, instance)
            .is_ok()
    }
}

pub trait Nova<F: LurkField>: Prover<F>
where
    <Self::Grp as Group>::Scalar: ff::PrimeField,
{
    type Grp: Group;

    fn make_r1cs(
        multi_frame: MultiFrame<
            '_,
            <Self::Grp as Group>::Scalar,
            IO<<Self::Grp as Group>::Scalar>,
            Witness<<Self::Grp as Group>::Scalar>,
        >,
        shape: &R1CSShape<Self::Grp>,
        gens: &R1CSGens<Self::Grp>,
    ) -> Result<(R1CSInstance<Self::Grp>, R1CSWitness<Self::Grp>), NovaError>
    where
        <<Self as Nova<F>>::Grp as Group>::Scalar: LurkField,
    {
        let mut cs = SatisfyingAssignment::<Self::Grp>::new();

        multi_frame.synthesize(&mut cs).unwrap();

        let (instance, witness) = cs.r1cs_instance_and_witness(shape, gens)?;

        Ok((instance, witness))
    }
    fn get_evaluation_frames(
        &self,
        expr: Ptr<<Self::Grp as Group>::Scalar>,
        env: Ptr<<Self::Grp as Group>::Scalar>,
        store: &mut Store<<Self::Grp as Group>::Scalar>,
        limit: usize,
    ) -> Vec<Frame<IO<<Self::Grp as Group>::Scalar>, Witness<<Self::Grp as Group>::Scalar>>>
    where
        <<Self as Nova<F>>::Grp as Group>::Scalar: LurkField,
    {
        let padding_predicate = |count| self.needs_frame_padding(count);

        let frames = Evaluator::generate_frames(expr, env, store, limit, padding_predicate);
        store.hydrate_scalar_cache();

        frames
    }
    fn evaluate_and_prove(
        &self,
        expr: Ptr<<Self::Grp as Group>::Scalar>,
        env: Ptr<<Self::Grp as Group>::Scalar>,
        store: &mut Store<<Self::Grp as Group>::Scalar>,
        limit: usize,
    ) -> Result<(Proof<Self::Grp>, RelaxedR1CSInstance<Self::Grp>), SynthesisError>
    where
        <<Self as Nova<F>>::Grp as Group>::Scalar: LurkField,
    {
        let frames = self.get_evaluation_frames(expr, env, store, limit);

        let (shape, gens) = self.make_shape_and_gens();

        self.make_proof(frames.as_slice(), &shape, &gens, store, true)
    }

    fn make_shape_and_gens(&self) -> (R1CSShape<Self::Grp>, R1CSGens<Self::Grp>);

    fn make_proof(
        &self,
        frames: &[Frame<IO<<Self::Grp as Group>::Scalar>, Witness<<Self::Grp as Group>::Scalar>>],
        shape: &R1CSShape<Self::Grp>,
        gens: &R1CSGens<Self::Grp>,
        store: &mut Store<<Self::Grp as Group>::Scalar>,
        verify_steps: bool, // Sanity check for development, until we have recursion.
    ) -> Result<(Proof<Self::Grp>, RelaxedR1CSInstance<Self::Grp>), SynthesisError>
    where
        <<Self as Nova<F>>::Grp as Group>::Scalar: LurkField,
    {
        let multiframes = MultiFrame::from_frames(self.chunk_frame_count(), frames, store);
        for mf in &multiframes {
            assert_eq!(
                Some(self.chunk_frame_count()),
                mf.frames.clone().map(|x| x.len())
            );
        }
        let r1cs_instances = multiframes
            .iter()
            .map(|f| Self::make_r1cs(f.clone(), shape, gens).unwrap())
            .collect::<Vec<_>>();

        let mut step_proofs = Vec::new();
        let mut prover_transcript = Transcript::new(b"LurkProver");
        let mut verifier_transcript = Transcript::new(b"LurkVerifier");

        let initial_acc = (
            RelaxedR1CSInstance::default(gens, shape),
            RelaxedR1CSWitness::default(shape),
        );

        let (acc_U, acc_W) =
            r1cs_instances
                .iter()
                .fold(initial_acc, |(acc_U, acc_W), (next_U, next_W)| {
                    let (step_proof, (step_U, step_W)) = Self::make_step_snark(
                        gens,
                        shape,
                        &acc_U,
                        &acc_W,
                        next_U,
                        next_W,
                        &mut prover_transcript,
                    );
                    if verify_steps {
                        step_proof
                            .verify(&acc_U, next_U, &mut verifier_transcript)
                            .unwrap();
                        step_proofs.push(step_proof);
                    };
                    (step_U, step_W)
                });

        let final_proof = Self::make_final_snark(&acc_W);

        let proof = Proof {
            step_proofs,
            final_proof,
            final_instance: acc_U.clone(),
        };

        Ok((proof, acc_U))
    }

    fn make_step_snark(
        gens: &R1CSGens<Self::Grp>,
        S: &R1CSShape<Self::Grp>,
        r_U: &RelaxedR1CSInstance<Self::Grp>,
        r_W: &RelaxedR1CSWitness<Self::Grp>,
        U2: &R1CSInstance<Self::Grp>,
        W2: &R1CSWitness<Self::Grp>,
        prover_transcript: &mut merlin::Transcript,
    ) -> (
        StepSNARK<Self::Grp>,
        (
            RelaxedR1CSInstance<Self::Grp>,
            RelaxedR1CSWitness<Self::Grp>,
        ),
    ) {
        let res = StepSNARK::prove(gens, S, r_U, r_W, U2, W2, prover_transcript);
        res.expect("make_step_snark failed")
    }

    fn make_final_snark(W: &RelaxedR1CSWitness<Self::Grp>) -> FinalSNARK<Self::Grp> {
        // produce a final SNARK
        let res = FinalSNARK::prove(W);
        res.expect("make_final_snark failed")
    }
}

pub struct NovaProver<F: LurkField> {
    chunk_frame_count: usize,
    _p: PhantomData<F>,
}

impl<F: LurkField> NovaProver<F> {
    pub fn new(chunk_frame_count: usize) -> Self {
        NovaProver::<F> {
            chunk_frame_count,
            _p: PhantomData::<F>,
        }
    }
}

impl<F: LurkField> Prover<F> for NovaProver<F> {
    fn chunk_frame_count(&self) -> usize {
        self.chunk_frame_count
    }
}

impl<F: LurkField> Nova<F> for NovaProver<F> {
    type Grp = PallasPoint;

    fn make_shape_and_gens(&self) -> (R1CSShape<Self::Grp>, R1CSGens<Self::Grp>) {
        let mut cs = ShapeCS::<Self::Grp>::new();
        let store = Store::<<Self::Grp as Group>::Scalar>::default();

        MultiFrame::blank(&store, self.chunk_frame_count)
            .synthesize(&mut cs)
            .unwrap();

        let shape = cs.r1cs_shape();
        let gens = cs.r1cs_gens();

        (shape, gens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::{empty_sym_env, Status};
    use crate::proof::{verify_sequential_css, SequentialCS};
    use crate::writer::Write;

    use bellperson::util_cs::{metric_cs::MetricCS, Comparable, Delta};
    use pallas::Scalar as Fr;

    const DEFAULT_CHECK_NOVA: bool = false;

    const DEFAULT_CHUNK_FRAME_COUNT: usize = 5;

    fn outer_prove_aux<Fo: Fn(&'_ mut Store<Fr>) -> Ptr<Fr>>(
        source: &str,
        expected_result: Fo,
        expected_cont: Status,
        expected_iterations: usize,
        chunk_frame_count: usize,
        check_nova: bool,
        check_constraint_systems: bool,
        limit: usize,
        debug: bool,
    ) {
        let mut s = Store::default();
        let expected_result = expected_result(&mut s);

        let expr = s.read(source).unwrap();

        let e = empty_sym_env(&s);

        let nova_prover = NovaProver::<Fr>::new(chunk_frame_count);
        let proof_results = if check_nova {
            Some(
                nova_prover
                    .evaluate_and_prove(expr, empty_sym_env(&s), &mut s, limit)
                    .unwrap(),
            )
        } else {
            None
        };

        let shape_and_gens = nova_prover.make_shape_and_gens();

        if check_nova {
            if let Some((proof, instance)) = proof_results {
                proof.verify(&shape_and_gens, &instance);
            }
        }

        if check_constraint_systems {
            let frames = nova_prover.get_evaluation_frames(expr, e, &mut s, limit);

            let multiframes = MultiFrame::from_frames(nova_prover.chunk_frame_count(), &frames, &s);
            let cs = nova_prover.outer_synthesize(&multiframes).unwrap();

            let adjusted_iterations = nova_prover.expected_total_iterations(expected_iterations);
            let output = cs[cs.len() - 1].0.output.unwrap();

            if !debug {
                dbg!(
                    multiframes.len(),
                    nova_prover.chunk_frame_count(),
                    frames.len(),
                    output.expr.fmt_to_string(&s)
                );

                assert_eq!(expected_iterations, Frame::significant_frame_count(&frames));
                assert_eq!(adjusted_iterations, cs.len());
                let status: Status = output.cont.into();
                assert_eq!(expected_cont, status);
                if expected_cont != Status::Error {
                    assert_eq!(expected_result, output.expr);
                }
            }

            let constraint_systems_verified = verify_sequential_css::<Fr>(&cs).unwrap();
            assert!(constraint_systems_verified);

            check_cs_deltas(&cs, nova_prover.chunk_frame_count());
        }
    }

    fn check_emitted_frames<Fo: Fn(&'_ mut Store<Fr>) -> Vec<Ptr<Fr>>>(
        source: &str,
        chunk_frame_count: usize,
        limit: usize,
        emitted: Fo,
    ) {
        let mut s = Store::default();

        let expr = s.read(source).unwrap();

        let e = empty_sym_env(&s);

        let nova_prover = NovaProver::<Fr>::new(chunk_frame_count);

        let frames = nova_prover.get_evaluation_frames(expr, e, &mut s, limit);

        let emitted_result = emitted(&mut s);

        let emitted_vec: Vec<_> = frames
            .iter()
            .flat_map(|frame| frame.output.maybe_emitted_expression(&s))
            .collect();
        assert_eq!(emitted_vec, emitted_result);
    }

    pub fn check_cs_deltas(
        constraint_systems: &SequentialCS<Fr, IO<Fr>, Witness<Fr>>,
        chunk_frame_count: usize,
    ) {
        let mut cs_blank = MetricCS::<Fr>::new();
        let store = Store::<Fr>::default();

        let blank = MultiFrame::<Fr, IO<Fr>, Witness<Fr>>::blank(&store, chunk_frame_count);
        blank
            .synthesize(&mut cs_blank)
            .expect("failed to synthesize");

        for (i, (_frame, cs)) in constraint_systems.iter().enumerate() {
            let delta = cs.delta(&cs_blank, true);
            dbg!(i, &delta);
            assert!(delta == Delta::Equal);
        }
    }

    // IMPORTANT: Run next tests at least once. Some are ignored because they
    // are expensive. The criteria is that if the number of iteractions is
    // more than 30 we ignore it.
    ////////////////////////////////////////////////////////////////////////////

    #[test]
    #[ignore]
    fn outer_prove_arithmetic_let() {
        outer_prove_aux(
            "(let ((a 5)
                      (b 1)
                      (c 2))
                 (/ (+ a b) c))",
            |store| store.num(3),
            Status::Terminal,
            18,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            100,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_binop() {
        outer_prove_aux(
            "(+ 1 2)",
            |store| store.num(3),
            Status::Terminal,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            100,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_eq() {
        outer_prove_aux(
            "(eq 5 5)",
            |store| store.t(),
            Status::Terminal,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            true, // Always check Nova in at least one test.
            true,
            100,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_num_equal() {
        outer_prove_aux(
            "(= 5 5)",
            |store| store.t(),
            Status::Terminal,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            100,
            false,
        );

        outer_prove_aux(
            "(= 5 6)",
            |store| store.nil(),
            Status::Terminal,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            100,
            false,
        );
    }

    #[test]
    fn outer_prove_invalid_num_equal() {
        outer_prove_aux(
            "(= 5 nil)",
            |store| store.nil(),
            Status::Error,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            100,
            false,
        );

        outer_prove_aux(
            "(= nil 5)",
            |store| store.num(5),
            Status::Error,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            100,
            false,
        );
    }

    #[test]
    fn outer_prove_quote_end_is_nil_error() {
        outer_prove_aux(
            "(quote (1) (2))",
            |store| store.num(1),
            Status::Error,
            1,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            10,
            false,
        );
    }

    #[test]
    fn outer_prove_if() {
        outer_prove_aux(
            "(if t 5 6)",
            |store| store.num(5),
            Status::Terminal,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            100,
            false,
        );

        outer_prove_aux(
            "(if nil 5 6)",
            |store| store.num(6),
            Status::Terminal,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            100,
            false,
        )
    }

    #[test]
    fn outer_prove_if_end_is_nil_error() {
        outer_prove_aux(
            "(if nil 5 6 7)",
            |store| store.num(5),
            Status::Error,
            2,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            100,
            false,
        )
    }

    #[test]
    #[ignore]
    fn outer_prove_if_fully_evaluates() {
        outer_prove_aux(
            "(if t (+ 5 5) 6)",
            |store| store.num(10),
            Status::Terminal,
            5,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            100,
            false,
        );
    }

    #[test]
    #[ignore] // Skip expensive tests in CI for now. Do run these locally, please.
    fn outer_prove_recursion1() {
        outer_prove_aux(
            "(letrec ((exp (lambda (base)
                               (lambda (exponent)
                                 (if (= 0 exponent)
                                     1
                                     (* base ((exp base) (- exponent 1))))))))
                 ((exp 5) 2))",
            |store| store.num(25),
            Status::Terminal,
            66,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            200,
            false,
        );
    }

    #[test]
    #[ignore] // Skip expensive tests in CI for now. Do run these locally, please.
    fn outer_prove_recursion2() {
        outer_prove_aux(
            "(letrec ((exp (lambda (base)
                                  (lambda (exponent)
                                     (lambda (acc)
                                       (if (= 0 exponent)
                                          acc
                                          (((exp base) (- exponent 1)) (* acc base))))))))
                (((exp 5) 2) 1))",
            |store| store.num(25),
            Status::Terminal,
            93,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    fn outer_prove_unop_regression_aux(chunk_count: usize) {
        outer_prove_aux(
            "(atom 123)",
            |store| store.sym("t"),
            Status::Terminal,
            2,
            chunk_count, // This needs to be 1 to exercise the bug.
            DEFAULT_CHECK_NOVA,
            true,
            10,
            false,
        );

        outer_prove_aux(
            "(car '(1 . 2))",
            |store| store.num(1),
            Status::Terminal,
            2,
            chunk_count, // This needs to be 1 to exercise the bug.
            DEFAULT_CHECK_NOVA,
            true,
            10,
            false,
        );

        outer_prove_aux(
            "(cdr '(1 . 2))",
            |store| store.num(2),
            Status::Terminal,
            2,
            chunk_count, // This needs to be 1 to exercise the bug.
            DEFAULT_CHECK_NOVA,
            true,
            10,
            false,
        );

        outer_prove_aux(
            "(emit 123)",
            |store| store.num(123),
            Status::Terminal,
            3,
            chunk_count,
            DEFAULT_CHECK_NOVA,
            true,
            10,
            false,
        )
    }

    #[test]
    #[ignore]
    fn outer_prove_unop_regression() {
        // We need to at least use chunk size 1 to exercise the regression.
        // Also use a non-1 value to check the MultiFrame case.
        for i in 1..2 {
            outer_prove_unop_regression_aux(i);
        }
    }

    #[test]
    #[ignore]
    fn outer_prove_emit_output() {
        outer_prove_aux(
            "(emit 123)",
            |store| store.num(123),
            Status::Terminal,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            10,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate() {
        outer_prove_aux(
            "((lambda (x) x) 99)",
            |store| store.num(99),
            Status::Terminal,
            4,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate2() {
        outer_prove_aux(
            "((lambda (y)
                    ((lambda (x) y) 888))
                  99)",
            |store| store.num(99),
            Status::Terminal,
            9,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate3() {
        outer_prove_aux(
            "((lambda (y)
                     ((lambda (x)
                        ((lambda (z) z)
                         x))
                      y))
                   999)",
            |store| store.num(999),
            Status::Terminal,
            10,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate4() {
        outer_prove_aux(
            "((lambda (y)
                     ((lambda (x)
                        ((lambda (z) z)
                         x))
                      ;; NOTE: We pass a different value here.
                      888))
                  999)",
            |store| store.num(888),
            Status::Terminal,
            10,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate5() {
        outer_prove_aux(
            "(((lambda (fn)
                      (lambda (x) (fn x)))
                    (lambda (y) y))
                   999)",
            |store| store.num(999),
            Status::Terminal,
            13,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_sum() {
        outer_prove_aux(
            "(+ 2 (+ 3 4))",
            |store| store.num(9),
            Status::Terminal,
            6,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_binop_rest_is_nil() {
        outer_prove_aux(
            "(- 9 8 7)",
            |store| store.num(9),
            Status::Error,
            2,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_relop_rest_is_nil() {
        outer_prove_aux(
            "(= 9 8 7)",
            |store| store.num(9),
            Status::Error,
            2,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_diff() {
        outer_prove_aux(
            "(- 9 5)",
            |store| store.num(4),
            Status::Terminal,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_product() {
        outer_prove_aux(
            "(* 9 5)",
            |store| store.num(45),
            Status::Terminal,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_quotient() {
        outer_prove_aux(
            "(/ 21 3)",
            |store| store.num(7),
            Status::Terminal,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_error_div_by_zero() {
        outer_prove_aux(
            "(/ 21 0)",
            |store| store.num(0),
            Status::Error,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_error_invalid_type_and_not_cons() {
        outer_prove_aux(
            "(/ 21 nil)",
            |store| store.nil(),
            Status::Error,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_adder() {
        outer_prove_aux(
            "(((lambda (x)
                    (lambda (y)
                      (+ x y)))
                  2)
                 3)",
            |store| store.num(5),
            Status::Terminal,
            13,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_current_env_simple() {
        outer_prove_aux(
            "(current-env)",
            |store| store.nil(),
            Status::Terminal,
            1,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_current_env_rest_is_nil_error() {
        outer_prove_aux(
            "(current-env a)",
            |store| store.nil(),
            Status::Error,
            1,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_let_simple() {
        outer_prove_aux(
            "(let ((a 1))
                  a)",
            |store| store.num(1),
            Status::Terminal,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_let_end_is_nil_error() {
        outer_prove_aux(
            "(let ((a 1 2)) a)",
            |store| store.num(1),
            Status::Error,
            1,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_letrec_end_is_nil_error() {
        outer_prove_aux(
            "(letrec ((a 1 2)) a)",
            |store| store.num(1),
            Status::Error,
            1,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_let_empty_error() {
        outer_prove_aux(
            "(let)",
            |store| store.num(1),
            Status::Error,
            1,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_let_empty_body_error() {
        outer_prove_aux(
            "(let ((a 1)))",
            |store| store.num(1),
            Status::Error,
            1,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_letrec_empty_error() {
        outer_prove_aux(
            "(letrec)",
            |store| store.num(1),
            Status::Error,
            1,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_letrec_empty_body_error() {
        outer_prove_aux(
            "(letrec ((a 1)))",
            |store| store.num(1),
            Status::Error,
            1,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_let_body_nil() {
        outer_prove_aux(
            "(eq nil (let () nil))",
            |store| store.t(),
            Status::Terminal,
            4,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_let_rest_body_is_nil_error() {
        outer_prove_aux(
            "(let ((a 1)) a 1)",
            |store| store.sym("a"),
            Status::Error,
            1,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_letrec_rest_body_is_nil_error() {
        outer_prove_aux(
            "(letrec ((a 1)) a 1)",
            |store| store.sym("a"),
            Status::Error,
            1,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_let_null_bindings() {
        outer_prove_aux(
            "(let () (+ 1 2))",
            |store| store.num(3),
            Status::Terminal,
            4,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }
    #[test]
    #[ignore]
    fn outer_prove_evaluate_letrec_null_bindings() {
        outer_prove_aux(
            "(letrec () (+ 1 2))",
            |store| store.num(3),
            Status::Terminal,
            4,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_let() {
        outer_prove_aux(
            "(let ((a 1)
                       (b 2)
                       (c 3))
                  (+ a (+ b c)))",
            |store| store.num(6),
            Status::Terminal,
            18,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_arithmetic() {
        outer_prove_aux(
            "((((lambda (x)
                      (lambda (y)
                        (lambda (z)
                          (* z
                             (+ x y)))))
                    2)
                  3)
                 4)",
            |store| store.num(20),
            Status::Terminal,
            23,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_arithmetic_let() {
        outer_prove_aux(
            "(let ((x 2)
                        (y 3)
                        (z 4))
                   (* z (+ x y)))",
            |store| store.num(20),
            Status::Terminal,
            18,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_comparison() {
        outer_prove_aux(
            "(let ((x 2)
                       (y 3)
                       (z 4))
                  (= 20 (* z
                           (+ x y))))",
            |store| store.t(),
            Status::Terminal,
            21,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_conditional() {
        outer_prove_aux(
            "(let ((true (lambda (a)
                               (lambda (b)
                                 a)))
                       (false (lambda (a)
                                (lambda (b)
                                  b)))
                      ;; NOTE: We cannot shadow IF because it is built-in.
                      (if- (lambda (a)
                             (lambda (c)
                               (lambda (cond)
                                 ((cond a) c))))))
                 (((if- 5) 6) true))",
            |store| store.num(5),
            Status::Terminal,
            35,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_conditional2() {
        outer_prove_aux(
            "(let ((true (lambda (a)
                               (lambda (b)
                                 a)))
                       (false (lambda (a)
                                (lambda (b)
                                  b)))
                      ;; NOTE: We cannot shadow IF because it is built-in.
                      (if- (lambda (a)
                             (lambda (c)
                               (lambda (cond)
                                 ((cond a) c))))))
                 (((if- 5) 6) false))",
            |store| store.num(6),
            Status::Terminal,
            32,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_fundamental_conditional_bug() {
        outer_prove_aux(
            "(let ((true (lambda (a)
                               (lambda (b)
                                 a)))
                       ;; NOTE: We cannot shadow IF because it is built-in.
                       (if- (lambda (a)
                              (lambda (c)
                               (lambda (cond)
                                 ((cond a) c))))))
                 (((if- 5) 6) true))",
            |store| store.num(5),
            Status::Terminal,
            32,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_if() {
        outer_prove_aux(
            "(if nil 5 6)",
            |store| store.num(6),
            Status::Terminal,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_fully_evaluates() {
        outer_prove_aux(
            "(if t (+ 5 5) 6)",
            |store| store.num(10),
            Status::Terminal,
            5,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_recursion() {
        outer_prove_aux(
            "(letrec ((exp (lambda (base)
                                   (lambda (exponent)
                                     (if (= 0 exponent)
                                         1
                                         (* base ((exp base) (- exponent 1))))))))
                           ((exp 5) 2))",
            |store| store.num(25),
            Status::Terminal,
            66,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_recursion_multiarg() {
        outer_prove_aux(
            "(letrec ((exp (lambda (base exponent)
                                   (if (= 0 exponent)
                                       1
                                       (* base (exp base (- exponent 1)))))))
                           (exp 5 2))",
            |store| store.num(25),
            Status::Terminal,
            69,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_recursion_optimized() {
        outer_prove_aux(
            "(let ((exp (lambda (base)
                                (letrec ((base-inner
                                           (lambda (exponent)
                                             (if (= 0 exponent)
                                                 1
                                                 (* base (base-inner (- exponent 1)))))))
                                        base-inner))))
                   ((exp 5) 2))",
            |store| store.num(25),
            Status::Terminal,
            56,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_tail_recursion() {
        outer_prove_aux(
            "(letrec ((exp (lambda (base)
                                   (lambda (exponent-remaining)
                                     (lambda (acc)
                                       (if (= 0 exponent-remaining)
                                           acc
                                           (((exp base) (- exponent-remaining 1)) (* acc base))))))))
                          (((exp 5) 2) 1))",
            |store| store.num(25),
            Status::Terminal,
            93,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_tail_recursion_somewhat_optimized() {
        outer_prove_aux(
            "(letrec ((exp (lambda (base)
                                   (letrec ((base-inner
                                              (lambda (exponent-remaining)
                                                (lambda (acc)
                                                  (if (= 0 exponent-remaining)
                                                      acc
                                                     ((base-inner (- exponent-remaining 1)) (* acc base)))))))
                                           base-inner))))
                          (((exp 5) 2) 1))",
            |store| store.num(25),
            Status::Terminal,
            81,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_no_mutual_recursion() {
        outer_prove_aux(
            "(letrec ((even (lambda (n)
                                  (if (= 0 n)
                                      t
                                      (odd (- n 1)))))
                          (odd (lambda (n)
                                 (even (- n 1)))))
                        ;; NOTE: This is not true mutual-recursion.
                        ;; However, it exercises the behavior of LETREC.
                        (odd 1))",
            |store| store.t(),
            Status::Terminal,
            22,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            50,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_no_mutual_recursion_error() {
        outer_prove_aux(
            "(letrec ((even (lambda (n)
                                  (if (= 0 n)
                                      t
                                      (odd (- n 1)))))
                          (odd (lambda (n)
                                 (even (- n 1)))))
                        ;; NOTE: This is not true mutual-recursion.
                        ;; However, it exercises the behavior of LETREC.
                        (odd 2))",
            |store| store.sym("odd"),
            Status::Error,
            25,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_cons1() {
        outer_prove_aux(
            "(car (cons 1 2))",
            |store| store.num(1),
            Status::Terminal,
            5,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_car_end_is_nil_error() {
        outer_prove_aux(
            "(car (1 2) 3)",
            |store| store.num(1),
            Status::Error,
            1,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_cdr_end_is_nil_error() {
        outer_prove_aux(
            "(cdr (1 2) 3)",
            |store| store.num(1),
            Status::Error,
            1,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_atom_end_is_nil_error() {
        outer_prove_aux(
            "(atom 123 4)",
            |store| store.num(123),
            Status::Error,
            1,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_emit_end_is_nil_error() {
        outer_prove_aux(
            "(emit 123 4)",
            |store| store.num(123),
            Status::Error,
            1,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_cons2() {
        outer_prove_aux(
            "(cdr (cons 1 2))",
            |store| store.num(2),
            Status::Terminal,
            5,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_zero_arg_lambda1() {
        outer_prove_aux(
            "((lambda () 123))",
            |store| store.num(123),
            Status::Terminal,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_zero_arg_lambda2() {
        outer_prove_aux(
            "(let ((x 9) (f (lambda () (+ x 1)))) (f))",
            |store| store.num(10),
            Status::Terminal,
            10,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }
    #[test]
    fn outer_prove_evaluate_zero_arg_lambda3() {
        outer_prove_aux(
            "((lambda (x) 123))",
            |store| {
                let arg = store.sym("x");
                let num = store.num(123);
                let body = store.list(&[num]);
                let env = store.nil();
                store.intern_fun(arg, body, env)
            },
            Status::Terminal,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_zero_arg_lambda4() {
        outer_prove_aux(
            "((lambda () 123) 1)",
            |store| store.intern_num(1),
            Status::Error,
            3,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_evaluate_zero_arg_lambda5() {
        outer_prove_aux(
            "(123)",
            |store| store.intern_num(123),
            Status::Error,
            2,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_minimal_tail_call() {
        outer_prove_aux(
            "(letrec
                   ((f (lambda (x)
                         (if (= x 3)
                             123
                             (f (+ x 1))))))
                   (f 0))",
            |store| store.num(123),
            Status::Terminal,
            50,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            100,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_cons_in_function1() {
        outer_prove_aux(
            "(((lambda (a)
                    (lambda (b)
                      (car (cons a b))))
                  2)
                 3)",
            |store| store.num(2),
            Status::Terminal,
            15,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_cons_in_function2() {
        outer_prove_aux(
            "(((lambda (a)
                    (lambda (b)
                      (cdr (cons a b))))
                  2)
                 3)",
            |store| store.num(3),
            Status::Terminal,
            15,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_multiarg_eval_bug() {
        outer_prove_aux(
            "(car (cdr '(1 2 3 4)))",
            |store| store.num(2),
            Status::Terminal,
            4,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_multiple_letrec_bindings() {
        outer_prove_aux(
            "(letrec
                   ((x 888)
                    (f (lambda (x)
                         (if (= x 5)
                             123
                             (f (+ x 1))))))
                  (f 0))",
            |store| store.num(123),
            Status::Terminal,
            78,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_tail_call2() {
        outer_prove_aux(
            "(letrec
                   ((f (lambda (x)
                         (if (= x 5)
                             123
                             (f (+ x 1)))))
                    (g (lambda (x) (f x))))
                  (g 0))",
            |store| store.num(123),
            Status::Terminal,
            84,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_multiple_letrecstar_bindings() {
        outer_prove_aux(
            "(letrec ((double (lambda (x) (* 2 x)))
                           (square (lambda (x) (* x x))))
                          (+ (square 3) (double 2)))",
            |store| store.num(13),
            Status::Terminal,
            22,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_multiple_letrecstar_bindings_referencing() {
        outer_prove_aux(
            "(letrec ((double (lambda (x) (* 2 x)))
                           (double-inc (lambda (x) (+ 1 (double x)))))
                          (+ (double 3) (double-inc 2)))",
            |store| store.num(11),
            Status::Terminal,
            31,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_multiple_letrecstar_bindings_recursive() {
        outer_prove_aux(
            "(letrec ((exp (lambda (base exponent)
                                  (if (= 0 exponent)
                                      1
                                      (* base (exp base (- exponent 1))))))
                           (exp2 (lambda (base exponent)
                                   (if (= 0 exponent)
                                      1
                                      (* base (exp2 base (- exponent 1))))))
                          (exp3 (lambda (base exponent)
                                  (if (= 0 exponent)
                                      1
                                      (* base (exp3 base (- exponent 1)))))))
                         (+ (+ (exp 3 2) (exp2 2 3))
                            (exp3 4 2)))",
            |store| store.num(33),
            Status::Terminal,
            242,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_dont_discard_rest_env() {
        outer_prove_aux(
            "(let ((z 9))
                   (letrec ((a 1)
                             (b 2)
                             (l (lambda (x) (+ z x))))
                            (l 9)))",
            |store| store.num(18),
            Status::Terminal,
            22,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_evaluate_fibonacci() {
        outer_prove_aux(
            "(letrec ((next (lambda (a b n target)
                     (if (eq n target)
                         a
                         (next b
                             (+ a b)
                             (+ 1 n)
                            target))))
                    (fib (next 0 1 0)))
                (fib 1))",
            |store| store.num(1),
            Status::Terminal,
            89,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_terminal_continuation_regression() {
        let mut s = Store::<Fr>::default();
        let src = "(letrec ((a (lambda (x) (cons 2 2))))
                     (a 1))";

        let expr = s.read(src).unwrap();
        let limit = 300;

        let (
            IO {
                expr: result_expr,
                env: _new_env,
                cont: _continuation,
            },
            _iterations,
            _emitted,
        ) = Evaluator::new(expr, empty_sym_env(&s), &mut s, limit).eval();

        outer_prove_aux(
            &src,
            |_| result_expr,
            Status::Terminal,
            9,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    #[ignore]
    fn outer_prove_chained_functional_commitment() {
        let mut s = Store::<Fr>::default();

        let src = "(letrec ((secret 12345)
                       (a (lambda (acc x)
                            (let ((acc (+ acc x)))
                              (cons acc (cons secret (a acc)))))))
                (a 0 5))";

        let expr = s.read(src).unwrap();
        let limit = 300;

        let (
            IO {
                expr: result_expr,
                env: _new_env,
                cont: _continuation,
            },
            _iterations,
            _emitted,
        ) = Evaluator::new(expr, empty_sym_env(&s), &mut s, limit).eval();

        outer_prove_aux(
            &src,
            |_| result_expr,
            Status::Terminal,
            39,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }

    #[test]
    fn outer_prove_begin() {
        let mut s = Store::<Fr>::default();

        let src = "(begin (emit 1) (emit 2) (emit 3))";

        let expr = s.read(src).unwrap();
        let limit = 300;

        let (_io, _iterations, emitted) =
            Evaluator::new(expr, empty_sym_env(&s), &mut s, limit).eval();
        let expected = vec![s.num(1), s.num(2), s.num(3)];
        assert_eq!(emitted, expected);

        outer_prove_aux(
            src,
            |store| store.num(3),
            Status::Terminal,
            13,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );

        check_emitted_frames(&src, DEFAULT_CHUNK_FRAME_COUNT, 300, |store| {
            vec![store.num(1), store.num(2), store.num(3)]
        });
    }

    #[test]
    fn outer_prove_begin_empty() {
        outer_prove_aux(
            "(begin)",
            |store| store.nil(),
            Status::Terminal,
            2,
            DEFAULT_CHUNK_FRAME_COUNT,
            DEFAULT_CHECK_NOVA,
            true,
            300,
            false,
        );
    }
}
