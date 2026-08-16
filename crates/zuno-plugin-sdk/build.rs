use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

const OPENAPI_CAPTURE: &str = "../../.omo/fixtures/oracle-openapi-1.18.18.json";
const HTTP_METHODS: &[&str] = &[
    "delete", "get", "head", "options", "patch", "post", "put", "trace",
];
const MODEL_PROVIDER_SCHEMAS: &[&str] = &[
    "Model",
    "ModelRef",
    "ModelV2Info",
    "Provider",
    "ProviderV2Info",
];

#[derive(Debug)]
struct Arrival {
    variant: String,
    operation_id: String,
    method: String,
    path: String,
}

fn main() {
    println!("cargo:rerun-if-changed={OPENAPI_CAPTURE}");

    let document = read_openapi(Path::new(OPENAPI_CAPTURE));
    let arrivals = model_provider_arrivals(&document);
    let generated = render(&arrivals);
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo always sets OUT_DIR"))
        .join("generated_client_arrivals.rs");
    fs::write(&output, generated)
        .unwrap_or_else(|error| panic!("could not write {}: {error}", output.display()));
}

fn read_openapi(path: &Path) -> Value {
    let bytes =
        fs::read(path).unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("could not parse {}: {error}", path.display()))
}

fn model_provider_arrivals(document: &Value) -> Vec<Arrival> {
    let schemas = document
        .pointer("/components/schemas")
        .and_then(Value::as_object)
        .expect("the pinned OpenAPI capture has component schemas");
    let paths = document
        .get("paths")
        .and_then(Value::as_object)
        .expect("the pinned OpenAPI capture has paths");
    let mut arrivals = BTreeMap::new();

    for (path, path_item) in paths {
        let path_item = path_item
            .as_object()
            .unwrap_or_else(|| panic!("OpenAPI path {path} is an object"));
        for method in HTTP_METHODS {
            let Some(operation) = path_item.get(*method) else {
                continue;
            };
            let Some(success) = operation.pointer("/responses/200") else {
                continue;
            };
            let reachable = reachable_schemas(success, schemas);
            if !MODEL_PROVIDER_SCHEMAS
                .iter()
                .any(|schema| reachable.contains(*schema))
            {
                continue;
            }

            let operation_id = operation
                .get("operationId")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{method} {path} has an operationId"));
            let arrival = Arrival {
                variant: rust_variant(operation_id),
                operation_id: operation_id.to_owned(),
                method: (*method).to_owned(),
                path: path.to_owned(),
            };
            assert!(
                arrivals.insert(operation_id.to_owned(), arrival).is_none(),
                "duplicate generated SDK operationId {operation_id}"
            );
        }
    }

    assert!(
        !arrivals.is_empty(),
        "the OpenAPI scan found no Model/Provider-bearing generated client arrivals"
    );
    arrivals.into_values().collect()
}

fn reachable_schemas(value: &Value, schemas: &serde_json::Map<String, Value>) -> BTreeSet<String> {
    let mut pending = direct_schema_refs(value).into_iter().collect::<Vec<_>>();
    let mut reachable = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if !reachable.insert(name.clone()) {
            continue;
        }
        if let Some(schema) = schemas.get(&name) {
            pending.extend(
                direct_schema_refs(schema)
                    .into_iter()
                    .filter(|child| !reachable.contains(child)),
            );
        }
    }
    reachable
}

fn direct_schema_refs(value: &Value) -> BTreeSet<String> {
    let mut refs = BTreeSet::new();
    collect_schema_refs(value, &mut refs);
    refs
}

fn collect_schema_refs(value: &Value, refs: &mut BTreeSet<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_schema_refs(value, refs);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                if key == "$ref"
                    && let Some(reference) = value.as_str()
                    && let Some(name) = reference.strip_prefix("#/components/schemas/")
                {
                    refs.insert(name.to_owned());
                } else {
                    collect_schema_refs(value, refs);
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn rust_variant(operation_id: &str) -> String {
    let mut variant = String::new();
    for word in operation_id.split(|character: char| !character.is_ascii_alphanumeric()) {
        let mut characters = word.chars();
        if let Some(first) = characters.next() {
            variant.extend(first.to_uppercase());
            variant.extend(characters);
        }
    }
    assert!(
        !variant.is_empty(),
        "operationId {operation_id:?} has no Rust variant"
    );
    variant
}

fn render(arrivals: &[Arrival]) -> String {
    let mut source = String::from(
        "// @generated by crates/zuno-plugin-sdk/build.rs from the pinned OpenAPI capture.\n\
         // Do not edit this file; regenerate the capture from the pinned release instead.\n\n\
         #[derive(Debug, Clone, Copy, PartialEq, Eq)]\n\
         pub enum GeneratedClientArrival {\n",
    );
    for arrival in arrivals {
        writeln!(
            source,
            "    #[doc = {:?}]\n    {},",
            format!(
                "{} {} (`{}`)",
                arrival.method.to_uppercase(),
                arrival.path,
                arrival.operation_id
            ),
            arrival.variant
        )
        .expect("writing to a String cannot fail");
    }
    source.push_str("}\n\nimpl GeneratedClientArrival {\n    pub const ALL: [Self; ");
    write!(source, "{}", arrivals.len()).expect("writing to a String cannot fail");
    source.push_str("] = [\n");
    for arrival in arrivals {
        writeln!(source, "        Self::{},", arrival.variant)
            .expect("writing to a String cannot fail");
    }
    source.push_str(
        "    ];\n\n    #[must_use]\n    pub const fn method(self) -> &'static str {\n        match self {\n",
    );
    for arrival in arrivals {
        writeln!(
            source,
            "            Self::{} => {:?},",
            arrival.variant, arrival.method
        )
        .expect("writing to a String cannot fail");
    }
    source.push_str(
        "        }\n    }\n\n    #[must_use]\n    pub const fn path(self) -> &'static str {\n        match self {\n",
    );
    for arrival in arrivals {
        writeln!(
            source,
            "            Self::{} => {:?},",
            arrival.variant, arrival.path
        )
        .expect("writing to a String cannot fail");
    }
    source.push_str(
        "        }\n    }\n\n    #[must_use]\n    pub const fn operation_id(self) -> &'static str {\n        match self {\n",
    );
    for arrival in arrivals {
        writeln!(
            source,
            "            Self::{} => {:?},",
            arrival.variant, arrival.operation_id
        )
        .expect("writing to a String cannot fail");
    }
    source.push_str("        }\n    }\n}\n");
    source
}
