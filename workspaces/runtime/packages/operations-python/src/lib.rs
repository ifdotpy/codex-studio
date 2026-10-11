use std::panic::{AssertUnwindSafe, catch_unwind};

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

mod record_adapter;

const PROTOCOL_VERSION: u32 = 3;

fn catch_panic<T, F>(operation: F) -> Result<T, ()>
where
    F: FnOnce() -> T,
{
    catch_unwind(AssertUnwindSafe(operation)).map_err(|_| ())
}

#[pyclass(module = "studio_operations_native")]
pub struct TransitionResult {
    #[pyo3(get)]
    kind: String,
    #[pyo3(get)]
    rejection_code: Option<String>,
    #[pyo3(get)]
    rejection_message: Option<String>,
    #[pyo3(get)]
    stage: Option<String>,
    #[pyo3(get)]
    outcome: Option<String>,
    #[pyo3(get)]
    cancel_requested: Option<bool>,
}

impl From<record_adapter::AdapterResult> for TransitionResult {
    fn from(result: record_adapter::AdapterResult) -> Self {
        Self {
            kind: result.kind.to_owned(),
            rejection_code: result.rejection_code,
            rejection_message: result.rejection_message,
            stage: result.stage.map(|value| value.label().to_owned()),
            outcome: result.outcome.map(|value| value.label().to_owned()),
            cancel_requested: result.cancel_requested,
        }
    }
}

#[pyfunction]
#[pyo3(signature = (record_json, event, fingerprint, *, proof=None, empty=false, generation=None, admission_fingerprint=None))]
fn transition_record(
    record_json: &str,
    event: &str,
    fingerprint: Vec<u8>,
    proof: Option<&str>,
    empty: bool,
    generation: Option<u64>,
    admission_fingerprint: Option<Vec<u8>>,
) -> PyResult<TransitionResult> {
    let result = catch_panic(|| {
        record_adapter::transition_record(
            record_json,
            event,
            fingerprint,
            proof,
            empty,
            generation,
            admission_fingerprint,
        )
    });
    match result {
        Ok(Ok(result)) => Ok(result.into()),
        Ok(Err(message)) => Err(PyValueError::new_err(message)),
        Err(()) => Err(PyRuntimeError::new_err(
            "Rust operation transition panicked",
        )),
    }
}

#[pyfunction]
fn reserve_tool_request(snapshot_json: &str) -> PyResult<String> {
    let result = catch_panic(|| record_adapter::reserve_tool_request(snapshot_json));
    match result {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(message)) => Err(PyValueError::new_err(message)),
        Err(()) => Err(PyRuntimeError::new_err(
            "Rust tool request reservation panicked",
        )),
    }
}

#[pymodule]
#[allow(unsafe_code)]
fn studio_operations_native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("PROTOCOL_VERSION", PROTOCOL_VERSION)?;
    module.add("EVENT_NAMES", record_adapter::EVENT_NAMES.to_vec())?;
    module.add("PROOF_NAMES", record_adapter::PROOF_NAMES.to_vec())?;
    module.add("RESULT_NAMES", record_adapter::RESULT_NAMES.to_vec())?;
    module.add("PERSISTED_STAGES", record_adapter::STAGE_NAMES.to_vec())?;
    module.add("PERSISTED_OUTCOMES", record_adapter::OUTCOME_NAMES.to_vec())?;
    module.add_class::<TransitionResult>()?;
    module.add_function(wrap_pyfunction!(transition_record, module)?)?;
    module.add_function(wrap_pyfunction!(reserve_tool_request, module)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::catch_panic;

    #[test]
    fn forced_panic_is_caught_for_conversion_to_python_exception() {
        assert!(catch_panic(|| panic!("forced test panic")).is_err());
    }
}
