use std::io::{self, Read};
fn main() {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).unwrap();
    let cases: Vec<(String, bool)> = serde_json::from_str(&input).unwrap();
    let output: Vec<_> = cases
        .into_iter()
        .map(|(text, streaming)| studio_client_domain::markdown::parse(&text, streaming).unwrap())
        .collect();
    println!("{}", serde_json::to_string(&output).unwrap());
}
