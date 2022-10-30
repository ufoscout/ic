//! Non-trivial mathematical operations on `PublicCoefficients`.

use super::super::ThresholdError;
use super::*;
use ic_crypto_internal_bls12_381_type::{G1Projective, G2Projective, Scalar};
use std::ops::MulAssign;

impl PublicCoefficients {
    /// Evaluate the public coefficients at x
    pub fn evaluate_at(&self, x: &Scalar) -> G2Projective {
        let mut coefficients = self.coefficients.iter().rev();
        let first = coefficients.next().map(|pk| pk.0.clone());
        match first {
            None => G2Projective::identity(),
            Some(ans) => {
                let mut ans: G2Projective = ans;
                for coeff in coefficients {
                    ans *= x;
                    ans += &coeff.0;
                }
                ans
            }
        }
    }

    /// Given a list of samples `(x, f(x) * g)` for a polynomial `f` in the scalar field, and a generator g of G1 returns
    /// `f(0) * g`.
    /// See: https://en.wikipedia.org/wiki/Shamir%27s_Secret_Sharing#Computationally_efficient_approach
    /// # Arguments:
    /// * `samples` contains the list of `(x, y)` points to be used in the interpolation, where `x` is an element in the scalar field, and the `y` is an element of G1.
    /// # Returns
    /// The generator `g` of G1 multiplied by to the constant term of the interpolated polynomial `f(x)`. If `samples` contains multiple entries for the same scalar `x`, only the first sample contributes toward the interpolation and the subsequent entries are discarded.
    pub fn interpolate_g1(
        samples: &[(Scalar, G1Projective)],
    ) -> Result<G1Projective, ThresholdError> {
        let all_x: Vec<Scalar> = samples.iter().map(|(x, _)| x.clone()).collect();
        let coefficients = Self::lagrange_coefficients_at_zero(&all_x)?;
        let mut result = G1Projective::identity();
        for (coefficient, sample) in coefficients.iter().zip(samples.iter().map(|(_, y)| y)) {
            result += sample * coefficient;
        }
        Ok(result)
    }

    /// Given a list of samples `(x, f(x) * g)` for a polynomial `f` in the scalar field, and a generator g of G2 returns
    /// `f(0) * g`.
    /// See: https://en.wikipedia.org/wiki/Shamir%27s_Secret_Sharing#Computationally_efficient_approach
    /// # Arguments:
    /// * `samples` contains the list of `(x, y)` points to be used in the interpolation, where `x` is an element in the scalar field, and the `y` is an element of G2.
    /// # Returns
    /// The generator `g` of G2 multiplied by to the constant term of the interpolated polynomial `f(x)`, i.e. `f(0)`. If `samples` contains multiple entries for the same scalar `x`, only the first sample contributes toward the interpolation and the subsequent entries are discarded.
    pub fn interpolate_g2(
        samples: &[(Scalar, G2Projective)],
    ) -> Result<G2Projective, ThresholdError> {
        let all_x: Vec<Scalar> = samples.iter().map(|(x, _)| x.clone()).collect();
        let coefficients = Self::lagrange_coefficients_at_zero(&all_x)?;
        let mut result = G2Projective::identity();
        for (coefficient, sample) in coefficients.iter().zip(samples.iter().map(|(_, y)| y)) {
            result += sample * coefficient;
        }
        Ok(result)
    }

    fn contains_duplicates(scalars: &[Scalar]) -> bool {
        let mut set = std::collections::HashSet::new();

        for scalar in scalars {
            if !set.insert(scalar.serialize().to_vec()) {
                return true;
            }
        }

        false
    }

    /// Compute the Lagrange coefficients at x=0.
    ///
    /// # Arguments
    /// * `samples` is a list of values x_0, x_1, ...x_n.
    /// # Result
    /// * `[lagrange_0, lagrange_1, ..., lagrange_n]` where:
    ///    * lagrange_i = numerator_i/denominator_i
    ///    * numerator_i = x_0 * x_1 * ... * x_(i-1) * x_(i+1) * ... * x_n
    ///    * denominator_i = (x_0 - x_i) * (x_1 - x_i) * ... * (x_(i-1) - x_i) *
    ///      (x_(i+1) - x_i) * ... * (x_n - x_i)
    /// # Errors
    /// `ThresholdError::DuplicateX`: in case the interpolation points `samples` are not all distinct.
    pub fn lagrange_coefficients_at_zero(
        samples: &[Scalar],
    ) -> Result<Vec<Scalar>, ThresholdError> {
        let len = samples.len();
        if len == 0 {
            return Ok(Vec::new());
        }
        if len == 1 {
            return Ok(vec![Scalar::one()]);
        }

        if Self::contains_duplicates(samples) {
            return Err(ThresholdError::DuplicateX);
        }

        // The j'th numerator is the product of all `x_prod[i]` for `i!=j`.
        // Note: The usual subtractions can be omitted as we are computing the Lagrange
        // coefficient at zero.
        let mut x_prod: Vec<Scalar> = Vec::with_capacity(len);
        let mut tmp = Scalar::one();
        x_prod.push(tmp.clone());
        for x in samples.iter().take(len - 1) {
            tmp *= x;
            x_prod.push(tmp.clone());
        }
        tmp = Scalar::one();
        for (i, x) in samples[1..].iter().enumerate().rev() {
            tmp *= x;
            x_prod[i] *= &tmp;
        }

        for (lagrange_0, x_i) in x_prod.iter_mut().zip(samples) {
            // Compute the value at 0 of the Lagrange polynomial that is `0` at the other
            // data points but `1` at `x`.
            let mut denom = Scalar::one();
            for x_j in samples.iter().filter(|x_j| *x_j != x_i) {
                let diff = x_j - x_i;
                denom *= &diff;
            }

            let inv = match denom.inverse() {
                None => return Err(ThresholdError::DuplicateX),
                Some(i) => i,
            };

            //lagrange_0 *= inv;
            lagrange_0.mul_assign(inv);
        }
        Ok(x_prod)
    }

    pub(super) fn remove_zeros(&mut self) {
        let zeros = self
            .coefficients
            .iter()
            .rev()
            .take_while(|c| c.0.is_identity())
            .count();
        let len = self.coefficients.len() - zeros;
        self.coefficients.truncate(len)
    }
}
