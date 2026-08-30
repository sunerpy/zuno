//! Print the canonical `zuno.json` schema.

fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&zuno_config::json_schema::document())
            .expect("the generated schema is serializable")
    );
}
