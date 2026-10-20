//! The API reference page (`/settings/api`). Built from the same OpenAPI
//! document as `/api/openapi.json`, so the page cannot drift from the
//! handlers: every endpoint, parameter, field and response shown here comes
//! from the `utoipa` annotations in [`super::api`].

use super::api::ApiDoc;
use super::error::status_for;
use crate::domain::TransferState;
use crate::error::ErrorKind;
use serde_json::{Map, Value};
use std::sync::LazyLock;
use utoipa::OpenApi;

/// The reference, built once: the OpenAPI document never changes at runtime.
pub(super) static REFERENCE: LazyLock<ApiReference> = LazyLock::new(|| {
    let doc = serde_json::to_value(ApiDoc::openapi()).unwrap_or_default();
    ApiReference::from_openapi(&doc)
});

/// Text with inline `code` spans, from an OpenAPI description. Rendered by the
/// template span by span, so nothing in it is ever treated as HTML.
pub(super) type Text = Vec<Span>;

pub(super) struct Span {
    pub text: String,
    pub code: bool,
}

pub(super) struct ApiReference {
    pub title: String,
    pub intro: Vec<Text>,
    pub groups: Vec<Group>,
    pub schemas: Vec<Schema>,
    pub states: Vec<StateRow>,
    pub error_kinds: Vec<ErrorKindRow>,
}

/// Endpoints sharing an OpenAPI tag.
pub(super) struct Group {
    pub id: String,
    pub name: String,
    pub description: String,
    pub endpoints: Vec<Endpoint>,
}

pub(super) struct Endpoint {
    pub id: String,
    pub method: String,
    pub path: String,
    pub summary: Text,
    pub details: Vec<Text>,
    pub params: Vec<Field>,
    pub body: Option<Body>,
    pub responses: Vec<Response>,
    pub example: String,
}

impl Endpoint {
    /// CSS class for the method badge.
    pub fn method_class(&self) -> String {
        self.method.to_ascii_lowercase()
    }

    /// The path without the common `/api/v1` prefix, for the contents list.
    pub fn short_path(&self) -> &str {
        self.path.strip_prefix("/api/v1").unwrap_or(&self.path)
    }
}

pub(super) struct Body {
    pub ty: TypeRef,
    pub required: bool,
    pub fields: Vec<Field>,
}

pub(super) struct Response {
    pub status: String,
    pub description: Text,
    pub ty: Option<TypeRef>,
}

impl Response {
    pub fn is_error(&self) -> bool {
        !self.status.starts_with('2')
    }
}

pub(super) struct Field {
    pub name: String,
    /// `path` or `query` for parameters; empty for body fields.
    pub location: String,
    pub ty: TypeRef,
    pub required: bool,
    pub nullable: bool,
    pub description: Text,
    /// Allowed values of an inline enum.
    pub values: Vec<String>,
}

/// A field's type, linking to its schema when it has one.
pub(super) struct TypeRef {
    pub label: String,
    /// Prefix such as "array of" before a linked schema name.
    pub prefix: String,
    pub schema: Option<String>,
}

pub(super) struct Schema {
    pub name: String,
    pub description: Vec<Text>,
    pub fields: Vec<Field>,
    pub values: Vec<String>,
}

pub(super) struct StateRow {
    pub name: String,
    pub meaning: &'static str,
    pub next: Vec<String>,
}

pub(super) struct ErrorKindRow {
    pub name: String,
    pub status: u16,
    pub meaning: &'static str,
}

impl ApiReference {
    fn from_openapi(doc: &Value) -> Self {
        let schemas = doc
            .pointer("/components/schemas")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        let mut groups: Vec<Group> = doc["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|tag| {
                let name = str_of(&tag["name"]);
                Group {
                    id: format!("tag-{name}"),
                    description: str_of(&tag["description"]),
                    name,
                    endpoints: Vec::new(),
                }
            })
            .collect();

        for (path, item) in doc["paths"].as_object().into_iter().flatten() {
            for method in ["get", "post", "put", "patch", "delete"] {
                let Some(op) = item.get(method) else { continue };
                let endpoint = endpoint(method, path, op, &schemas);
                let tag = str_of(&op["tags"][0]);
                match groups.iter_mut().find(|g| g.name == tag) {
                    Some(group) => group.endpoints.push(endpoint),
                    None => groups.push(Group {
                        id: format!("tag-{tag}"),
                        name: tag,
                        description: String::new(),
                        endpoints: vec![endpoint],
                    }),
                }
            }
        }

        Self {
            title: str_of(&doc["info"]["title"]),
            intro: paragraphs(&str_of(&doc["info"]["description"])),
            groups,
            schemas: schemas
                .iter()
                .map(|(name, schema)| Schema {
                    name: name.clone(),
                    description: paragraphs(&str_of(&schema["description"])),
                    fields: fields(schema, &schemas),
                    values: enum_values(schema),
                })
                .collect(),
            states: TransferState::ALL
                .iter()
                .map(|&state| StateRow {
                    name: state.as_str().to_owned(),
                    meaning: state_meaning(state),
                    next: TransferState::ALL
                        .iter()
                        .filter(|&&next| state.can_transition_to(next))
                        .map(|next| next.as_str().to_owned())
                        .collect(),
                })
                .collect(),
            error_kinds: enum_values(&schemas["ErrorKind"])
                .into_iter()
                .filter_map(|name| {
                    let kind: ErrorKind = serde_json::from_value(Value::String(name)).ok()?;
                    Some(ErrorKindRow {
                        name: kind.as_str().to_owned(),
                        status: status_for(kind).as_u16(),
                        meaning: error_meaning(kind),
                    })
                })
                .collect(),
        }
    }
}

fn endpoint(method: &str, path: &str, op: &Value, schemas: &Map<String, Value>) -> Endpoint {
    // utoipa puts the first doc paragraph in `summary` and the rest in
    // `description`.
    let mut text = paragraphs(&str_of(&op["summary"]));
    text.extend(paragraphs(&str_of(&op["description"])));
    let summary = if text.is_empty() {
        Text::new()
    } else {
        text.remove(0)
    };

    let params: Vec<Field> = op["parameters"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            let (ty, nullable) = type_of(&p["schema"]);
            Field {
                name: str_of(&p["name"]),
                location: str_of(&p["in"]),
                ty,
                required: p["required"].as_bool().unwrap_or(false),
                nullable,
                description: inline(&str_of(&p["description"])),
                values: enum_values(&p["schema"]),
            }
        })
        .collect();

    let body = op
        .pointer("/requestBody/content/application~1json/schema")
        .map(|schema| {
            let resolved = resolve(schema, schemas);
            Body {
                ty: type_of(schema).0,
                required: op["requestBody"]["required"].as_bool().unwrap_or(false),
                fields: fields(resolved, schemas),
            }
        });

    let responses = op["responses"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(status, r)| Response {
            status: status.clone(),
            description: match str_of(&r["description"]) {
                d if d.is_empty() && status.starts_with('2') => inline("Success"),
                d => inline(&d),
            },
            ty: r
                .pointer("/content/application~1json/schema")
                .map(|s| type_of(s).0),
        })
        .collect();

    Endpoint {
        id: format!("op-{}", str_of(&op["operationId"])),
        method: method.to_ascii_uppercase(),
        path: path.to_owned(),
        summary,
        details: text,
        example: curl_example(method, path, &params, body.as_ref(), schemas),
        params,
        body,
        responses,
    }
}

/// The fields of an object schema, required ones first.
fn fields(schema: &Value, schemas: &Map<String, Value>) -> Vec<Field> {
    let required: Vec<&str> = schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut out: Vec<Field> = schema["properties"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, prop)| {
            let (ty, nullable) = type_of(prop);
            Field {
                name: name.clone(),
                location: String::new(),
                ty,
                required: required.contains(&name.as_str()),
                nullable,
                description: inline(&description_of(prop)),
                values: enum_values(resolve(prop, schemas)),
            }
        })
        .collect();
    // `#[serde(flatten)]` fields arrive as `allOf` parts.
    for part in schema["allOf"].as_array().into_iter().flatten() {
        out.extend(fields(resolve(part, schemas), schemas));
    }
    out.sort_by_key(|f| !f.required);
    out
}

/// A property's type, and whether it may be null.
fn type_of(schema: &Value) -> (TypeRef, bool) {
    if let Some(r) = schema["$ref"].as_str() {
        return (TypeRef::schema("", ref_name(r)), false);
    }
    // `Option<Schema>` is written as `oneOf: [{$ref}, {type: null}]`.
    if let Some(variants) = schema["oneOf"].as_array() {
        let nullable = variants.iter().any(|v| v["type"] == "null");
        let mut real = variants.iter().filter(|v| v["type"] != "null");
        if let (Some(only), None) = (real.next(), real.next()) {
            return (type_of(only).0, nullable);
        }
    }
    if let Some(parts) = schema["allOf"].as_array()
        && let [only] = parts.as_slice()
    {
        return type_of(only);
    }

    let (name, nullable) = match &schema["type"] {
        Value::String(t) => (t.as_str(), false),
        Value::Array(ts) => (
            ts.iter()
                .filter_map(Value::as_str)
                .find(|t| *t != "null")
                .unwrap_or("any"),
            ts.iter().any(|t| t == "null"),
        ),
        _ => ("any", false),
    };
    let ty = match name {
        "array" => {
            let inner = type_of(&schema["items"]).0;
            match inner.schema {
                Some(s) => TypeRef::schema("array of ", &s),
                None => TypeRef::plain(format!("array of {}", inner.label)),
            }
        }
        "string" => match schema["format"].as_str() {
            Some(f @ ("uuid" | "date-time")) => TypeRef::plain(format!("string ({f})")),
            _ => TypeRef::plain("string"),
        },
        other => TypeRef::plain(other),
    };
    (ty, nullable)
}

impl TypeRef {
    fn plain(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            prefix: String::new(),
            schema: None,
        }
    }

    fn schema(prefix: &str, name: &str) -> Self {
        Self {
            label: name.to_owned(),
            prefix: prefix.to_owned(),
            schema: Some(name.to_owned()),
        }
    }
}

/// A property's description, which utoipa places inside `oneOf` for
/// optional references.
fn description_of(prop: &Value) -> String {
    if let Some(d) = prop["description"].as_str() {
        return d.to_owned();
    }
    prop["oneOf"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|v| v["description"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// The schema a `$ref` (possibly wrapped in `oneOf` with null) points to.
fn resolve<'a>(schema: &'a Value, schemas: &'a Map<String, Value>) -> &'a Value {
    let reference = schema["$ref"].as_str().or_else(|| {
        schema["oneOf"]
            .as_array()?
            .iter()
            .find_map(|v| v["$ref"].as_str())
    });
    reference
        .and_then(|r| schemas.get(ref_name(r)))
        .unwrap_or(schema)
}

fn ref_name(reference: &str) -> &str {
    reference.rsplit('/').next().unwrap_or(reference)
}

fn enum_values(schema: &Value) -> Vec<String> {
    schema["enum"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

fn str_of(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// Split a doc comment into paragraphs, joining wrapped lines.
fn paragraphs(text: &str) -> Vec<Text> {
    text.split("\n\n")
        .map(inline)
        .filter(|p| !p.is_empty())
        .collect()
}

/// Parse `code` spans. Rustdoc links (`` [`Name`] ``) lose their brackets.
fn inline(text: &str) -> Text {
    let text = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("[`", "`")
        .replace("`]", "`");
    let mut out = Text::new();
    for (i, part) in text.split('`').enumerate() {
        if !part.is_empty() {
            out.push(Span {
                text: part.to_owned(),
                code: i % 2 == 1,
            });
        }
    }
    out
}

/// A `curl` command for the endpoint, using the `$DIREKT` and `$TOKEN` shell
/// variables the page sets up. The body holds the required fields and those
/// with an example value.
fn curl_example(
    method: &str,
    path: &str,
    params: &[Field],
    body: Option<&Body>,
    schemas: &Map<String, Value>,
) -> String {
    let mut url = path.to_owned();
    for p in params.iter().filter(|p| p.location == "path") {
        url = url.replace(
            &format!("{{{}}}", p.name),
            &format!("${}", p.name.to_uppercase()),
        );
    }
    let mut cmd = String::from("curl");
    if method != "get" {
        cmd.push_str(&format!(" -X {}", method.to_ascii_uppercase()));
    }
    cmd.push_str(&format!(
        " \"$DIREKT{url}\" \\\n  -H \"Authorization: Bearer $TOKEN\""
    ));

    let json = body.and_then(|b| {
        let props = b.ty.schema.as_deref().and_then(|name| schemas.get(name))?;
        let mut example = Map::new();
        for field in &b.fields {
            let prop = &props["properties"][&field.name];
            let value =
                example_of(prop).or_else(|| field.required.then(|| placeholder(field)).flatten());
            if let Some(value) = value {
                example.insert(field.name.clone(), value);
            }
        }
        (!example.is_empty() || b.required).then_some(Value::Object(example))
    });
    if let Some(json) = json {
        let json = serde_json::to_string(&json).unwrap_or_default();
        cmd.push_str(" \\\n  -H \"Content-Type: application/json\"");
        cmd.push_str(&format!(" \\\n  -d '{}'", json.replace('\'', "'\\''")));
    }
    cmd
}

fn example_of(prop: &Value) -> Option<Value> {
    if let Some(v) = prop.get("example") {
        return Some(v.clone());
    }
    let from_list = |p: &Value| p["examples"].as_array().and_then(|e| e.first()).cloned();
    from_list(prop).or_else(|| {
        prop["oneOf"]
            .as_array()?
            .iter()
            .find_map(|v| v.get("example").cloned().or_else(|| from_list(v)))
    })
}

fn placeholder(field: &Field) -> Option<Value> {
    if let Some(first) = field.values.first() {
        return Some(Value::String(first.clone()));
    }
    Some(match field.ty.label.as_str() {
        "boolean" => Value::Bool(true),
        "integer" | "number" => Value::from(1),
        l if l.starts_with("string") => Value::String(format!("<{}>", field.name)),
        _ => return None,
    })
}

fn state_meaning(state: TransferState) -> &'static str {
    match state {
        TransferState::Queued => "Waiting for a free download slot.",
        TransferState::Downloading => "Transferring bytes.",
        TransferState::Retrying => "A temporary failure; tried again at next_attempt_at.",
        TransferState::Paused => "Paused by the user. Partial data is kept.",
        TransferState::Blocked => "Needs the user first, e.g. credentials or a password.",
        TransferState::Completed => "The file is complete. Organizing is tracked in post_state.",
        TransferState::Failed => "A permanent failure, or retries ran out.",
        TransferState::Cancelled => "Cancelled by the user; partial data deleted.",
    }
}

fn error_meaning(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidRequest => "The request or URL is malformed or not permitted.",
        ErrorKind::UnsupportedProvider => "No provider can handle the URL.",
        ErrorKind::AuthenticationFailed => "A file host rejected or lacked credentials.",
        ErrorKind::ProviderRestricted => {
            "A file host refuses for now: rate limit, wait time, quota."
        }
        ErrorKind::Network => "Connection, DNS, TLS or timeout failure.",
        ErrorKind::RemoteUnavailable => "The remote file is gone or does not exist.",
        ErrorKind::InsufficientDiskSpace => "Not enough free disk space.",
        ErrorKind::Filesystem => "A local file operation failed.",
        ErrorKind::Persistence => "The database failed.",
        ErrorKind::MediaIdentification => "The file could not be identified or organized.",
        ErrorKind::Notification => "A notification could not be delivered.",
        ErrorKind::Unauthorized => "Missing or invalid token or session.",
        ErrorKind::Forbidden => "Not allowed, e.g. a cross-site request.",
        ErrorKind::RateLimited => "Too many attempts; wait before trying again.",
        ErrorKind::NotFound => "No such download, preview, token or endpoint.",
        ErrorKind::Conflict => "Not possible in the item's current state.",
        ErrorKind::Internal => "An unexpected failure; see the server log.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_is_documented() {
        let reference = &*REFERENCE;
        let count: usize = reference.groups.iter().map(|g| g.endpoints.len()).sum();
        assert!(count >= 30, "only {count} endpoints");
        for group in &reference.groups {
            for e in &group.endpoints {
                assert!(
                    !e.summary.is_empty(),
                    "{} {} has no summary",
                    e.method,
                    e.path
                );
                assert!(
                    !e.responses.is_empty(),
                    "{} {} has no responses",
                    e.method,
                    e.path
                );
                for name in e
                    .path
                    .split('{')
                    .skip(1)
                    .filter_map(|s| s.split('}').next())
                {
                    assert!(
                        e.params
                            .iter()
                            .any(|p| p.name == name && p.location == "path"),
                        "{} {} does not document {{{name}}}",
                        e.method,
                        e.path
                    );
                }
            }
        }
    }

    #[test]
    fn schema_links_resolve() {
        let reference = &*REFERENCE;
        let known: Vec<&str> = reference.schemas.iter().map(|s| s.name.as_str()).collect();
        let types = reference
            .groups
            .iter()
            .flat_map(|g| &g.endpoints)
            .flat_map(|e| {
                e.params
                    .iter()
                    .chain(e.body.iter().flat_map(|b| &b.fields))
                    .map(|f| &f.ty)
                    .chain(e.body.iter().map(|b| &b.ty))
                    .chain(e.responses.iter().filter_map(|r| r.ty.as_ref()))
            })
            .chain(
                reference
                    .schemas
                    .iter()
                    .flat_map(|s| s.fields.iter().map(|f| &f.ty)),
            );
        for ty in types {
            if let Some(name) = &ty.schema {
                assert!(known.contains(&name.as_str()), "unknown schema {name}");
            }
        }
    }

    #[test]
    fn optional_references_keep_their_description_and_nullability() {
        let dto = REFERENCE
            .schemas
            .iter()
            .find(|s| s.name == "DownloadDto")
            .unwrap();
        let error = dto.fields.iter().find(|f| f.name == "error").unwrap();
        assert!(error.nullable && !error.required);
        assert_eq!(error.ty.schema.as_deref(), Some("DownloadError"));
        assert!(!error.description.is_empty());
    }

    #[test]
    fn code_spans_are_parsed() {
        let text = inline("Use [`MiscSettings`] or\n`POST /x`.");
        let rendered: Vec<(bool, &str)> = text.iter().map(|s| (s.code, s.text.as_str())).collect();
        assert_eq!(
            rendered,
            [
                (false, "Use "),
                (true, "MiscSettings"),
                (false, " or "),
                (true, "POST /x"),
                (false, ".")
            ]
        );
    }

    #[test]
    fn examples_fill_request_bodies() {
        let create = REFERENCE
            .groups
            .iter()
            .flat_map(|g| &g.endpoints)
            .find(|e| e.id == "op-create_download")
            .unwrap();
        assert!(
            create
                .example
                .starts_with("curl -X POST \"$DIREKT/api/v1/downloads\"")
        );
        assert!(
            create
                .example
                .contains(r#"-d '{"url":"https://example.com/files/movie.mkv"}'"#)
        );

        let get = REFERENCE
            .groups
            .iter()
            .flat_map(|g| &g.endpoints)
            .find(|e| e.id == "op-get_download")
            .unwrap();
        assert_eq!(
            get.example,
            "curl \"$DIREKT/api/v1/downloads/$ID\" \\\n  -H \"Authorization: Bearer $TOKEN\""
        );
    }

    #[test]
    fn error_kinds_and_states_are_listed() {
        assert_eq!(REFERENCE.states.len(), TransferState::ALL.len());
        let not_found = REFERENCE
            .error_kinds
            .iter()
            .find(|k| k.name == "not_found")
            .unwrap();
        assert_eq!(not_found.status, 404);
    }
}
