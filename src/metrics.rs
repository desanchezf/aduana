use serde_json::Value;

/// Figures Ollama reports in its own response body.
#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub struct OllamaMetrics {
    pub total_ms: Option<i64>,
    pub load_ms: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub tokens_per_second: Option<f64>,
}

impl OllamaMetrics {
    pub fn from_response(body: &[u8]) -> Self {
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return Self::default();
        };
        let int = |key: &str| value.get(key).and_then(Value::as_i64);
        let nanos_to_ms = |key: &str| int(key).map(|nanos| nanos / 1_000_000);
        let completion_tokens = int("eval_count");
        let tokens_per_second = match (completion_tokens, int("eval_duration")) {
            (Some(tokens), Some(nanos)) if nanos > 0 => Some(tokens as f64 * 1e9 / nanos as f64),
            _ => None,
        };
        Self {
            total_ms: nanos_to_ms("total_duration"),
            load_ms: nanos_to_ms("load_duration"),
            prompt_tokens: int("prompt_eval_count"),
            completion_tokens,
            tokens_per_second,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ollama_timings() {
        let metrics = OllamaMetrics::from_response(
            br#"{"total_duration":2000000000,"load_duration":5000000,
                 "prompt_eval_count":7,"eval_count":20,"eval_duration":4000000000}"#,
        );
        assert_eq!(metrics.total_ms, Some(2000));
        assert_eq!(metrics.load_ms, Some(5));
        assert_eq!(metrics.prompt_tokens, Some(7));
        assert_eq!(metrics.tokens_per_second, Some(5.0));
    }

    #[test]
    fn non_json_response_has_no_metrics() {
        assert_eq!(
            OllamaMetrics::from_response(b"nope"),
            OllamaMetrics::default()
        );
    }
}
