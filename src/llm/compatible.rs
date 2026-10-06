//! OpenAI-compatible endpoints: the Chat Completions wire format with `max_tokens`.

use super::{CallError, Endpoint, LlmRequest, LlmResponse};

pub async fn call(client: &reqwest::Client, ep: &Endpoint, req: &LlmRequest) -> Result<LlmResponse, CallError> {
    super::openai::call(client, ep, req, false).await
}
