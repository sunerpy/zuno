use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use url::Url;

use super::terminal_prompt::{self, Choice};

const AWS_CHAIN: &str = "aws-credential-chain";
const BEDROCK_BEARER: &str = "bedrock-bearer-token";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProviderTemplate {
    AmazonBedrock,
    OpenAiCompatible,
    OpenAiResponses,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SetupCredential {
    ApiKey,
    BedrockBearer,
    AwsCredentialChain,
}

pub(super) struct PreparedProviderSetup {
    provider_id: String,
    display_name: String,
    credential: SetupCredential,
    change: ConfigChange,
}

impl PreparedProviderSetup {
    pub(super) fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub(super) fn display_name(&self) -> &str {
        &self.display_name
    }

    pub(super) const fn credential(&self) -> SetupCredential {
        self.credential
    }

    pub(super) fn path(&self) -> &Path {
        self.change.path()
    }

    pub(super) fn commit(&self) -> Result<(), String> {
        self.change.commit()
    }

    pub(super) fn rollback(&self) -> Result<(), String> {
        self.change.rollback()
    }
}

pub(super) fn prepare(
    template: ProviderTemplate,
    layout: &zuno_paths::Layout,
    env: &zuno_paths::Env,
) -> Result<PreparedProviderSetup, String> {
    match template {
        ProviderTemplate::AmazonBedrock => prepare_bedrock(layout, env),
        ProviderTemplate::OpenAiCompatible => prepare_openai_compatible(layout),
        ProviderTemplate::OpenAiResponses => prepare_openai_responses(layout),
    }
}

fn prepare_bedrock(
    layout: &zuno_paths::Layout,
    env: &zuno_paths::Env,
) -> Result<PreparedProviderSetup, String> {
    let provider_id = "amazon-bedrock".to_owned();
    let display_name = "Amazon Bedrock".to_owned();
    let model_id = required_text("Bedrock model id", None)?;
    let model_name = terminal_prompt::text("Model display name", Some(&model_id))?;
    let region_default = env
        .truthy_value("AWS_REGION")
        .or_else(|| env.truthy_value("AWS_DEFAULT_REGION"))
        .unwrap_or("us-east-2");
    let region = terminal_prompt::text("AWS region", Some(region_default))?;
    let profile = terminal_prompt::optional_text("AWS profile", env.truthy_value("AWS_PROFILE"))?;
    let credential = terminal_prompt::select(
        "Amazon Bedrock authentication",
        vec![
            Choice::new(AWS_CHAIN, "AWS credential chain")
                .hinted("profile, access keys, IAM role, or web identity"),
            Choice::new(BEDROCK_BEARER, "Amazon Bedrock API key").hinted("Authorization: Bearer"),
        ],
    )?
    .ok_or_else(|| "provider setup cancelled".to_owned())?;
    let credential = match credential.as_str() {
        AWS_CHAIN => SetupCredential::AwsCredentialChain,
        BEDROCK_BEARER => SetupCredential::BedrockBearer,
        _ => return Err("unknown Amazon Bedrock authentication choice".to_owned()),
    };

    let mut options = Map::new();
    options.insert("region".to_owned(), Value::String(region));
    if let Some(profile) = profile {
        options.insert("profile".to_owned(), Value::String(profile));
    }
    let provider = provider_document(
        &display_name,
        "bedrock-mantle",
        Some("responses"),
        None,
        &model_id,
        &model_name,
        Some(Value::Object(options)),
    );
    prepared(
        layout,
        provider_id,
        display_name,
        model_id,
        credential,
        provider,
    )
}

fn prepare_openai_compatible(layout: &zuno_paths::Layout) -> Result<PreparedProviderSetup, String> {
    let provider_id = provider_id("Provider id", "openai-compatible")?;
    let display_name = terminal_prompt::text("Provider display name", Some("OpenAI-compatible"))?;
    let endpoint = endpoint("Base URL", None)?;
    let model_id = required_text("Model id", None)?;
    let model_name = terminal_prompt::text("Model display name", Some(&model_id))?;
    let provider = provider_document(
        &display_name,
        "openai-compatible",
        Some("chat"),
        Some(&endpoint),
        &model_id,
        &model_name,
        None,
    );
    prepared(
        layout,
        provider_id,
        display_name,
        model_id,
        SetupCredential::ApiKey,
        provider,
    )
}

fn prepare_openai_responses(layout: &zuno_paths::Layout) -> Result<PreparedProviderSetup, String> {
    let provider_id = provider_id("Provider id", "openai-responses")?;
    if provider_id == "openai" {
        return Err(
            "the official `openai` id is reserved; choose another id for a custom endpoint"
                .to_owned(),
        );
    }
    let display_name = terminal_prompt::text("Provider display name", Some("OpenAI Responses"))?;
    let endpoint = endpoint("Responses base URL", Some("https://api.openai.com/v1"))?;
    let model_id = required_text("Model id", None)?;
    let model_name = terminal_prompt::text("Model display name", Some(&model_id))?;
    let provider = provider_document(
        &display_name,
        "openai",
        Some("responses"),
        Some(&endpoint),
        &model_id,
        &model_name,
        None,
    );
    prepared(
        layout,
        provider_id,
        display_name,
        model_id,
        SetupCredential::ApiKey,
        provider,
    )
}

fn prepared(
    layout: &zuno_paths::Layout,
    provider_id: String,
    display_name: String,
    model_id: String,
    credential: SetupCredential,
    provider: Value,
) -> Result<PreparedProviderSetup, String> {
    let use_default = terminal_prompt::confirm_choice(&format!(
        "Use {provider_id}/{model_id} as the default model?"
    ))?;
    let change = ConfigChange::prepare(
        layout,
        &provider_id,
        provider,
        use_default.then(|| format!("{provider_id}/{model_id}")),
    )?;
    Ok(PreparedProviderSetup {
        provider_id,
        display_name,
        credential,
        change,
    })
}

fn provider_document(
    display_name: &str,
    transport: &str,
    surface: Option<&str>,
    endpoint: Option<&str>,
    model_id: &str,
    model_name: &str,
    options: Option<Value>,
) -> Value {
    let mut provider = Map::new();
    provider.insert("name".to_owned(), Value::String(display_name.to_owned()));
    provider.insert("transport".to_owned(), Value::String(transport.to_owned()));
    if let Some(surface) = surface {
        provider.insert("surface".to_owned(), Value::String(surface.to_owned()));
    }
    if let Some(endpoint) = endpoint {
        provider.insert("api".to_owned(), Value::String(endpoint.to_owned()));
    }
    let mut options = options
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(endpoint) = endpoint {
        options.insert("baseURL".to_owned(), Value::String(endpoint.to_owned()));
    }
    if !options.is_empty() {
        provider.insert("options".to_owned(), Value::Object(options));
    }
    provider.insert("models".to_owned(), json!({model_id: {"name": model_name}}));
    Value::Object(provider)
}

fn required_text(message: &str, default: Option<&str>) -> Result<String, String> {
    terminal_prompt::text(message, default)
}

fn provider_id(message: &str, default: &str) -> Result<String, String> {
    let provider = terminal_prompt::text(message, Some(default))?;
    if provider == "openai"
        || provider.is_empty()
        || !provider
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(
            "provider id must contain only lowercase ASCII letters, digits, or `-`, and must not be `openai`"
                .to_owned(),
        );
    }
    Ok(provider)
}

fn endpoint(message: &str, default: Option<&str>) -> Result<String, String> {
    let raw = required_text(message, default)?;
    let parsed = Url::parse(&raw).map_err(|error| format!("invalid base URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("base URL must use http or https".to_owned());
    }
    Ok(raw.trim_end_matches('/').to_owned())
}

struct ConfigChange {
    path: PathBuf,
    original: Option<Vec<u8>>,
    updated: Vec<u8>,
}

impl ConfigChange {
    fn prepare(
        layout: &zuno_paths::Layout,
        provider_id: &str,
        provider: Value,
        default_model: Option<String>,
    ) -> Result<Self, String> {
        let path = layout.config().join("zuno.json");
        let original = match fs::read(&path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
        };
        let mut document = match original.as_deref() {
            None | Some([]) => json!({}),
            Some(contents) => serde_json::from_slice::<Value>(contents)
                .map_err(|error| format!("failed to parse {}: {error}", path.display()))?,
        };
        let root = document
            .as_object_mut()
            .ok_or_else(|| format!("{} must contain a JSON object", path.display()))?;
        let providers = root
            .entry("provider")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or_else(|| format!("{}.provider must be an object", path.display()))?;
        if providers.contains_key(provider_id) {
            return Err(format!(
                "provider {provider_id:?} is already configured in {}; edit it explicitly instead of replacing it",
                path.display()
            ));
        }
        providers.insert(provider_id.to_owned(), provider);
        if let Some(default_model) = default_model {
            root.insert("model".to_owned(), Value::String(default_model));
        }
        serde_json::from_value::<zuno_config::Config>(document.clone())
            .map_err(|error| format!("generated provider configuration is invalid: {error}"))?;
        let mut updated = serde_json::to_vec_pretty(&document)
            .map_err(|error| format!("failed to render provider configuration: {error}"))?;
        updated.push(b'\n');
        Ok(Self {
            path,
            original,
            updated,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn commit(&self) -> Result<(), String> {
        zuno_atomic_file::replace(&self.path, &self.updated)
            .map_err(|error| format!("failed to update {}: {error}", self.path.display()))
    }

    fn rollback(&self) -> Result<(), String> {
        match &self.original {
            Some(contents) => zuno_atomic_file::replace(&self.path, contents)
                .map_err(|error| format!("failed to restore {}: {error}", self.path.display())),
            None => match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!("failed to remove {}: {error}", self.path.display())),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_transports_keep_responses_and_compatible_chat_distinct() {
        let responses = provider_document(
            "Responses",
            "openai",
            Some("responses"),
            Some("https://gateway.example/v1"),
            "gpt",
            "GPT",
            None,
        );
        assert_eq!(responses["transport"], "openai");
        assert_eq!(responses["surface"], "responses");
        assert_eq!(responses["api"], "https://gateway.example/v1");
        assert_eq!(
            responses["options"]["baseURL"],
            "https://gateway.example/v1"
        );

        let compatible = provider_document(
            "Compatible",
            "openai-compatible",
            Some("chat"),
            Some("https://gateway.example/v1"),
            "model",
            "Model",
            None,
        );
        assert_eq!(compatible["transport"], "openai-compatible");
        assert_eq!(compatible["surface"], "chat");
        assert_eq!(compatible["api"], "https://gateway.example/v1");
    }

    #[test]
    fn config_change_preserves_unrelated_configuration_and_can_rollback() {
        let root = tempfile::tempdir().expect("root");
        let env = zuno_paths::Env::empty()
            .with("HOME", root.path().join("home").to_string_lossy())
            .with(
                "XDG_CONFIG_HOME",
                root.path().join("config").to_string_lossy(),
            );
        let layout = zuno_paths::Layout::resolve_with(&env, None);
        let path = layout.config().join("zuno.json");
        fs::create_dir_all(path.parent().expect("parent")).expect("config directory");
        fs::write(
            &path,
            br#"{"formatter":false,"lsp":false,"compaction":{"auto":true,"prune":true}}"#,
        )
        .expect("seed config");
        let original = fs::read(&path).expect("original");
        let change = ConfigChange::prepare(
            &layout,
            "custom",
            provider_document(
                "Custom",
                "openai-compatible",
                Some("chat"),
                Some("https://example.test/v1"),
                "model",
                "Model",
                None,
            ),
            Some("custom/model".to_owned()),
        )
        .expect("prepare");

        change.commit().expect("commit");
        let written: Value =
            serde_json::from_slice(&fs::read(&path).expect("written")).expect("json");
        assert_eq!(written["formatter"], false);
        assert_eq!(written["lsp"], false);
        assert_eq!(written["compaction"]["auto"], true);
        assert_eq!(written["compaction"]["prune"], true);
        assert_eq!(written["model"], "custom/model");
        assert_eq!(
            written["provider"]["custom"]["transport"],
            "openai-compatible"
        );

        change.rollback().expect("rollback");
        assert_eq!(fs::read(path).expect("restored"), original);
    }
}
