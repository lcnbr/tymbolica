//! A small, versioned interchange format for a numerical Symbolica model.
//!
//! Symbolica's normal [`Atom::export`] format stores the complete Symbolica
//! state alongside every atom. This crate stores that state once and writes the
//! expression and parameter atoms with [`AtomView::write`]. A consumer imports
//! the state once and remaps every atom with [`Atom::import_with_map`].
//!
//! The payload intentionally describes only the common symbolic boundary. ODE
//! solver configuration, integration results, and presentation belong to the
//! consuming plugin.
//!
//! Version 1 is a trusted-producer format for ordinary Symbolica atoms. Its
//! decoder must only receive bytes created by a compatible Tymbolica plugin:
//! Symbolica's inner state/atom importer is not hardened for hostile input,
//! and custom Rust normalization or evaluation hooks are not serialized.

use std::collections::HashSet;
use std::fmt;
use std::io::{self, Cursor};

use symbolica::prelude::{Atom, AtomCore, ExpressionEvaluator, Indeterminate, State};

/// The Symbolica revision whose atom representation this format carries.
pub const SYMBOLICA_REVISION: &str = "9ad7ca3f59f9ed8637e3f4ae8157ead177662994";

/// The current Tymbolica atom-model payload version.
pub const PAYLOAD_VERSION: u16 = 1;

/// Maximum accepted size of a complete payload.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// Maximum accepted size of the shared Symbolica state section.
pub const MAX_STATE_BYTES: usize = 8 * 1024 * 1024;

/// Maximum accepted size of one stateless atom section.
pub const MAX_ATOM_BYTES: usize = 4 * 1024 * 1024;

/// Maximum cumulative stateless-atom size in one model.
pub const MAX_MODEL_ATOM_BYTES: usize = 8 * 1024 * 1024;

/// Maximum total number of expressions and parameters in one model.
pub const MAX_ATOM_COUNT: usize = 4096;

const MAGIC: &[u8; 8] = b"TYMATOM\0";
const REVISION_LEN: usize = 40;
const STATELESS_ATOM_HEADER_LEN: usize = 9;

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
            let atom_len = atom
                .as_atom_view()
                .get_data()
                .len()
                .checked_add(STATELESS_ATOM_HEADER_LEN)
                .ok_or(PayloadError::LimitExceeded("atom"))?;
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

    /// Encode this model with one shared Symbolica state snapshot.
    pub fn encode(&self) -> Result<Vec<u8>, PayloadError> {
        self.validate()?;

        let mut state = Vec::new();
        State::export(&mut state).map_err(|source| PayloadError::Io {
            operation: "exporting Symbolica state",
            source,
        })?;
        if state.len() > MAX_STATE_BYTES {
            return Err(PayloadError::LimitExceeded("Symbolica state"));
        }

        let mut output = Vec::new();
        append_checked(&mut output, MAGIC, "payload")?;
        append_checked(&mut output, &PAYLOAD_VERSION.to_le_bytes(), "payload")?;
        append_checked(&mut output, SYMBOLICA_REVISION.as_bytes(), "payload")?;
        write_u32(&mut output, self.expressions.len(), "expression count")?;
        write_u32(&mut output, self.parameters.len(), "parameter count")?;
        write_blob(&mut output, &state, "Symbolica state")?;
        for atom in &self.expressions {
            write_stateless_atom(&mut output, atom, "expression atom")?;
        }
        for atom in &self.parameters {
            write_stateless_atom(&mut output, atom, "parameter atom")?;
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

        let state = reader.read_blob(MAX_STATE_BYTES, "Symbolica state")?;
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

        // Import only after the outer envelope and every atom frame have been
        // checked. State import mutates Symbolica's process-global registry.
        let mut state_cursor = Cursor::new(state);
        let state_map =
            State::import(&mut state_cursor, None).map_err(|source| PayloadError::Io {
                operation: "importing Symbolica state",
                source,
            })?;
        require_cursor_eof(&state_cursor, state.len(), "Symbolica state")?;

        let expressions = expression_bytes
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| {
                decode_stateless_atom(bytes, &state_map, format!("expression {index}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let parameters = parameter_bytes
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| {
                decode_stateless_atom(bytes, &state_map, format!("parameter {index}"))
            })
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

fn write_stateless_atom(
    output: &mut Vec<u8>,
    atom: &Atom,
    section: &'static str,
) -> Result<(), PayloadError> {
    let atom_len = atom
        .as_atom_view()
        .get_data()
        .len()
        .checked_add(STATELESS_ATOM_HEADER_LEN)
        .ok_or(PayloadError::LimitExceeded(section))?;
    if atom_len > MAX_ATOM_BYTES {
        return Err(PayloadError::LimitExceeded(section));
    }
    let framed_len = atom_len
        .checked_add(4)
        .ok_or(PayloadError::LimitExceeded(section))?;
    if output
        .len()
        .checked_add(framed_len)
        .is_none_or(|length| length > MAX_PAYLOAD_BYTES)
    {
        return Err(PayloadError::LimitExceeded("payload"));
    }

    write_u32(output, atom_len, section)?;
    atom.as_atom_view()
        .write(output)
        .map_err(|source| PayloadError::Io {
            operation: "writing an atom",
            source,
        })
}

fn decode_stateless_atom(
    bytes: &[u8],
    state_map: &symbolica::state::StateMap,
    label: String,
) -> Result<Atom, PayloadError> {
    validate_stateless_atom_frame(bytes, &label)?;
    let mut cursor = Cursor::new(bytes);
    let atom =
        Atom::import_with_map(&mut cursor, state_map).map_err(|source| PayloadError::Io {
            operation: "importing an atom",
            source,
        })?;
    require_cursor_eof(&cursor, bytes.len(), "atom")?;
    Ok(atom)
}

fn validate_stateless_atom_frame(bytes: &[u8], label: &str) -> Result<(), PayloadError> {
    if bytes.len() > MAX_ATOM_BYTES {
        return Err(PayloadError::LimitExceeded("atom"));
    }
    if bytes.len() < STATELESS_ATOM_HEADER_LEN {
        return Err(PayloadError::InvalidAtom(format!("{label} is truncated")));
    }
    if bytes[0] != 0 {
        return Err(PayloadError::InvalidAtom(format!(
            "{label} has unsupported flags"
        )));
    }

    let declared = u64::from_le_bytes(
        bytes[1..STATELESS_ATOM_HEADER_LEN]
            .try_into()
            .expect("fixed-size atom length"),
    );
    let declared = usize::try_from(declared)
        .map_err(|_| PayloadError::InvalidAtom(format!("{label} length is out of range")))?;
    if declared != bytes.len() - STATELESS_ATOM_HEADER_LEN {
        return Err(PayloadError::InvalidAtom(format!(
            "{label} has an inconsistent length"
        )));
    }
    if declared == 0 {
        return Err(PayloadError::InvalidAtom(format!("{label} is empty")));
    }

    // The low three bits are Symbolica's atom discriminant at the pinned
    // revision: Num=1 through Pow=6. Checking it prevents the importer's
    // unreachable branch for obviously malformed frames.
    if !(1..=6).contains(&(bytes[STATELESS_ATOM_HEADER_LEN] & 0b111)) {
        return Err(PayloadError::InvalidAtom(format!(
            "{label} has an unknown atom type"
        )));
    }

    Ok(())
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

fn require_cursor_eof(
    cursor: &Cursor<&[u8]>,
    expected: usize,
    section: &'static str,
) -> Result<(), PayloadError> {
    if cursor.position() == expected as u64 {
        Ok(())
    } else {
        Err(PayloadError::InvalidAtom(format!(
            "{section} contains trailing bytes"
        )))
    }
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
        kind: &'static str,
    ) -> Result<Vec<&'a [u8]>, PayloadError> {
        let mut atoms = Vec::with_capacity(count);
        for index in 0..count {
            let atom = self.read_blob(MAX_ATOM_BYTES, "atom")?;
            validate_stateless_atom_frame(atom, &format!("{kind} {index}"))?;
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
