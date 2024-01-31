use super::params::NUM_WITNESS_LIMBS;
use super::params::{convert_polynomial, convert_vec, Limbs};
use super::util::{compute_root_quotient_and_shift, split_u16_limbs_to_u8_limbs};
use super::util_air::eval_field_operation;
use crate::air::CurtaAirBuilder;
use crate::air::Polynomial;
use crate::utils::ec::field::FieldParameters;
use core::borrow::{Borrow, BorrowMut};
use core::mem::size_of;
use num::{BigUint, Zero};
use p3_air::AirBuilder;
use p3_baby_bear::BabyBear;
use p3_field::Field;
use std::fmt::Debug;
use valida_derive::AlignedBorrow;

#[derive(PartialEq, Copy, Clone, Debug)]
pub enum FpOperation {
    Add,
    Mul,
    Sub,
    Div, // We don't constrain that the divisor is non-zero.
}

/// A set of columns to compute `FpOperation(a, b)` where a, b are field elements.
/// Right now the number of limbs is assumed to be a constant, although this could be macro-ed
/// or made generic in the future.
#[derive(Debug, Clone, AlignedBorrow)]
#[repr(C)]
pub struct FpOpCols<T> {
    /// The result of `a op b`, where a, b are field elements
    pub result: Limbs<T>,
    pub(crate) carry: Limbs<T>,
    pub(crate) witness_low: [T; NUM_WITNESS_LIMBS],
    pub(crate) witness_high: [T; NUM_WITNESS_LIMBS],
}

impl<F: Field> FpOpCols<F> {
    pub fn populate<P: FieldParameters>(
        &mut self,
        a: &BigUint,
        b: &BigUint,
        op: FpOperation,
    ) -> BigUint {
        /// TODO: This operation relies on `F` being a PrimeField32, but our traits do not
        /// support that. This is a hack, since we always use BabyBear, to get around that, but
        /// all operations using "PF" should use "F" in the future.
        type PF = BabyBear;

        if b == &BigUint::zero() && op == FpOperation::Div {
            // Division by 0 is allowed only when dividing 0 so that padded rows can be all 0.
            assert_eq!(
                *a,
                BigUint::zero(),
                "division by zero is allowed only when dividing zero"
            );
        }

        let modulus = P::modulus();

        // If doing the subtraction operation, a - b = result, equivalent to a = result + b.
        if op == FpOperation::Sub {
            let result = (modulus.clone() + a - b) % &modulus;
            // We populate the carry, witness_low, witness_high as if we were doing an addition with result + b.
            // But we populate `result` with the actual result of the subtraction because those columns are expected
            // to contain the result by the user.
            // Note that this reversal means we have to flip result, a correspondingly in
            // the `eval` function.
            self.populate::<P>(&result, b, FpOperation::Add);
            let p_result: Polynomial<PF> = P::to_limbs_field::<PF>(&result).into();
            self.result = convert_polynomial(p_result);
            return result;
        }

        // a / b = result is equivalent to a = result * b.
        if op == FpOperation::Div {
            // As modulus is prime, we can use Fermat's little theorem to compute the
            // inverse.
            let result =
                (a * b.modpow(&(modulus.clone() - 2u32), &modulus.clone())) % modulus.clone();

            // We populate the carry, witness_low, witness_high as if we were doing a multiplication
            // with result * b. But we populate `result` with the actual result of the
            // multiplication because those columns are expected to contain the result by the user.
            // Note that this reversal means we have to flip result, a correspondingly in the `eval`
            // function.
            self.populate::<P>(&result, b, FpOperation::Mul);
            let p_result: Polynomial<PF> = P::to_limbs_field::<PF>(&result).into();
            self.result = convert_polynomial(p_result);
            return result;
        }

        let p_a: Polynomial<PF> = P::to_limbs_field::<PF>(a).into();
        let p_b: Polynomial<PF> = P::to_limbs_field::<PF>(b).into();

        // Compute field addition in the integers.
        let modulus = &P::modulus();
        let (result, carry) = match op {
            FpOperation::Add => ((a + b) % modulus, (a + b - (a + b) % modulus) / modulus),
            FpOperation::Mul => ((a * b) % modulus, (a * b - (a * b) % modulus) / modulus),
            FpOperation::Sub | FpOperation::Div => unreachable!(),
        };
        debug_assert!(&result < modulus);
        debug_assert!(&carry < modulus);
        match op {
            FpOperation::Add => debug_assert_eq!(&carry * modulus, a + b - &result),
            FpOperation::Mul => debug_assert_eq!(&carry * modulus, a * b - &result),
            FpOperation::Sub | FpOperation::Div => unreachable!(),
        }

        // Make little endian polynomial limbs.
        let p_modulus: Polynomial<PF> = P::to_limbs_field::<PF>(modulus).into();
        let p_result: Polynomial<PF> = P::to_limbs_field::<PF>(&result).into();
        let p_carry: Polynomial<PF> = P::to_limbs_field::<PF>(&carry).into();

        // Compute the vanishing polynomial.
        let p_op = match op {
            FpOperation::Add => &p_a + &p_b,
            FpOperation::Mul => &p_a * &p_b,
            FpOperation::Sub | FpOperation::Div => unreachable!(),
        };
        let p_vanishing: Polynomial<PF> = &p_op - &p_result - &p_carry * &p_modulus;
        debug_assert_eq!(p_vanishing.degree(), P::NB_WITNESS_LIMBS);

        let p_witness = compute_root_quotient_and_shift(
            &p_vanishing,
            P::WITNESS_OFFSET,
            P::NB_BITS_PER_LIMB as u32,
        );
        let (p_witness_low, p_witness_high) = split_u16_limbs_to_u8_limbs(&p_witness);

        self.result = convert_polynomial(p_result);
        self.carry = convert_polynomial(p_carry);
        self.witness_low = convert_vec(p_witness_low).try_into().unwrap();
        self.witness_high = convert_vec(p_witness_high).try_into().unwrap();

        result
    }
}

impl<V: Copy> FpOpCols<V> {
    #[allow(unused_variables)]
    pub fn eval<
        AB: CurtaAirBuilder<Var = V>,
        P: FieldParameters,
        A: Into<Polynomial<AB::Expr>> + Clone,
        B: Into<Polynomial<AB::Expr>> + Clone,
    >(
        &self,
        builder: &mut AB,
        a: &A,
        b: &B,
        op: FpOperation,
    ) where
        V: Into<AB::Expr>,
    {
        let p_a_param: Polynomial<AB::Expr> = (*a).clone().into();
        let p_b: Polynomial<AB::Expr> = (*b).clone().into();

        let (p_a, p_result): (Polynomial<_>, Polynomial<_>) = match op {
            FpOperation::Add | FpOperation::Mul => (p_a_param, self.result.into()),
            FpOperation::Sub | FpOperation::Div => (self.result.into(), p_a_param),
        };
        let p_carry: Polynomial<<AB as AirBuilder>::Expr> = self.carry.into();
        let p_op = match op {
            FpOperation::Add | FpOperation::Sub => p_a + p_b,
            FpOperation::Mul | FpOperation::Div => p_a * p_b,
        };
        let p_op_minus_result: Polynomial<AB::Expr> = p_op - p_result;
        let p_limbs = Polynomial::from_iter(P::modulus_field_iter::<AB::F>().map(AB::Expr::from));
        let p_vanishing = p_op_minus_result - &(&p_carry * &p_limbs);
        let p_witness_low = self.witness_low.iter().into();
        let p_witness_high = self.witness_high.iter().into();
        eval_field_operation::<AB, P>(builder, &p_vanishing, &p_witness_low, &p_witness_high);
    }
}

#[cfg(test)]
mod tests {
    use num::BigUint;
    use p3_air::BaseAir;
    use p3_field::Field;

    use super::{FpOpCols, FpOperation, Limbs};
    use crate::utils::ec::edwards::ed25519::Ed25519BaseField;
    use crate::utils::ec::field::FieldParameters;
    use crate::utils::{pad_to_power_of_two, BabyBearPoseidon2, StarkUtils};
    use crate::utils::{uni_stark_prove as prove, uni_stark_verify as verify};
    use crate::{air::CurtaAirBuilder, runtime::Segment, utils::Chip};
    use core::borrow::{Borrow, BorrowMut};
    use core::mem::{size_of, transmute};
    use num::bigint::RandBigInt;
    use p3_air::Air;
    use p3_baby_bear::BabyBear;
    use p3_matrix::dense::RowMajorMatrix;
    use p3_matrix::MatrixRowSlices;
    use rand::thread_rng;
    use valida_derive::AlignedBorrow;

    #[derive(AlignedBorrow, Debug, Clone)]
    pub struct TestCols<T> {
        pub a: Limbs<T>,
        pub b: Limbs<T>,
        pub a_op_b: FpOpCols<T>,
    }

    pub const NUM_TEST_COLS: usize = size_of::<TestCols<u8>>();

    struct FpOpChip<P: FieldParameters> {
        pub operation: FpOperation,
        pub _phantom: std::marker::PhantomData<P>,
    }

    impl<P: FieldParameters> FpOpChip<P> {
        pub fn new(operation: FpOperation) -> Self {
            Self {
                operation,
                _phantom: std::marker::PhantomData,
            }
        }
    }

    impl<F: Field, P: FieldParameters> Chip<F> for FpOpChip<P> {
        fn name(&self) -> String {
            format!("FpOp{:?}", self.operation)
        }

        fn generate_trace(&self, _: &mut Segment) -> RowMajorMatrix<F> {
            let mut rng = thread_rng();
            let num_rows = 1 << 8;
            let mut operands: Vec<(BigUint, BigUint)> = (0..num_rows - 5)
                .map(|_| {
                    let a = rng.gen_biguint(256) % &P::modulus();
                    let b = rng.gen_biguint(256) % &P::modulus();
                    (a, b)
                })
                .collect();

            // Hardcoded edge cases. We purposely include 0 / 0. While mathematically, that is not
            // allowed, we allow it in our implementation so padded rows can be all 0.
            operands.extend(vec![
                (BigUint::from(0u32), BigUint::from(0u32)),
                (BigUint::from(0u32), BigUint::from(1u32)),
                (BigUint::from(1u32), BigUint::from(2u32)),
                (BigUint::from(4u32), BigUint::from(5u32)),
                (BigUint::from(10u32), BigUint::from(19u32)),
            ]);

            let rows = operands
                .iter()
                .map(|(a, b)| {
                    let mut row = [F::zero(); NUM_TEST_COLS];
                    let cols: &mut TestCols<F> = unsafe { transmute(&mut row) };
                    cols.a = P::to_limbs_field::<F>(a);
                    cols.b = P::to_limbs_field::<F>(b);
                    cols.a_op_b.populate::<P>(a, b, self.operation);
                    row
                })
                .collect::<Vec<_>>();
            // Convert the trace to a row major matrix.
            let mut trace = RowMajorMatrix::new(
                rows.into_iter().flatten().collect::<Vec<_>>(),
                NUM_TEST_COLS,
            );

            // Pad the trace to a power of two.
            pad_to_power_of_two::<NUM_TEST_COLS, F>(&mut trace.values);

            trace
        }
    }

    impl<F: Field, P: FieldParameters> BaseAir<F> for FpOpChip<P> {
        fn width(&self) -> usize {
            NUM_TEST_COLS
        }
    }

    impl<AB, P: FieldParameters> Air<AB> for FpOpChip<P>
    where
        AB: CurtaAirBuilder,
    {
        fn eval(&self, builder: &mut AB) {
            let main = builder.main();
            let local: &TestCols<AB::Var> = main.row_slice(0).borrow();
            local
                .a_op_b
                .eval::<AB, P, _, _>(builder, &local.a, &local.b, self.operation);

            // A dummy constraint to keep the degree 3.
            builder.assert_zero(
                local.a[0] * local.b[0] * local.a[0] - local.a[0] * local.b[0] * local.a[0],
            )
        }
    }

    #[test]
    fn generate_trace() {
        for op in [FpOperation::Add, FpOperation::Mul, FpOperation::Sub].iter() {
            println!("op: {:?}", op);
            let chip: FpOpChip<Ed25519BaseField> = FpOpChip::new(*op);
            let mut segment = Segment::default();
            let _: RowMajorMatrix<BabyBear> = chip.generate_trace(&mut segment);
            // println!("{:?}", trace.values)
        }
    }

    #[test]
    fn prove_babybear() {
        let config = BabyBearPoseidon2::new(&mut rand::thread_rng());

        for op in [
            FpOperation::Add,
            FpOperation::Sub,
            FpOperation::Mul,
            FpOperation::Div,
        ]
        .iter()
        {
            println!("op: {:?}", op);

            let mut challenger = config.challenger();

            let chip: FpOpChip<Ed25519BaseField> = FpOpChip::new(*op);
            let mut segment = Segment::default();
            let trace: RowMajorMatrix<BabyBear> = chip.generate_trace(&mut segment);
            let proof = prove::<BabyBearPoseidon2, _>(&config, &chip, &mut challenger, trace);

            let mut challenger = config.challenger();
            verify(&config, &chip, &mut challenger, &proof).unwrap();
        }
    }
}