//! The rules a well-formed schema still has to meet: names, references between declarations, and what each index may
//! cover. Every problem is reported, each naming who declared what it refuses.

use std::collections::{BTreeMap, BTreeSet};

use super::model::*;
use super::parse::Problem;

pub fn validate(schema: &Schema) -> Vec<Problem> {
    let mut v = Validator { schema, problems: Vec::new() };
    v.collation();
    for (name, role) in &schema.roles {
        let at = format!("roles.{name}");
        v.snake_case(&at, name, "role", role.declared_by.as_deref());
    }
    for (name, relation) in &schema.relations {
        v.relation(name, relation);
    }
    let mut prefixes: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, def) in &schema.types {
        v.entity_type(name, def);
        if let Some(code) = &def.short_code
            && let Some(other) = prefixes.insert(&code.prefix, name)
        {
            v.problem(
                format!("types.{name}.shortCode.prefix"),
                format!("\"{}\" is already {other}'s short code prefix", code.prefix),
                def.declared_by.as_deref(),
            );
        }
    }
    v.problems
}

struct Validator<'a> {
    schema: &'a Schema,
    problems: Vec<Problem>,
}

/// The engine's fields a value index may cover besides the type's own: the times it writes
const INDEXABLE_ENGINE_FIELDS: [&str; 2] = ["createdAt", "updatedAt"];

impl Validator<'_> {
    fn problem(&mut self, path: String, message: String, declared_by: Option<&str>) {
        let message = match declared_by {
            Some(who) => format!("{message} (declared by {who})"),
            None => message,
        };
        self.problems.push(Problem { path, message });
    }

    fn collation(&mut self) {
        let c = &self.schema.collation;
        let locale = c.split('-').enumerate().all(|(i, part)| {
            if i == 0 {
                (2..=3).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_lowercase())
            } else {
                (2..=8).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_alphanumeric())
            }
        });
        if c != "root" && !locale {
            self.problem("collation".into(), format!("\"{c}\" is neither \"root\" nor a locale such as \"en\""), None);
        }
    }

    fn snake_case(&mut self, at: &str, name: &str, what: &str, declared_by: Option<&str>) {
        let mut bytes = name.bytes();
        let ok = bytes.next().is_some_and(|b| b.is_ascii_lowercase())
            && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !ok {
            self.problem(at.into(), format!("a {what}'s name is snake_case, which \"{name}\" isn't"), declared_by);
        }
    }

    fn lower_camel(&mut self, at: &str, name: &str, what: &str, declared_by: Option<&str>) {
        let mut bytes = name.bytes();
        let ok = bytes.next().is_some_and(|b| b.is_ascii_lowercase()) && bytes.all(|b| b.is_ascii_alphanumeric());
        if !ok {
            self.problem(at.into(), format!("a {what}'s name is lowerCamelCase, which \"{name}\" isn't"), declared_by);
        }
    }

    fn type_exists(&mut self, at: String, name: &str, declared_by: Option<&str>) {
        if !self.schema.types.contains_key(name) {
            self.problem(at, format!("no such type \"{name}\""), declared_by);
        }
    }

    fn relation(&mut self, name: &str, relation: &RelationDef) {
        let who = relation.declared_by.as_deref();
        let at = format!("relations.{name}");
        self.snake_case(&at, name, "relation kind", who);
        if let Some(endpoints) = &relation.endpoints {
            if endpoints.is_empty() {
                self.problem(format!("{at}.endpoints"), "lists no type pair; leave it out for any".into(), who);
            }
            for (i, end) in endpoints.iter().enumerate() {
                self.type_exists(format!("{at}.endpoints[{i}].source"), &end.source, who);
                self.type_exists(format!("{at}.endpoints[{i}].target"), &end.target, who);
            }
        }
        self.value_type(&format!("{at}.info"), &relation.info.value, who);
    }

    fn entity_type(&mut self, name: &str, def: &TypeDef) {
        let who = def.declared_by.as_deref();
        let at = format!("types.{name}");
        let mut bytes = name.bytes();
        let pascal = bytes.next().is_some_and(|b| b.is_ascii_uppercase()) && bytes.all(|b| b.is_ascii_alphanumeric());
        if !pascal {
            self.problem(at.clone(), format!("a type's name is PascalCase, which \"{name}\" isn't"), who);
        }
        if RESERVED_TYPES.contains(&name) {
            self.problem(at.clone(), format!("\"{name}\" is a name the engine's own ids use"), who);
        }
        if let Some(code) = &def.short_code {
            let p = &code.prefix;
            let mut bytes = p.bytes();
            let ok = bytes.next().is_some_and(|b| b.is_ascii_uppercase())
                && p.len() <= 8
                && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
            if !ok {
                self.problem(
                    format!("{at}.shortCode.prefix"),
                    format!(
                        "\"{p}\" isn't a short code prefix: up to 8 capital letters and digits, starting with a letter"
                    ),
                    who,
                );
            }
        }
        for (field, field_def) in &def.fields {
            let field_at = format!("{at}.fields.{field}");
            self.lower_camel(&field_at, field, "field", who);
            if RESERVED_FIELDS.contains(&field.as_str()) {
                self.problem(field_at.clone(), format!("\"{field}\" is a field the engine writes itself"), who);
            }
            self.value_type(&field_at, &field_def.value, who);
        }
        for (index, index_def) in &def.indexes {
            self.index(name, def, index, index_def);
        }
    }

    fn value_type(&mut self, at: &str, value: &ValueType, who: Option<&str>) {
        match value {
            ValueType::Enum { values } => {
                if values.is_empty() {
                    self.problem(format!("{at}.values"), "lists no value".into(), who);
                }
                let mut seen = BTreeSet::new();
                for v in values {
                    if !seen.insert(v) {
                        self.problem(format!("{at}.values"), format!("lists \"{v}\" twice"), who);
                    }
                }
            }
            ValueType::Ref { to } => {
                if to.is_empty() {
                    self.problem(format!("{at}.to"), "names no type".into(), who);
                }
                for (i, t) in to.iter().enumerate() {
                    self.type_exists(format!("{at}.to[{i}]"), t, who);
                }
            }
            ValueType::List { of } => self.value_type(&format!("{at}.of"), of, who),
            _ => {}
        }
    }

    fn index(&mut self, type_name: &str, def: &TypeDef, index: &str, index_def: &IndexDef) {
        let who = def.declared_by.as_deref();
        let at = format!("types.{type_name}.indexes.{index}");
        self.lower_camel(&at, index, "index", who);
        let field_of = |name: &str| def.fields.get(name).map(|f| &f.value);
        match index_def {
            IndexDef::Value { fields, .. } => {
                if fields.is_empty() {
                    self.problem(format!("{at}.fields"), "names no field".into(), who);
                }
                let mut seen = BTreeSet::new();
                for (i, field) in fields.iter().enumerate() {
                    let field_at = format!("{at}.fields[{i}]");
                    if !seen.insert(field) {
                        self.problem(field_at, format!("names \"{field}\" twice"), who);
                        continue;
                    }
                    if INDEXABLE_ENGINE_FIELDS.contains(&field.as_str()) {
                        continue;
                    }
                    match field_of(field) {
                        None => self.problem(field_at, format!("no such field on {type_name}"), who),
                        Some(value) if !value.is_value_indexable() => self.problem(
                            field_at,
                            format!("a value index can't hold a {} field (\"{field}\")", value.name()),
                            who,
                        ),
                        Some(_) => {}
                    }
                }
            }
            IndexDef::Text { fields, analyzer, stem } => {
                if fields.is_empty() {
                    self.problem(format!("{at}.fields"), "names no field".into(), who);
                }
                for (field, text) in fields {
                    let field_at = format!("{at}.fields.{field}");
                    match field_of(field) {
                        None => self.problem(field_at.clone(), format!("no such field on {type_name}"), who),
                        Some(value) if !value.is_text() => self.problem(
                            field_at.clone(),
                            format!("a text index reads string fields, and \"{field}\" is a {} field", value.name()),
                            who,
                        ),
                        Some(_) => {}
                    }
                    if !(text.weight.is_finite() && text.weight > 0.0) {
                        self.problem(format!("{field_at}.weight"), "must be a positive number".into(), who);
                    }
                }
                if let Some(language) = stem {
                    if *analyzer != Analyzer::Standard {
                        self.problem(format!("{at}.stem"), "only the standard analyzer stems".into(), who);
                    } else if !STEM_LANGUAGES.contains(&language.as_str()) {
                        self.problem(
                            format!("{at}.stem"),
                            format!("\"{language}\" isn't a stemming language ({})", STEM_LANGUAGES.join(", ")),
                            who,
                        );
                    }
                }
            }
            IndexDef::Vector { from, model, .. } => {
                if from.is_empty() {
                    self.problem(format!("{at}.from"), "names no field".into(), who);
                }
                for (i, field) in from.iter().enumerate() {
                    let field_at = format!("{at}.from[{i}]");
                    match field_of(field) {
                        None => self.problem(field_at, format!("no such field on {type_name}"), who),
                        Some(value) if !value.is_text() => self.problem(
                            field_at,
                            format!("a vector index embeds string fields, and \"{field}\" is a {} field", value.name()),
                            who,
                        ),
                        Some(_) => {}
                    }
                }
                if model.trim().is_empty() {
                    self.problem(format!("{at}.model"), "names no model".into(), who);
                }
            }
        }
    }
}
