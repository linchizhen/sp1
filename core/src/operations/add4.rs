use core::borrow::Borrow;
use core::borrow::BorrowMut;

use p3_air::AirBuilder;
use p3_field::AbstractField;
use p3_field::Field;
use std::mem::size_of;
use valida_derive::AlignedBorrow;

use crate::air::CurtaAirBuilder;
use crate::air::Word;
use crate::air::WORD_SIZE;
use crate::bytes::ByteOpcode;
use crate::runtime::Segment;

/// A set of columns needed to compute the add of four words.
#[derive(AlignedBorrow, Default, Debug, Clone, Copy)]
#[repr(C)]
pub struct Add4Operation<T> {
    /// The result of `a + b + c + d`.
    pub value: Word<T>,

    /// Indicates if the carry for the `i`th digit is 0.
    pub is_carry_0: Word<T>,

    /// Indicates if the carry for the `i`th digit is 1.
    pub is_carry_1: Word<T>,

    /// Indicates if the carry for the `i`th digit is 2.
    pub is_carry_2: Word<T>,

    /// Indicates if the carry for the `i`th digit is 3. The carry when adding 4 words is at most 3.
    pub is_carry_3: Word<T>,

    /// The carry for the `i`th digit.
    pub carry: Word<T>,
}

impl<F: Field> Add4Operation<F> {
    pub fn populate(
        &mut self,
        segment: &mut Segment,
        a_u32: u32,
        b_u32: u32,
        c_u32: u32,
        d_u32: u32,
    ) -> u32 {
        let expected = a_u32
            .wrapping_add(b_u32)
            .wrapping_add(c_u32)
            .wrapping_add(d_u32);
        self.value = Word::from(expected);
        let a = a_u32.to_le_bytes();
        let b = b_u32.to_le_bytes();
        let c = c_u32.to_le_bytes();
        let d = d_u32.to_le_bytes();

        let base = 256;
        let mut carry = [0u8, 0u8, 0u8, 0u8];
        for i in 0..WORD_SIZE {
            let mut res = (a[i] as u32) + (b[i] as u32) + (c[i] as u32) + (d[i] as u32);
            if i > 0 {
                res += carry[i - 1] as u32;
            }
            carry[i] = (res / base) as u8;
            self.is_carry_0[i] = F::from_bool(carry[i] == 0);
            self.is_carry_1[i] = F::from_bool(carry[i] == 1);
            self.is_carry_2[i] = F::from_bool(carry[i] == 2);
            self.is_carry_3[i] = F::from_bool(carry[i] == 3);
            self.carry[i] = F::from_canonical_u8(carry[i]);
            debug_assert!(carry[i] <= 3);
            debug_assert_eq!(self.value[i], F::from_canonical_u32(res % base));
        }

        // Range check.
        {
            segment.add_u8_range_checks(&a);
            segment.add_u8_range_checks(&b);
            segment.add_u8_range_checks(&c);
            segment.add_u8_range_checks(&d);
            segment.add_u8_range_checks(&expected.to_le_bytes());
        }
        expected
    }

    pub fn eval<AB: CurtaAirBuilder>(
        builder: &mut AB,
        a: Word<AB::Var>,
        b: Word<AB::Var>,
        c: Word<AB::Var>,
        d: Word<AB::Var>,
        is_real: AB::Var,
        cols: Add4Operation<AB::Var>,
    ) {
        // Range check each byte.
        {
            let bytes: Vec<AB::Var> =
                a.0.iter()
                    .chain(b.0.iter())
                    .chain(c.0.iter())
                    .chain(d.0.iter())
                    .chain(cols.value.0.iter())
                    .copied()
                    .collect();

            // The byte length is always even since each word has 4 bytes.
            assert_eq!(bytes.len() % 2, 0);

            // Pass two bytes to range check at a time.
            for i in (0..bytes.len()).step_by(2) {
                builder.send_byte_pair(
                    AB::F::from_canonical_u32(ByteOpcode::U8Range as u32),
                    AB::F::zero(),
                    AB::F::zero(),
                    bytes[i],
                    bytes[i + 1],
                    is_real,
                );
            }
        }

        builder.assert_bool(is_real);
        let mut builder_is_real = builder.when(is_real);

        // Each value in is_carry_{0,1,2,3} is 0 or 1, and exactly one of them is 1 per digit.
        {
            for i in 0..WORD_SIZE {
                builder_is_real.assert_bool(cols.is_carry_0[i]);
                builder_is_real.assert_bool(cols.is_carry_1[i]);
                builder_is_real.assert_bool(cols.is_carry_2[i]);
                builder_is_real.assert_bool(cols.is_carry_3[i]);
                builder_is_real.assert_eq(
                    cols.is_carry_0[i]
                        + cols.is_carry_1[i]
                        + cols.is_carry_2[i]
                        + cols.is_carry_3[i],
                    AB::Expr::one(),
                );
            }
        }

        // Calculates carry from is_carry_{0,1,2,3}.
        {
            let one = AB::Expr::one();
            let two = AB::F::from_canonical_u32(2);
            let three = AB::F::from_canonical_u32(3);

            for i in 0..WORD_SIZE {
                builder_is_real.assert_eq(
                    cols.carry[i],
                    cols.is_carry_1[i] * one.clone()
                        + cols.is_carry_2[i] * two
                        + cols.is_carry_3[i] * three,
                );
            }
        }

        // Compare the sum and summands by looking at carry.
        {
            let base = AB::F::from_canonical_u32(256);
            // For each limb, assert that difference between the carried result and the non-carried
            // result is the product of carry and base.
            for i in 0..WORD_SIZE {
                let mut overflow = a[i] + b[i] + c[i] + d[i] - cols.value[i];
                if i > 0 {
                    overflow += cols.carry[i - 1].into();
                }
                builder_is_real.assert_eq(cols.carry[i] * base, overflow.clone());
            }
        }

        // Degree 3 constraint to avoid "OodEvaluationMismatch".
        builder.assert_zero(a[0] * b[0] * cols.value[0] - a[0] * b[0] * cols.value[0]);
    }
}