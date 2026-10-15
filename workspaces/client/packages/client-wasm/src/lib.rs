//! Browser bindings. Persistence and network I/O remain host adapters.
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn parse_markdown(text: &str, streaming: bool) -> Result<String, JsValue> {
    studio_client_domain::markdown::parse(text, streaming)
        .and_then(|value| serde_json::to_string(&value).map_err(|error| error.to_string()))
        .map_err(|error| JsValue::from_str(&error))
}
