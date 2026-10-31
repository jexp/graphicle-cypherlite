//! Numeric and trigonometric builtins: abs/round/sqrt/ceil/floor, exp/log,
//! the trig family, degrees/radians/sign, `isnan`, and `rand`.

use cypherlite_core::{Error, Result, Value};

use crate::overlay::ExecGraph;

use super::super::numeric::{float_unary, num_unary, round_function};

pub(super) fn bi_isnan(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok(match vals.first() {
        Some(Value::Float(f)) => Value::Bool(f.is_nan()),
        Some(Value::Int(_)) => Value::Bool(false),
        Some(Value::Null) | None => Value::Null,
        _ => return Err(Error::ty("isNan() needs a number")),
    })
}

pub(super) fn bi_abs(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    num_unary(&vals, i64::checked_abs, |f| f.abs())
}

pub(super) fn bi_sqrt(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, |f| f.sqrt())
}

pub(super) fn bi_ceil(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, |f| f.ceil())
}

pub(super) fn bi_floor(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, |f| f.floor())
}

pub(super) fn bi_round(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    round_function(&vals)
}

pub(super) fn bi_e(_graph: &ExecGraph<'_>, _n: &str, _vals: Vec<Value>) -> Result<Value> {
    Ok(Value::Float(std::f64::consts::E))
}
pub(super) fn bi_pi(_graph: &ExecGraph<'_>, _n: &str, _vals: Vec<Value>) -> Result<Value> {
    Ok(Value::Float(std::f64::consts::PI))
}
pub(super) fn bi_exp(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, f64::exp)
}

pub(super) fn bi_ln(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, f64::ln)
}

pub(super) fn bi_log10(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, f64::log10)
}

pub(super) fn bi_sin(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, f64::sin)
}

pub(super) fn bi_cos(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, f64::cos)
}

pub(super) fn bi_tan(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, f64::tan)
}

pub(super) fn bi_cot(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, |f| 1.0 / f.tan())
}

pub(super) fn bi_sinh(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, f64::sinh)
}

pub(super) fn bi_cosh(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, f64::cosh)
}

pub(super) fn bi_tanh(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, f64::tanh)
}

pub(super) fn bi_coth(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, |f| if f == 0.0 { f64::NAN } else { 1.0 / f.tanh() })
}

pub(super) fn bi_asin(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, f64::asin)
}

pub(super) fn bi_acos(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, f64::acos)
}

pub(super) fn bi_atan(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, f64::atan)
}

pub(super) fn bi_atan2(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok({
        let y = vals.first().and_then(Value::as_number);
        let x = vals.get(1).and_then(Value::as_number);
        match (y, x) {
            (Some(y), Some(x)) => Value::Float(y.atan2(x)),
            _ if vals.iter().any(Value::is_null) => Value::Null,
            _ => return Err(Error::ty("atan2() requires numbers")),
        }
    })
}
pub(super) fn bi_degrees(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, f64::to_degrees)
}

pub(super) fn bi_radians(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    float_unary(&vals, f64::to_radians)
}

pub(super) fn bi_sign(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    Ok(match vals.first() {
        Some(Value::Int(i)) => Value::Int(i.signum()),
        Some(Value::Float(f)) => Value::Int(if *f > 0.0 {
            1
        } else if *f < 0.0 {
            -1
        } else {
            0
        }),
        Some(Value::Null) | None => Value::Null,
        _ => return Err(Error::ty("sign() needs a number")),
    })
}

/// `rand()` draws a fresh 64-bit entropy sample per call from the workspace
/// `rand` crate (ThreadRng/ChaCha12, OS-seeded) and scales the top 53 bits
/// into the uniform [0, 1) interval. No thread-local PRNG state survives, so
/// one observed value reveals nothing about the next (CWE-338).
pub(super) fn bi_rand(_graph: &ExecGraph<'_>, _n: &str, _vals: Vec<Value>) -> Result<Value> {
    Ok(Value::Float(
        (rand::random::<u64>() >> 11) as f64 / ((1u64 << 53) as f64),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;

    fn draw() -> f64 {
        let g = Graph::default();
        let graph = ExecGraph::read(&g);
        match bi_rand(&graph, "rand", vec![]).unwrap() {
            Value::Float(f) => f,
            other => panic!("rand() returned a non-float: {other:?}"),
        }
    }

    #[test]
    fn rand_draws_stay_in_the_unit_interval() {
        for _ in 0..1000 {
            let f = draw();
            assert!((0.0..1.0).contains(&f), "rand() out of [0, 1): {f}");
        }
    }

    #[test]
    fn rand_consecutive_draws_in_one_thread_differ() {
        // Regression for CWE-338: rand() used to replay a fixed-seed
        // xorshift64* sequence from a thread-local cell, so every thread
        // emitted the identical stream. Draws are now independent OS-seeded
        // entropy; nothing deterministic is pinned here, only that
        // back-to-back values in one thread are not trivially identical
        // (a collision is a 2^-53 event per pair).
        let a = draw();
        let b = draw();
        assert_ne!(
            a, b,
            "two rand() draws in one thread returned the same value"
        );
        use std::collections::HashSet;
        let draws: Vec<u64> = (0..100).map(|_| draw().to_bits()).collect();
        assert_eq!(
            draws.iter().copied().collect::<HashSet<_>>().len(),
            100,
            "rand() draws collided"
        );
    }
}
