//! A deliberately small Peroxide consumer for Tymbolica atom models.
//!
//! This crate is a proof of the shared payload boundary: Tymbolica constructs
//! the symbolic model, while this independent plugin owns numerical ODE
//! integration and the shape of its result.

use std::cell::RefCell;
use std::io::Cursor;

use ciborium::value::Value;
use peroxide::{numerical::ode::RK4, prelude::*};
use tymbolica_atom_payload::{AtomModel, RealEvaluator};
use wasm_minimal_protocol::*;

initiate_protocol!();

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    if dest.is_null() && len != 0 {
        return Err(getrandom::Error::new_custom(1));
    }

    let mut state = 0x9e37_79b9_7f4a_7c15u64 ^ len as u64;
    for index in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        unsafe { dest.add(index).write((state >> 56) as u8) };
    }

    Ok(())
}

const MAX_STEPS: usize = 100_000;
const MAX_OUTPUT_VALUES: usize = 250_000;

#[derive(Debug)]
struct OdeConfig {
    t_start: f64,
    t_end: f64,
    step_size: f64,
    step_count: usize,
    initial: Vec<f64>,
}

fn decode_cbor(input: &[u8], label: &str) -> Result<Value, String> {
    let mut cursor = Cursor::new(input);
    let value = ciborium::from_reader::<Value, _>(&mut cursor)
        .map_err(|err| format!("{label} must be CBOR-encoded: {err}"))?;
    if cursor.position() != input.len() as u64 {
        return Err(format!("{label} has trailing bytes"));
    }
    Ok(value)
}

fn encode_cbor(value: Value) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    ciborium::into_writer(&value, &mut bytes)
        .map_err(|err| format!("failed to encode result: {err}"))?;
    Ok(bytes)
}

fn map_get<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter().find_map(|(candidate, value)| match candidate {
        Value::Text(candidate) if candidate == key => Some(value),
        _ => None,
    })
}

fn value_f64(value: &Value, label: &str) -> Result<f64, String> {
    let value = match value {
        Value::Float(value) => *value,
        Value::Integer(value) => {
            let value: i64 = (*value)
                .try_into()
                .map_err(|_| format!("{label} integer is out of range"))?;
            value as f64
        }
        other => return Err(format!("{label} must be a number, got {other:?}")),
    };
    if !value.is_finite() {
        return Err(format!("{label} must be finite"));
    }
    Ok(value)
}

fn config_from_cbor(input: &[u8]) -> Result<OdeConfig, String> {
    let Value::Map(map) = decode_cbor(input, "ODE configuration")? else {
        return Err("ODE configuration must be a dictionary".to_owned());
    };

    let Some(Value::Array(t_span)) = map_get(&map, "t-span") else {
        return Err("t-span must be a pair".to_owned());
    };
    if t_span.len() != 2 {
        return Err("t-span must contain exactly two numbers".to_owned());
    }
    let t_start = value_f64(&t_span[0], "t-span[0]")?;
    let t_end = value_f64(&t_span[1], "t-span[1]")?;
    if t_start >= t_end {
        return Err("t-span start must be less than its end".to_owned());
    }

    let step_size = value_f64(
        map_get(&map, "step-size").ok_or_else(|| "missing step-size".to_owned())?,
        "step-size",
    )?;
    if step_size <= 0.0 {
        return Err("step-size must be positive".to_owned());
    }
    if step_size > t_end - t_start {
        return Err("step-size must not exceed the integration interval".to_owned());
    }

    let Some(Value::Array(initial)) = map_get(&map, "initial") else {
        return Err("initial must be an array".to_owned());
    };
    if initial.is_empty() {
        return Err("initial must not be empty".to_owned());
    }
    let initial = initial
        .iter()
        .enumerate()
        .map(|(index, value)| value_f64(value, &format!("initial[{index}]")))
        .collect::<Result<Vec<_>, _>>()?;

    let step_count = (t_end - t_start) / step_size;
    if !step_count.is_finite() {
        return Err("the requested step count is not finite".to_owned());
    }
    let rounded_steps = step_count.round();
    let tolerance = 32.0 * f64::EPSILON * step_count.abs().max(1.0);
    if (step_count - rounded_steps).abs() > tolerance {
        return Err("step-size must divide the integration interval for fixed-step RK4".to_owned());
    }
    if rounded_steps < 1.0 {
        return Err("integration must contain at least one step".to_owned());
    }
    if rounded_steps > MAX_STEPS as f64 {
        return Err(format!(
            "integration would exceed the {MAX_STEPS}-step limit"
        ));
    }
    let step_count = rounded_steps as usize;

    Ok(OdeConfig {
        t_start,
        t_end,
        step_size,
        step_count,
        initial,
    })
}

struct SymbolicOde {
    evaluator: RefCell<RealEvaluator>,
}

impl ODEProblem for SymbolicOde {
    fn rhs(&self, t: f64, y: &[f64], dy: &mut [f64]) -> anyhow::Result<()> {
        if !t.is_finite() || y.iter().any(|value| !value.is_finite()) {
            anyhow::bail!("ODE solver supplied a non-finite time or state");
        }
        let mut inputs = Vec::with_capacity(y.len() + 1);
        inputs.push(t);
        inputs.extend_from_slice(y);

        self.evaluator
            .try_borrow_mut()
            .map_err(|err| anyhow::anyhow!("symbolic evaluator is already borrowed: {err}"))?
            .evaluate(&inputs, dy)
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;

        if dy.iter().any(|value| !value.is_finite()) {
            anyhow::bail!("symbolic right-hand side produced a non-finite value");
        }
        Ok(())
    }
}

fn solve_fixed_rk4<P: ODEProblem>(
    problem: &P,
    config: &OdeConfig,
) -> Result<(Vec<f64>, Vec<Vec<f64>>), String> {
    let row_count = config
        .step_count
        .checked_add(1)
        .ok_or_else(|| "result row count overflowed".to_owned())?;
    let row_width = config
        .initial
        .len()
        .checked_add(1)
        .ok_or_else(|| "result row width overflowed".to_owned())?;
    let output_values = row_count
        .checked_mul(row_width)
        .ok_or_else(|| "result size overflowed".to_owned())?;
    if output_values > MAX_OUTPUT_VALUES {
        return Err(format!(
            "result would contain {output_values} values; the limit is {MAX_OUTPUT_VALUES}"
        ));
    }

    let integrator = RK4;
    let mut time = config.t_start;
    let mut state = config.initial.clone();
    let mut times = Vec::with_capacity(row_count);
    let mut states = Vec::with_capacity(row_count);
    times.push(time);
    states.push(state.clone());

    for step_index in 0..config.step_count {
        let completed_steps = step_index + 1;
        let next_time = if completed_steps == config.step_count {
            config.t_end
        } else {
            config.t_start + completed_steps as f64 * config.step_size
        };
        if !next_time.is_finite() || next_time <= time {
            return Err(format!(
                "step-size is too small to advance time at step {completed_steps}"
            ));
        }

        integrator
            .step(problem, time, &mut state, config.step_size)
            .map_err(|err| format!("Peroxide RK4 step {completed_steps} failed: {err}"))?;
        if state.iter().any(|value| !value.is_finite()) {
            return Err(format!(
                "Peroxide RK4 step {completed_steps} produced a non-finite state"
            ));
        }

        time = next_time;
        times.push(time);
        states.push(state.clone());
    }

    Ok((times, states))
}

fn rows_to_cbor(times: &[f64], states: &[Vec<f64>]) -> Value {
    Value::Array(
        times
            .iter()
            .zip(states)
            .map(|(time, state)| {
                let mut row = Vec::with_capacity(state.len() + 1);
                row.push(Value::Float(*time));
                row.extend(state.iter().copied().map(Value::Float));
                Value::Array(row)
            })
            .collect(),
    )
}

/// Solve an imported atom model with Peroxide's fixed-step RK4 integrator.
///
/// The model parameters must be ordered as time followed by the state
/// variables. Results are CBOR rows of `[time, state_0, ...]`.
#[wasm_func]
pub fn solve_rk4(model: &[u8], config: &[u8]) -> Result<Vec<u8>, String> {
    let model = AtomModel::decode(model).map_err(|err| format!("invalid atom model: {err}"))?;
    let config = config_from_cbor(config)?;

    if model.expression_count() != config.initial.len() {
        return Err(format!(
            "model has {} right-hand sides but initial has {} states",
            model.expression_count(),
            config.initial.len()
        ));
    }
    if model.parameter_count() != config.initial.len() + 1 {
        return Err(format!(
            "model has {} parameters; expected time plus {} states",
            model.parameter_count(),
            config.initial.len()
        ));
    }

    let evaluator = model
        .build_real_evaluator()
        .map_err(|err| format!("could not compile atom model: {err}"))?;
    let problem = SymbolicOde {
        evaluator: RefCell::new(evaluator),
    };
    let (times, states) = solve_fixed_rk4(&problem, &config)?;

    encode_cbor(rows_to_cbor(&times, &states))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ConstantDerivative;

    impl ODEProblem for ConstantDerivative {
        fn rhs(&self, _t: f64, _y: &[f64], dy: &mut [f64]) -> anyhow::Result<()> {
            dy.fill(1.0);
            Ok(())
        }
    }

    fn test_config(t_start: f64, t_end: f64, step_size: f64) -> OdeConfig {
        let value = Value::Map(vec![
            (
                Value::Text("t-span".to_owned()),
                Value::Array(vec![Value::Float(t_start), Value::Float(t_end)]),
            ),
            (Value::Text("step-size".to_owned()), Value::Float(step_size)),
            (
                Value::Text("initial".to_owned()),
                Value::Array(vec![Value::Float(1.0)]),
            ),
        ]);
        config_from_cbor(&encode_cbor(value).unwrap()).unwrap()
    }

    #[test]
    fn decimal_step_count_drives_exactly_that_many_steps() {
        let config = test_config(0.0, 1.0, 0.1);
        assert_eq!(config.step_count, 10);

        let (times, states) = solve_fixed_rk4(&ConstantDerivative, &config).unwrap();
        assert_eq!(times.len(), 11);
        assert_eq!(times.last(), Some(&1.0));
        assert!((states.last().unwrap()[0] - 2.0).abs() < 1e-12);
    }

    #[test]
    fn rejects_a_step_that_cannot_advance_a_large_time() {
        let config = test_config(1e16, 1e16 + 2.0, 1.0);
        assert!(solve_fixed_rk4(&ConstantDerivative, &config).is_err());
    }
}
