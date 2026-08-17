//! A small, versioned interchange format for a numerical Symbolica model.
//!
//! The payload stores canonical, namespaced Symbolica source for each expression
//! and parameter. This keeps unrelated process-global state—such as Rubi's
//! internal rule symbols—outside the interchange boundary.
//!
//! The payload intentionally describes only the common symbolic boundary. ODE
//! solver configuration, integration results, and presentation belong to the
//! consuming plugin.
//!
//! The decoder accepts only bounded UTF-8 frames and reparses them with the
//! pinned Symbolica revision. Custom Rust normalization or evaluation hooks are
//! intentionally outside the format.

use std::collections::HashSet;
use std::fmt;
use std::io;

use symbolica::prelude::{Atom, AtomCore, ExpressionEvaluator, Indeterminate, ParseSettings};

/// The Symbolica revision whose atom representation this format carries.
pub const SYMBOLICA_REVISION: &str = "680f51f5b70c0ad00a2cc7745206b3c7de9af2c1";

/// The current Tymbolica atom-model payload version.
pub const PAYLOAD_VERSION: u16 = 2;

/// Maximum accepted size of a complete payload.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// Maximum accepted size of one canonical atom section.
pub const MAX_ATOM_BYTES: usize = 4 * 1024 * 1024;

/// Maximum cumulative stateless-atom size in one model.
pub const MAX_MODEL_ATOM_BYTES: usize = 8 * 1024 * 1024;

/// Maximum total number of expressions and parameters in one model.
pub const MAX_ATOM_COUNT: usize = 4096;

const MAGIC: &[u8; 8] = b"TYMATOM\0";
const REVISION_LEN: usize = 40;

/// An ordered numerical model crossing the plugin boundary.
///
/// `parameters[i]` corresponds to input value `i`, while `expressions[i]`
/// corresponds to output value `i`.
#[derive(Clone, Debug, PartialEq)]
pub struct AtomModel {
    expressions: Vec<Atom>,
    parameters: Vec<Atom>,
}

impl AtomModel {
    /// Construct a model and check its structural invariants.
    pub fn new(expressions: Vec<Atom>, parameters: Vec<Atom>) -> Result<Self, PayloadError> {
        let model = Self {
            expressions,
            parameters,
        };
        model.validate()?;
        Ok(model)
    }

    /// Validate the model without serializing it.
    pub fn validate(&self) -> Result<(), PayloadError> {
        if self.expressions.is_empty() {
            return Err(PayloadError::InvalidModel(
                "a model must contain at least one expression".into(),
            ));
        }

        let atom_count = self
            .expressions
            .len()
            .checked_add(self.parameters.len())
            .ok_or(PayloadError::LimitExceeded("atom count"))?;
        if atom_count > MAX_ATOM_COUNT {
            return Err(PayloadError::LimitExceeded("atom count"));
        }

        let mut atom_bytes = 0usize;
        for atom in self.expressions.iter().chain(&self.parameters) {
            let atom_len = atom.to_canonical_string().len();
            if atom_len > MAX_ATOM_BYTES {
                return Err(PayloadError::LimitExceeded("atom"));
            }
            atom_bytes = atom_bytes
                .checked_add(atom_len)
                .ok_or(PayloadError::LimitExceeded("model atoms"))?;
        }
        if atom_bytes > MAX_MODEL_ATOM_BYTES {
            return Err(PayloadError::LimitExceeded("model atoms"));
        }

        let mut seen = HashSet::with_capacity(self.parameters.len());
        for (index, parameter) in self.parameters.iter().enumerate() {
            Indeterminate::try_from(parameter.clone()).map_err(|message| {
                PayloadError::InvalidModel(format!(
                    "parameter {index} is not an indeterminate: {message}"
                ))
            })?;
            if !seen.insert(parameter) {
                return Err(PayloadError::InvalidModel(format!(
                    "parameter {index} duplicates an earlier parameter"
                )));
            }
        }

        Ok(())
    }

    /// Number of ordered model outputs.
    pub fn expression_count(&self) -> usize {
        self.expressions.len()
    }

    /// Number of ordered numerical inputs.
    pub fn parameter_count(&self) -> usize {
        self.parameters.len()
    }

    /// Encode this model as bounded canonical, namespaced expressions.
    pub fn encode(&self) -> Result<Vec<u8>, PayloadError> {
        self.validate()?;

        let mut output = Vec::new();
        append_checked(&mut output, MAGIC, "payload")?;
        append_checked(&mut output, &PAYLOAD_VERSION.to_le_bytes(), "payload")?;
        append_checked(&mut output, SYMBOLICA_REVISION.as_bytes(), "payload")?;
        write_u32(&mut output, self.expressions.len(), "expression count")?;
        write_u32(&mut output, self.parameters.len(), "parameter count")?;
        for atom in &self.expressions {
            write_atom_source(&mut output, atom, "expression atom")?;
        }
        for atom in &self.parameters {
            write_atom_source(&mut output, atom, "parameter atom")?;
        }

        Ok(output)
    }

    /// Decode a model, rejecting incompatible revisions, malformed lengths,
    /// excess data, and inputs above the documented limits.
    pub fn decode(input: &[u8]) -> Result<Self, PayloadError> {
        if input.len() > MAX_PAYLOAD_BYTES {
            return Err(PayloadError::LimitExceeded("payload"));
        }

        let mut reader = Reader::new(input);
        if reader.take(MAGIC.len(), "magic")? != MAGIC {
            return Err(PayloadError::InvalidMagic);
        }

        let version = reader.read_u16("payload version")?;
        if version != PAYLOAD_VERSION {
            return Err(PayloadError::UnsupportedVersion(version));
        }

        let revision = reader.take(REVISION_LEN, "Symbolica revision")?;
        if revision != SYMBOLICA_REVISION.as_bytes() {
            return Err(PayloadError::IncompatibleSymbolicaRevision(
                String::from_utf8_lossy(revision).into_owned(),
            ));
        }

        let expression_count = reader.read_count("expression count")?;
        if expression_count == 0 {
            return Err(PayloadError::InvalidModel(
                "a model must contain at least one expression".into(),
            ));
        }
        let parameter_count = reader.read_count("parameter count")?;
        let atom_count = expression_count
            .checked_add(parameter_count)
            .ok_or(PayloadError::LimitExceeded("atom count"))?;
        if atom_count > MAX_ATOM_COUNT {
            return Err(PayloadError::LimitExceeded("atom count"));
        }

        let expression_bytes = reader.read_atom_blobs(expression_count, "expression")?;
        let parameter_bytes = reader.read_atom_blobs(parameter_count, "parameter")?;
        reader.finish()?;

        let model_atom_bytes = expression_bytes
            .iter()
            .chain(&parameter_bytes)
            .try_fold(0usize, |total, atom| total.checked_add(atom.len()))
            .ok_or(PayloadError::LimitExceeded("model atoms"))?;
        if model_atom_bytes > MAX_MODEL_ATOM_BYTES {
            return Err(PayloadError::LimitExceeded("model atoms"));
        }

        let expressions = expression_bytes
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| decode_atom_source(bytes, format!("expression {index}")))
            .collect::<Result<Vec<_>, _>>()?;
        let parameters = parameter_bytes
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| decode_atom_source(bytes, format!("parameter {index}")))
            .collect::<Result<Vec<_>, _>>()?;

        Self::new(expressions, parameters)
    }

    /// Compile the model to a mutable real-valued evaluator suitable for a
    /// numerical solver callback.
    pub fn build_real_evaluator(&self) -> Result<RealEvaluator, PayloadError> {
        self.validate()?;
        let evaluator = Atom::evaluator_multiple(&self.expressions, &self.parameters)
            .build()
            .map_err(|error| PayloadError::Evaluation(error.to_string()))?;

        if !evaluator.is_real() {
            return Err(PayloadError::InvalidModel(
                "the model contains non-real coefficients".into(),
            ));
        }

        if evaluator.get_constants().iter().any(|coefficient| {
            let real = coefficient.re.to_f64();
            !real.is_finite() || real == 0.0 && !coefficient.re.is_zero()
        }) {
            return Err(PayloadError::InvalidModel(
                "a nonzero model coefficient is outside the finite f64 range".into(),
            ));
        }
        let inner = evaluator.map_coeff(&|coefficient| coefficient.re.to_f64());
        if inner
            .get_constants()
            .iter()
            .any(|coefficient| !coefficient.is_finite())
        {
            return Err(PayloadError::InvalidModel(
                "evaluating a model constant produced a non-finite f64".into(),
            ));
        }

        Ok(RealEvaluator { inner })
    }
}

/// A mutable, length-checked wrapper around Symbolica's real evaluator.
pub struct RealEvaluator {
    inner: ExpressionEvaluator<f64>,
}

impl RealEvaluator {
    pub fn input_len(&self) -> usize {
        self.inner.get_input_len()
    }

    pub fn output_len(&self) -> usize {
        self.inner.get_output_len()
    }

    pub fn evaluate(&mut self, inputs: &[f64], outputs: &mut [f64]) -> Result<(), PayloadError> {
        self.inner
            .try_evaluate(inputs, outputs)
            .map_err(|error| PayloadError::Evaluation(error.to_string()))
    }
}

/// Errors produced while validating, transporting, or compiling an atom model.
#[derive(Debug)]
pub enum PayloadError {
    InvalidMagic,
    UnsupportedVersion(u16),
    IncompatibleSymbolicaRevision(String),
    Truncated(&'static str),
    TrailingBytes,
    LimitExceeded(&'static str),
    InvalidAtom(String),
    InvalidModel(String),
    Evaluation(String),
    Io {
        operation: &'static str,
        source: io::Error,
    },
}

impl fmt::Display for PayloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMagic => formatter.write_str("invalid atom-model payload magic"),
            Self::UnsupportedVersion(version) => {
                write!(
                    formatter,
                    "unsupported atom-model payload version {version}"
                )
            }
            Self::IncompatibleSymbolicaRevision(revision) => write!(
                formatter,
                "incompatible Symbolica revision {revision:?}; expected {SYMBOLICA_REVISION}"
            ),
            Self::Truncated(section) => write!(formatter, "{section} is truncated"),
            Self::TrailingBytes => formatter.write_str("atom-model payload has trailing bytes"),
            Self::LimitExceeded(section) => write!(formatter, "{section} exceeds its size limit"),
            Self::InvalidAtom(message) => write!(formatter, "invalid atom: {message}"),
            Self::InvalidModel(message) => write!(formatter, "invalid model: {message}"),
            Self::Evaluation(message) => write!(formatter, "could not evaluate model: {message}"),
            Self::Io { operation, source } => write!(formatter, "error {operation}: {source}"),
        }
    }
}

impl std::error::Error for PayloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn write_atom_source(
    output: &mut Vec<u8>,
    atom: &Atom,
    section: &'static str,
) -> Result<(), PayloadError> {
    let source = atom.to_canonical_string();
    if source.len() > MAX_ATOM_BYTES {
        return Err(PayloadError::LimitExceeded(section));
    }
    write_blob(output, source.as_bytes(), section)
}

fn decode_atom_source(bytes: &[u8], label: String) -> Result<Atom, PayloadError> {
    if bytes.len() > MAX_ATOM_BYTES {
        return Err(PayloadError::LimitExceeded("atom"));
    }
    let source = std::str::from_utf8(bytes)
        .map_err(|error| PayloadError::InvalidAtom(format!("{label} is not UTF-8: {error}")))?;
    if source.is_empty() {
        return Err(PayloadError::InvalidAtom(format!("{label} is empty")));
    }
    Atom::parse(source, "tymbolica-atom-model", ParseSettings::default())
        .map_err(|error| PayloadError::InvalidAtom(format!("could not parse {label}: {error}")))
}

fn append_checked(
    output: &mut Vec<u8>,
    bytes: &[u8],
    section: &'static str,
) -> Result<(), PayloadError> {
    let new_len = output
        .len()
        .checked_add(bytes.len())
        .ok_or(PayloadError::LimitExceeded(section))?;
    if new_len > MAX_PAYLOAD_BYTES {
        return Err(PayloadError::LimitExceeded("payload"));
    }
    output.extend_from_slice(bytes);
    Ok(())
}

fn write_u32(
    output: &mut Vec<u8>,
    value: usize,
    section: &'static str,
) -> Result<(), PayloadError> {
    let value = u32::try_from(value).map_err(|_| PayloadError::LimitExceeded(section))?;
    append_checked(output, &value.to_le_bytes(), section)
}

fn write_blob(
    output: &mut Vec<u8>,
    bytes: &[u8],
    section: &'static str,
) -> Result<(), PayloadError> {
    write_u32(output, bytes.len(), section)?;
    append_checked(output, bytes, section)
}

struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { remaining: input }
    }

    fn take(&mut self, length: usize, section: &'static str) -> Result<&'a [u8], PayloadError> {
        if self.remaining.len() < length {
            return Err(PayloadError::Truncated(section));
        }
        let (value, remaining) = self.remaining.split_at(length);
        self.remaining = remaining;
        Ok(value)
    }

    fn read_u16(&mut self, section: &'static str) -> Result<u16, PayloadError> {
        Ok(u16::from_le_bytes(
            self.take(2, section)?.try_into().expect("fixed-size u16"),
        ))
    }

    fn read_u32(&mut self, section: &'static str) -> Result<u32, PayloadError> {
        Ok(u32::from_le_bytes(
            self.take(4, section)?.try_into().expect("fixed-size u32"),
        ))
    }

    fn read_count(&mut self, section: &'static str) -> Result<usize, PayloadError> {
        usize::try_from(self.read_u32(section)?).map_err(|_| PayloadError::LimitExceeded(section))
    }

    fn read_blob(
        &mut self,
        maximum: usize,
        section: &'static str,
    ) -> Result<&'a [u8], PayloadError> {
        let length = self.read_count(section)?;
        if length > maximum {
            return Err(PayloadError::LimitExceeded(section));
        }
        self.take(length, section)
    }

    fn read_atom_blobs(
        &mut self,
        count: usize,
        _kind: &'static str,
    ) -> Result<Vec<&'a [u8]>, PayloadError> {
        let mut atoms = Vec::with_capacity(count);
        for _ in 0..count {
            let atom = self.read_blob(MAX_ATOM_BYTES, "atom")?;
            atoms.push(atom);
        }
        Ok(atoms)
    }

    fn finish(self) -> Result<(), PayloadError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(PayloadError::TrailingBytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symbolica::parse;

    #[test]
    fn model_round_trips_and_builds_a_real_evaluator() {
        let model = AtomModel::new(vec![parse!("x^2+y")], vec![parse!("x"), parse!("y")]).unwrap();

        let payload = model.encode().unwrap();
        let decoded = AtomModel::decode(&payload).unwrap();
        assert_eq!(decoded, model);

        let mut evaluator = decoded.build_real_evaluator().unwrap();
        let mut output = [0.0];
        evaluator.evaluate(&[2.0, 3.0], &mut output).unwrap();
        assert_eq!(output, [7.0]);

        let mut payload_with_trailing_byte = payload;
        payload_with_trailing_byte.push(0);
        assert!(matches!(
            AtomModel::decode(&payload_with_trailing_byte),
            Err(PayloadError::TrailingBytes)
        ));

        let underflow = AtomModel::new(vec![parse!("x/10^400")], vec![parse!("x")]).unwrap();
        assert!(matches!(
            underflow.build_real_evaluator(),
            Err(PayloadError::InvalidModel(_))
        ));
    }
}
