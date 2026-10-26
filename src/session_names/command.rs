//! `/session-names`, typed at the line prompt or sent from the web page,
//! and from the full-screen view with words.

use crate::remote::Source;

impl crate::Harness {
    /// Shows or sets how sessions are named. Without words it says the
    /// setting and the forms. The words are `words`, or the ways in the
    /// order to try them. It takes effect from the next session's first
    /// message.
    pub(crate) fn session_names_command(&mut self, argument: &str, source: Source) {
        let before = super::load(&self.root);
        let words: Vec<&str> = argument.split_whitespace().collect();
        let result = if words.is_empty() {
            Ok(format!("{} {}", super::describe(&before), super::USAGE))
        } else {
            super::parse(&words).and_then(|ways| {
                super::save(&self.root, &ways)?;
                Ok(super::describe(&ways))
            })
        };
        let changed = before != super::load(&self.root);
        self.setting_reply(result, source, changed);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Way, load};
    use crate::remote::{Kind, Source};

    /// The words save the ways in the order given, and `words` goes back to
    /// the first words. Each says the setting, and a word that is no way
    /// changes nothing.
    #[tokio::test]
    async fn the_command_saves_the_ways_in_the_order_given() {
        let temp = tempfile::tempdir().unwrap();
        let server = crate::mock_model::ModelServer::start(std::sync::Arc::new(|_, _| {
            Box::pin(async { crate::mock_model::complete("1", "Done.") })
        }))
        .await;
        let (mut harness, _inputs, _sender) =
            crate::tests::harness(temp.path(), server.client(), true);
        let said = |harness: &crate::Harness| {
            harness
                .feed
                .subscribe()
                .0
                .items
                .into_iter()
                .rev()
                .find(|item| matches!(item.kind, Kind::Output | Kind::Notice))
                .map(|item| item.text)
                .unwrap_or_default()
        };
        assert!(harness.live_command("/session-names", Source::Terminal));
        let text = said(&harness);
        assert!(
            text.starts_with("Sessions are named by the first words of their first message."),
            "{text}"
        );
        assert!(text.contains("/session-names words"), "{text}");
        assert!(harness.live_command("/session-names openai-key claude-plan", Source::Terminal));
        assert_eq!(load(&harness.root), [Way::OpenAiKey, Way::ClaudePlan]);
        let text = said(&harness);
        assert!(
            text.starts_with(
                "Sessions are named by a model, trying GPT-6 Luna with the OpenAI API key, then Haiku 5.5 on the Claude plan."
            ),
            "{text}"
        );
        assert!(harness.live_command("/session-names claude-plan gemini", Source::Terminal));
        assert_eq!(
            load(&harness.root),
            [Way::OpenAiKey, Way::ClaudePlan],
            "a word that is no way changes nothing"
        );
        assert!(
            said(&harness).contains("`gemini` is not a way"),
            "{}",
            said(&harness)
        );
        // From the web page, the change is announced.
        assert!(harness.live_command("/session-names words", Source::Web));
        assert!(load(&harness.root).is_empty());
        assert!(
            said(&harness).starts_with("Changed from the web page."),
            "{}",
            said(&harness)
        );
        harness.crew.as_ref().unwrap().shutdown().await;
    }
}
