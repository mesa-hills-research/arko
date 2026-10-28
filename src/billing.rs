//! How a worker's model is billed: on one of the user's plans (the `cc:`
//! models on a Claude plan, OpenAI models on the ChatGPT login), or per
//! token (an API key, Gemini, OpenRouter), with the words the lists and the
//! pickers show beside a model billed per token.

use arko_ai::Model;

use crate::providers::{self, OpenAiAuth};

/// A price per million tokens in dollars, as people write it: `5`,
/// `1.25`, `0.10`, `0.075`.
fn per_million(amount: f64) -> String {
    if amount.fract() == 0.0 {
        return format!("{amount:.0}");
    }
    let mut text = format!("{amount:.4}");
    while text.ends_with('0')
        && text
            .split_once('.')
            .is_some_and(|(_, cents)| cents.len() > 2)
    {
        text.pop();
    }
    text
}

/// `model`'s price per million input and output tokens, when Arko knows it.
fn prices(model: &Model) -> Option<(f64, f64)> {
    let pricing = model.pricing.at(std::time::SystemTime::now());
    let (input, output) = (
        pricing.input_cost_per_million,
        pricing.output_cost_per_million,
    );
    (input > 0.0 || output > 0.0).then_some((input, output))
}

/// How a model billed per token shows its billing beside its name: `API,
/// $5/$25` (per million input and output tokens), or `API` when its price
/// isn't known. A plan's model gets no label, since the lists say which
/// plan pays.
pub(crate) fn api_label(model: &Model) -> String {
    match prices(model) {
        Some((input, output)) => format!("API, ${}/${}", per_million(input), per_million(output)),
        None => "API".to_string(),
    }
}

/// How the model `name` (as `/model` takes it) is billed, for a model
/// picker beside its name, when the session reaches OpenAI models by
/// `openai_auth`: `API, $5/$25` (per million input and output tokens) for a
/// model billed per token, and `None` for one a plan pays for, which needs
/// no label beside the others.
pub fn label(name: &str, openai_auth: Option<OpenAiAuth>) -> Option<String> {
    let model = providers::resolve_model(name).ok()?;
    providers::plan_of(&model, openai_auth)
        .is_none()
        .then(|| api_label(&model))
}

#[cfg(test)]
mod tests;
