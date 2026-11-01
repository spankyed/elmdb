//! Reading a schema from JSON: every problem found, by its path, before any is reported (`docs/architecture/schema.md`, Errors).
//!
//! The JSON is walked by hand rather than through serde's derive, because a derive stops at the first problem and can't
//! refuse unknown keys beside a flattened field.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use super::model::*;

/// One problem with a schema: where it is, and what is wrong
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Reads a schema, or every problem with it
pub fn parse(json: &Value) -> Result<Schema, Vec<Problem>> {
    let mut reader = Reader::default();
    let schema = reader.schema(json);
    let mut problems = reader.problems;
    if let Some(schema) = &schema {
        problems.extend(super::validate::validate(schema));
    }
    match schema {
        Some(schema) if problems.is_empty() => Ok(schema),
        _ => Err(problems),
    }
}

#[derive(Default)]
struct Reader {
    problems: Vec<Problem>,
}

/// An object being read: the keys taken from it, so the rest are reported as unknown
struct Object<'a> {
    map: &'a Map<String, Value>,
    path: String,
    taken: BTreeSet<&'static str>,
}

impl<'a> Object<'a> {
    fn get(&mut self, key: &'static str) -> Option<&'a Value> {
        self.taken.insert(key);
        self.map.get(key)
    }

    fn at(&self, key: &str) -> String {
        join(&self.path, key)
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() { key.to_owned() } else { format!("{path}.{key}") }
}

impl Reader {
    fn problem(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.problems.push(Problem { path: path.into(), message: message.into() });
    }

    fn object<'a>(&mut self, value: &'a Value, path: &str) -> Option<Object<'a>> {
        match value.as_object() {
            Some(map) => Some(Object { map, path: path.to_owned(), taken: BTreeSet::new() }),
            None => {
                self.problem(display(path), format!("must be an object, not {}", kind_of(value)));
                None
            }
        }
    }

    /// Reports every key of `object` nothing took
    fn finish(&mut self, object: Object<'_>) {
        for key in object.map.keys() {
            if !object.taken.contains(key.as_str()) {
                self.problem(object.at(key), "unknown key");
            }
        }
    }

    fn string(&mut self, object: &mut Object<'_>, key: &'static str) -> Option<String> {
        match object.get(key) {
            None => {
                self.problem(object.at(key), "missing");
                None
            }
            Some(value) => self.as_string(value, &object.at(key)),
        }
    }

    fn as_string(&mut self, value: &Value, path: &str) -> Option<String> {
        match value.as_str() {
            Some(s) => Some(s.to_owned()),
            None => {
                self.problem(path, format!("must be a string, not {}", kind_of(value)));
                None
            }
        }
    }

    fn optional_string(&mut self, object: &mut Object<'_>, key: &'static str) -> Option<Option<String>> {
        match object.get(key) {
            None | Some(Value::Null) => Some(None),
            Some(value) => self.as_string(value, &object.at(key)).map(Some),
        }
    }

    fn bool_or(&mut self, object: &mut Object<'_>, key: &'static str, default: bool) -> Option<bool> {
        match object.get(key) {
            None => Some(default),
            Some(Value::Bool(b)) => Some(*b),
            Some(value) => {
                self.problem(object.at(key), format!("must be true or false, not {}", kind_of(value)));
                None
            }
        }
    }

    /// One of `names`, by its JSON name
    fn choice<T: Copy>(
        &mut self,
        object: &mut Object<'_>,
        key: &'static str,
        default: Option<T>,
        names: &[(&str, T)],
    ) -> Option<T> {
        let Some(value) = object.get(key) else {
            if default.is_none() {
                self.problem(object.at(key), "missing");
            }
            return default;
        };
        let path = object.at(key);
        let name = self.as_string(value, &path)?;
        match names.iter().find(|(n, _)| *n == name) {
            Some((_, t)) => Some(*t),
            None => {
                let all: Vec<String> = names.iter().map(|(n, _)| format!("\"{n}\"")).collect();
                self.problem(path, format!("\"{name}\" isn't one of {}", all.join(", ")));
                None
            }
        }
    }

    fn strings(&mut self, value: &Value, path: &str) -> Option<Vec<String>> {
        let Some(items) = value.as_array() else {
            self.problem(path, format!("must be a list of strings, not {}", kind_of(value)));
            return None;
        };
        let mut out = Vec::new();
        let mut ok = true;
        for (i, item) in items.iter().enumerate() {
            match self.as_string(item, &format!("{path}[{i}]")) {
                Some(s) => out.push(s),
                None => ok = false,
            }
        }
        ok.then_some(out)
    }

    /// A map of named objects, each read by `read`; a name whose entry fails is left out
    fn named<T>(
        &mut self,
        object: &mut Object<'_>,
        key: &'static str,
        mut read: impl FnMut(&mut Self, &str, &Value, &str) -> Option<T>,
    ) -> Option<BTreeMap<String, T>> {
        let path = object.at(key);
        let Some(value) = object.get(key) else { return Some(BTreeMap::new()) };
        let Some(map) = value.as_object() else {
            self.problem(path, format!("must be an object, not {}", kind_of(value)));
            return None;
        };
        let mut out = BTreeMap::new();
        let mut ok = true;
        for (name, entry) in map {
            match read(self, name, entry, &join(&path, name)) {
                Some(t) => {
                    out.insert(name.clone(), t);
                }
                None => ok = false,
            }
        }
        ok.then_some(out)
    }

    fn schema(&mut self, json: &Value) -> Option<Schema> {
        let mut root = self.object(json, "")?;
        let language = self.string(&mut root, "$schema");
        if let Some(language) = &language
            && language != SCHEMA_LANGUAGE
        {
            self.problem(
                "$schema",
                format!("\"{language}\" isn't a schema language this version reads ({SCHEMA_LANGUAGE})"),
            );
        }
        let collation = match root.get("collation") {
            None => Some("root".to_owned()),
            Some(value) => self.as_string(value, "collation"),
        };
        let roles = self.named(&mut root, "roles", |r, _, v, path| r.role(v, path));
        let relations = self.named(&mut root, "relations", |r, _, v, path| r.relation(v, path));
        let types = self.named(&mut root, "types", |r, _, v, path| r.entity_type(v, path));
        let ranks = self.named(&mut root, "ranks", |r, _, v, path| r.rank(v, path));
        self.finish(root);
        Some(Schema {
            language: language?,
            collation: collation?,
            roles: roles?,
            relations: relations?,
            types: types?,
            ranks: ranks?,
        })
    }

    fn role(&mut self, value: &Value, path: &str) -> Option<RoleDef> {
        let mut object = self.object(value, path)?;
        let declared_by = self.optional_string(&mut object, "declaredBy");
        self.finish(object);
        Some(RoleDef { declared_by: declared_by? })
    }

    fn relation(&mut self, value: &Value, path: &str) -> Option<RelationDef> {
        let mut object = self.object(value, path)?;
        let declared_by = self.optional_string(&mut object, "declaredBy");
        let cardinality = self.choice(
            &mut object,
            "cardinality",
            None,
            &[
                ("one-to-one", Cardinality::OneToOne),
                ("one-to-many", Cardinality::OneToMany),
                ("many-to-one", Cardinality::ManyToOne),
                ("many-to-many", Cardinality::ManyToMany),
            ],
        );
        let acyclic = self.bool_or(&mut object, "acyclic", false);
        let key = match object.get("key") {
            None => Some(Vec::new()),
            Some(value) => self.strings(value, &object.at("key")),
        };
        let weighted = self.bool_or(&mut object, "weighted", false);
        let endpoints = match object.get("endpoints") {
            None | Some(Value::Null) => Some(None),
            Some(value) => self.endpoints(value, &object.at("endpoints")).map(Some),
        };
        let info = match object.get("info") {
            None => Some(any_info()),
            Some(value) => self.field(value, &object.at("info")),
        };
        self.finish(object);
        Some(RelationDef {
            declared_by: declared_by?,
            cardinality: cardinality?,
            acyclic: acyclic?,
            key: key?,
            weighted: weighted?,
            endpoints: endpoints?,
            info: info?,
        })
    }

    fn rank(&mut self, value: &Value, path: &str) -> Option<RankDef> {
        let mut object = self.object(value, path)?;
        let declared_by = self.optional_string(&mut object, "declaredBy");
        let types = self.required_strings(&mut object, "types");
        let relations = self.required_strings(&mut object, "relations");
        let direction = self.choice(
            &mut object,
            "direction",
            Some(Direction::Out),
            &[("out", Direction::Out), ("in", Direction::In), ("both", Direction::Both)],
        );
        let weights = self.bool_or(&mut object, "weights", true);
        let damping = self.number_or(&mut object, "damping", 0.85);
        let tolerance = self.number_or(&mut object, "tolerance", 1e-6);
        self.finish(object);
        Some(RankDef {
            declared_by: declared_by?,
            types: types?,
            relations: relations?,
            direction: direction?,
            weights: weights?,
            damping: damping?,
            tolerance: tolerance?,
        })
    }

    fn number_or(&mut self, object: &mut Object<'_>, key: &'static str, default: f64) -> Option<f64> {
        match object.get(key) {
            None => Some(default),
            Some(value) => match value.as_f64() {
                Some(n) => Some(n),
                None => {
                    self.problem(object.at(key), format!("must be a number, not {}", kind_of(value)));
                    None
                }
            },
        }
    }

    fn endpoints(&mut self, value: &Value, path: &str) -> Option<Vec<Endpoint>> {
        let Some(items) = value.as_array() else {
            self.problem(path, format!("must be a list, not {}", kind_of(value)));
            return None;
        };
        let mut out = Vec::new();
        let mut ok = true;
        for (i, item) in items.iter().enumerate() {
            let at = format!("{path}[{i}]");
            let Some(mut object) = self.object(item, &at) else {
                ok = false;
                continue;
            };
            let source = self.string(&mut object, "source");
            let target = self.string(&mut object, "target");
            self.finish(object);
            match (source, target) {
                (Some(source), Some(target)) => out.push(Endpoint { source, target }),
                _ => ok = false,
            }
        }
        ok.then_some(out)
    }

    fn entity_type(&mut self, value: &Value, path: &str) -> Option<TypeDef> {
        let mut object = self.object(value, path)?;
        let declared_by = self.optional_string(&mut object, "declaredBy");
        let short_code = match object.get("shortCode") {
            None | Some(Value::Null) => Some(None),
            Some(value) => {
                let at = object.at("shortCode");
                self.object(value, &at).and_then(|mut code| {
                    let prefix = self.string(&mut code, "prefix");
                    self.finish(code);
                    prefix.map(|prefix| Some(ShortCodeDef { prefix }))
                })
            }
        };
        let fields = self.named(&mut object, "fields", |r, _, v, path| r.field(v, path));
        let additional_fields = self.choice(
            &mut object,
            "additionalFields",
            Some(AdditionalFields::Refuse),
            &[("refuse", AdditionalFields::Refuse), ("allow", AdditionalFields::Allow)],
        );
        let indexes = self.named(&mut object, "indexes", |r, _, v, path| r.index(v, path));
        self.finish(object);
        Some(TypeDef {
            declared_by: declared_by?,
            short_code: short_code?,
            fields: fields?,
            additional_fields: additional_fields?,
            indexes: indexes?,
        })
    }

    fn field(&mut self, value: &Value, path: &str) -> Option<FieldDef> {
        let mut object = self.object(value, path)?;
        let value_type = self.value_type(&mut object);
        let optional = self.bool_or(&mut object, "optional", false);
        let nullable = self.bool_or(&mut object, "nullable", false);
        self.finish(object);
        Some(FieldDef { value: value_type?, optional: optional?, nullable: nullable? })
    }

    /// A `type` and its options, read from the object that holds them
    fn value_type(&mut self, object: &mut Object<'_>) -> Option<ValueType> {
        let name = self.string(object, "type")?;
        Some(match name.as_str() {
            "string" => ValueType::String,
            "number" => ValueType::Number,
            "integer" => ValueType::Integer,
            "boolean" => ValueType::Boolean,
            "bytes" => ValueType::Bytes,
            "object" => ValueType::Object,
            "any" => ValueType::Any,
            "enum" => {
                let values = match object.get("values") {
                    None => {
                        self.problem(object.at("values"), "missing");
                        None
                    }
                    Some(v) => self.strings(v, &object.at("values")),
                }?;
                ValueType::Enum { values }
            }
            "ref" => {
                let to = match object.get("to") {
                    None => {
                        self.problem(object.at("to"), "missing");
                        None
                    }
                    Some(v) => self.strings(v, &object.at("to")),
                }?;
                ValueType::Ref { to }
            }
            "list" => {
                let at = object.at("of");
                let of = match object.get("of") {
                    None => {
                        self.problem(at, "missing");
                        None
                    }
                    Some(v) => self.object(v, &at).and_then(|mut element| {
                        let element_type = self.value_type(&mut element);
                        self.finish(element);
                        element_type
                    }),
                }?;
                ValueType::List { of: Box::new(of) }
            }
            other => {
                self.problem(
                    object.at("type"),
                    format!(
                        "\"{other}\" isn't a field type (string, number, integer, boolean, bytes, enum, ref, list, object, any)"
                    ),
                );
                return None;
            }
        })
    }

    fn index(&mut self, value: &Value, path: &str) -> Option<IndexDef> {
        let mut object = self.object(value, path)?;
        let kind = self.string(&mut object, "kind")?;
        let index = match kind.as_str() {
            "value" => {
                let fields = self.required_strings(&mut object, "fields");
                let unique = self.bool_or(&mut object, "unique", false);
                Some(IndexDef::Value { fields: fields?, unique: unique? })
            }
            "text" => {
                let fields = self.text_fields(&mut object);
                let analyzer = self.choice(
                    &mut object,
                    "analyzer",
                    Some(Analyzer::Standard),
                    &[("standard", Analyzer::Standard), ("exact", Analyzer::Exact)],
                );
                let stem = self.optional_string(&mut object, "stem");
                Some(IndexDef::Text { fields: fields?, analyzer: analyzer?, stem: stem? })
            }
            "trigram" => {
                let fields = self.required_strings(&mut object, "fields");
                Some(IndexDef::Trigram { fields: fields? })
            }
            "vector" => {
                let from = self.required_strings(&mut object, "from");
                let chunk = self.choice(
                    &mut object,
                    "chunk",
                    Some(Chunk::Whole),
                    &[("whole", Chunk::Whole), ("paragraph", Chunk::Paragraph), ("heading", Chunk::Heading)],
                );
                let dims = self.dims(&mut object);
                let metric = self.choice(
                    &mut object,
                    "metric",
                    Some(Metric::Cosine),
                    &[("cosine", Metric::Cosine), ("dot", Metric::Dot), ("l2", Metric::L2)],
                );
                let precision = self.choice(
                    &mut object,
                    "precision",
                    Some(Precision::F32),
                    &[("f32", Precision::F32), ("f16", Precision::F16), ("i8", Precision::I8)],
                );
                let model = self.string(&mut object, "model");
                Some(IndexDef::Vector {
                    from: from?,
                    chunk: chunk?,
                    dims: dims?,
                    metric: metric?,
                    precision: precision?,
                    model: model?,
                })
            }
            other => {
                self.problem(
                    object.at("kind"),
                    format!("\"{other}\" isn't an index kind (value, text, trigram, vector)"),
                );
                None
            }
        };
        self.finish(object);
        index
    }

    fn required_strings(&mut self, object: &mut Object<'_>, key: &'static str) -> Option<Vec<String>> {
        match object.get(key) {
            None => {
                self.problem(object.at(key), "missing");
                None
            }
            Some(value) => self.strings(value, &object.at(key)),
        }
    }

    fn text_fields(&mut self, object: &mut Object<'_>) -> Option<BTreeMap<String, TextField>> {
        if object.map.get("fields").is_none() {
            self.problem(object.at("fields"), "missing");
            object.get("fields");
            return None;
        }
        self.named(object, "fields", |r, _, v, path| {
            let mut field = r.object(v, path)?;
            let weight = match field.get("weight") {
                None => Some(1.0),
                Some(w) => match w.as_f64() {
                    Some(w) => Some(w),
                    None => {
                        r.problem(field.at("weight"), format!("must be a number, not {}", kind_of(w)));
                        None
                    }
                },
            };
            r.finish(field);
            weight.map(|weight| TextField { weight })
        })
    }

    fn dims(&mut self, object: &mut Object<'_>) -> Option<u32> {
        let path = object.at("dims");
        match object.get("dims") {
            None => {
                self.problem(path, "missing");
                None
            }
            Some(value) => match value.as_u64() {
                Some(n) if (1..=u64::from(MAX_DIMS)).contains(&n) => Some(n as u32),
                _ => {
                    self.problem(path, format!("must be a whole number from 1 to {MAX_DIMS}, not {value}"));
                    None
                }
            },
        }
    }
}

fn display(path: &str) -> &str {
    if path.is_empty() { "(the schema)" } else { path }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}
