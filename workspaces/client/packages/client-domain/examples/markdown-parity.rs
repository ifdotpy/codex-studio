use std::io::{self, Read};
fn main() {
    let mut input = String::new();
    // The parity harness supplies UTF-8 JSON and expects malformed fixtures to fail.
    io::stdin()
        .read_to_string(&mut input)
        .expect("read parity fixtures");
    let cases: Vec<(String, bool)> = serde_json::from_str(&input).expect("valid parity fixtures");
    // All fixture documents are valid and within the parser budgets.
    let output: Vec<_> = cases
        .into_iter()
        .map(|(text, streaming)| {
            studio_client_domain::markdown::parse(&text, streaming).expect("valid Markdown fixture")
        })
        .collect();
    // Markdown trees contain only JSON-serializable fields.
    println!(
        "{}",
        serde_json::to_string(&output).expect("Markdown trees serialize")
    );
}
