use std::panic::{AssertUnwindSafe, catch_unwind};

#[cfg(not(test))]
use pyo3::exceptions::{PyRuntimeError, PyValueError};
#[cfg(not(test))]
use pyo3::prelude::*;
#[cfg(not(test))]
use studio_operations::{Event, State, transition as core_transition};

#[cfg(not(test))]
const PROTOCOL_VERSION: u32 = 1;

fn catch_panic<T, F>(operation: F) -> Result<T, ()>
where
    F: FnOnce() -> T,
{
    catch_unwind(AssertUnwindSafe(operation)).map_err(|_| ())
}

#[cfg(not(test))]
fn transition_json(state_json: &str, event_json: &str) -> PyResult<String> {
    let result = catch_panic(|| -> PyResult<String> {
        let state: State = serde_json::from_str(state_json)
            .map_err(|error| PyValueError::new_err(format!("Invalid operation state: {error}")))?;
        let event: Event = serde_json::from_str(event_json)
            .map_err(|error| PyValueError::new_err(format!("Invalid operation event: {error}")))?;
        let decision = core_transition(&state, &event);
        serde_json::to_string(&decision).map_err(|error| {
            PyRuntimeError::new_err(format!("Could not encode operation decision: {error}"))
        })
    });
    match result {
        Ok(result) => result,
        Err(()) => Err(PyRuntimeError::new_err(
            "Rust operation transition panicked",
        )),
    }
}

#[cfg(not(test))]
#[pyfunction]
fn transition(state_json: &str, event_json: &str) -> PyResult<String> {
    transition_json(state_json, event_json)
}

#[cfg(not(test))]
#[pymodule]
#[allow(unsafe_code)]
fn studio_operations_native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("PROTOCOL_VERSION", PROTOCOL_VERSION)?;
    module.add_function(wrap_pyfunction!(transition, module)?)?;
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
