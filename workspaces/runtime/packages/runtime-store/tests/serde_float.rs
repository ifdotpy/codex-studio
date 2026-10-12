#[test]
fn serde_json_rounds_long_decimal_inputs_to_nearest_f64() {
    for text in ["0.12345678901234567", "123456789.01234567"] {
        let parsed: f64 = serde_json::from_str(text).expect("finite JSON decimal parses");
        let expected = text.parse::<f64>().expect("Rust parses finite f64 literal");
        assert_eq!(parsed.to_bits(), expected.to_bits(), "{text}");
    }
}
