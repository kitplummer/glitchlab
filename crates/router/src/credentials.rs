#[derive(Debug, Clone, Default)]
pub struct Credentials {
    pub anthropic_api_key: Option<String>,
    pub gemini_api_key: Option<String>,
    pub openai_api_key: Option<String>,
}
