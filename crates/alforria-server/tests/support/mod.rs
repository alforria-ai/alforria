/// Encode a query value, including spaces, Unicode and reserved characters.
pub fn encode_query_value(value: &str) -> String {
    let url = reqwest::Url::parse_with_params("http://localhost/", [("value", value)]).unwrap();
    url.query()
        .unwrap()
        .strip_prefix("value=")
        .unwrap()
        .to_owned()
}
