use super::*;
use crate::providers::resolve_model;

fn model(name: &str) -> Model {
    resolve_model(name).unwrap()
}

/// The label beside a model in a picker: the price of one billed per token,
/// and nothing for a plan's.
#[test]
fn a_model_billed_per_token_is_labelled_with_its_price() {
    let (input, output) = prices(&model("opus")).unwrap();
    assert_eq!(
        label("opus", None),
        Some(format!(
            "API, ${}/${}",
            per_million(input),
            per_million(output)
        ))
    );
    assert_eq!(label("cc:opus", None), None);
    assert_eq!(label("sol", Some(OpenAiAuth::ChatGpt)), None);
    assert!(label("sol", Some(OpenAiAuth::ApiKey)).is_some());
    assert_eq!(label("nothing-like-a-model", None), None);
    assert_eq!(per_million(5.0), "5");
    assert_eq!(per_million(1.25), "1.25");
    assert_eq!(per_million(0.1), "0.10");
    assert_eq!(per_million(0.075), "0.075");
}
