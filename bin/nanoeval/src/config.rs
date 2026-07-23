use std::path::PathBuf;

use clap::{Args, builder::NonEmptyStringValueParser};
use eyre::{Result, WrapErr, eyre};
use nanocodex::{Nanocodex, NanocodexBuilder, OpenAiAuth, Thinking};

#[derive(Args)]
pub(crate) struct AgentArgs {
    /// Explicit `OpenAI` API key override. Otherwise `OPENAI_API_KEY` is preferred.
    #[arg(long, value_parser = NonEmptyStringValueParser::new())]
    api_key: Option<String>,

    /// Explicitly use `ChatGPT` authorization from this credential file.
    #[arg(long, env = "NANOCODEX_AUTH_FILE")]
    auth_file: Option<PathBuf>,

    /// Reasoning effort used by every fresh task agent.
    #[arg(long, env = "OPENAI_REASONING_EFFORT")]
    thinking: Option<Thinking>,
}

impl AgentArgs {
    pub(crate) fn builder(self, thinking: Thinking) -> Result<NanocodexBuilder> {
        let auth = Self::select_auth(self.api_key, self.auth_file, Self::environment_api_key()?)?;
        Ok(Nanocodex::builder(auth).thinking(thinking))
    }

    pub(crate) const fn thinking(&self) -> Option<Thinking> {
        self.thinking
    }

    fn select_auth(
        explicit_api_key: Option<String>,
        auth_file: Option<PathBuf>,
        environment_api_key: Option<String>,
    ) -> Result<OpenAiAuth> {
        if let Some(api_key) = explicit_api_key {
            return Ok(OpenAiAuth::api_key(api_key));
        }
        if let Some(auth_file) = auth_file {
            return Self::load_subscription_auth(&auth_file);
        }
        if let Some(api_key) = environment_api_key {
            return Ok(OpenAiAuth::api_key(api_key));
        }
        Self::load_subscription_auth(&Self::default_auth_file()?)
    }

    fn environment_api_key() -> Result<Option<String>> {
        match std::env::var("OPENAI_API_KEY") {
            Ok(api_key) if api_key.trim().is_empty() => Ok(None),
            Ok(api_key) => Ok(Some(api_key)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(error @ std::env::VarError::NotUnicode(_)) => {
                Err(error).wrap_err("OPENAI_API_KEY is not valid Unicode")
            }
        }
    }

    fn load_subscription_auth(auth_file: &std::path::Path) -> Result<OpenAiAuth> {
        nanocodex::load_chatgpt_auth(auth_file).map_err(|error| {
            eyre!(
                "ChatGPT authorization could not be loaded from {}: {error}. Run `nanocodex auth login`",
                auth_file.display()
            )
        })
    }

    fn default_auth_file() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("NANOCODEX_AUTH_FILE") {
            return Ok(PathBuf::from(path));
        }
        if let Some(path) = std::env::var_os("CODEX_HOME").filter(|path| !path.is_empty()) {
            return Ok(PathBuf::from(path).join("auth.json"));
        }
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .ok_or_else(|| {
                eyre!("home directory is unavailable; pass --auth-file or NANOCODEX_AUTH_FILE")
            })?;
        Ok(PathBuf::from(home).join(".codex/auth.json"))
    }
}
