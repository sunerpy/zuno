wit_bindgen::generate!({
    path: "../../../../wit/zuno-plugin",
    world: "plugin",
});

struct WordCountPlugin;

impl Guest for WordCountPlugin {
    fn initialize(
        _package_id: String,
        _workspace: String,
        _capabilities: Vec<String>,
    ) -> Result<String, String> {
        Ok("zuno.plugin/1".to_owned())
    }

    fn invoke(
        tool: String,
        arguments_json: String,
        _session_id: String,
        _message_id: String,
        _call_id: String,
        _agent: String,
    ) -> Result<(String, String, String), String> {
        if tool != "word_count" {
            return Err(format!("unknown tool `{tool}`"));
        }
        let arguments: serde_json::Value =
            serde_json::from_str(&arguments_json).map_err(|error| error.to_string())?;
        let text = arguments
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "`text` must be a string".to_owned())?;
        let count = text.split_whitespace().count();
        Ok((
            "Word count".to_owned(),
            count.to_string(),
            serde_json::json!({"words": count}).to_string(),
        ))
    }

    fn shutdown() -> Result<(), String> {
        Ok(())
    }
}

export!(WordCountPlugin);
