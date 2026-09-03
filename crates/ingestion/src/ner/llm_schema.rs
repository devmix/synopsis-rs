//! JSON schema generation for the LLM NER provider (design D5, task 2.3).
//!
//! The generated schema constrains the model's structured output to the
//! domain's entities/relations shape; the `llm` client embeds it under
//! `response_format.json_schema` (task 2.5 wires the call).
//!
//! # Design decisions
//!
//! - The mode flag lives at the call site. `LlmClient` exposes the mode as
//!   `ResponseFormat` (`config::preset`) and itself falls back to
//!   `json_object` when the schema is absent or empty (task 2.5 passes
//!   `None` there), so this function is a pure function of the domain config
//!   and always produces a schema.
//! - `serde_json` pretty-prints with a two-space indent and no HTML
//!   escaping. The schema contains no HTML characters, and the client parses
//!   the document back to JSON before embedding it, so the indent width is
//!   cosmetic.
//! - The `entity_schema`/`relation_schema` builders are `pub(crate)` because
//!   the tests exercise them directly.

use config::DomainConfig;
use config::ontology::AttributeType;
use serde_json::{Map, Value, json};

/// Generates the JSON schema for one domain's NER structured output.
///
/// The top level is an object with an `entities` array (always present and
/// required) and, when the domain defines relations, a `relations` array.
/// The result is a pretty-printed JSON document (two-space indent — see the
/// module docs); serialization of these literal values cannot fail, so the
/// `unwrap_or_default` fallback never fires.
#[must_use]
pub fn generate_json_schema(domain: &DomainConfig) -> String {
    let mut properties = Map::new();
    properties.insert("entities".to_owned(), entity_schema(domain));
    if !domain.relations.is_empty() {
        properties.insert("relations".to_owned(), relation_schema(domain));
    }
    let schema = json!({
        "type": "object",
        "properties": properties,
        "required": ["entities"],
    });
    serde_json::to_string_pretty(&schema).unwrap_or_default()
}

/// The `entities` array schema.
///
/// `name` and `type` are required; `type` carries an enum of the domain's
/// lowercased entity ids when the domain defines entities; `attributes`
/// carries the union of all entity attribute names (first definition wins on
/// a name collision).
pub(crate) fn entity_schema(domain: &DomainConfig) -> Value {
    let mut type_prop = Map::new();
    type_prop.insert("type".to_owned(), json!("string"));
    if !domain.entities.is_empty() {
        let enum_values = domain
            .entities
            .iter()
            .map(|entity| Value::String(entity.id.to_ascii_lowercase()))
            .collect();
        type_prop.insert("enum".to_owned(), Value::Array(enum_values));
    }

    // Without entity definitions the attributes property stays the plain
    // `{"type": "object"}` base; with them it gains the `properties` map
    // (possibly empty).
    let attributes = if domain.entities.is_empty() {
        json!({"type": "object"})
    } else {
        object_attributes_schema(&collect_attributes(
            domain
                .entities
                .iter()
                .flat_map(|entity| entity.attributes.iter())
                .map(|attribute| {
                    (
                        attribute.name.clone(),
                        entity_attr_schema_type(attribute.attr_type),
                    )
                }),
        ))
    };

    let mut properties = Map::new();
    properties.insert("name".to_owned(), json!({"type": "string"}));
    properties.insert("type".to_owned(), Value::Object(type_prop));
    properties.insert("confidence".to_owned(), json!({"type": "number"}));
    properties.insert("description".to_owned(), json!({"type": "string"}));
    properties.insert("attributes".to_owned(), attributes);

    json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": properties,
            "required": ["name", "type"],
        },
    })
}

/// The `relations` array schema.
///
/// All five subject/predicate/object fields are required; `predicate`
/// carries an enum of the domain's lowercased relation predicates when the
/// domain defines relations; `attributes` follows the same rule as in
/// [`entity_schema`].
pub(crate) fn relation_schema(domain: &DomainConfig) -> Value {
    let mut predicate_prop = Map::new();
    predicate_prop.insert("type".to_owned(), json!("string"));
    if !domain.relations.is_empty() {
        let enum_values = domain
            .relations
            .iter()
            .map(|relation| Value::String(relation.predicate.to_ascii_lowercase()))
            .collect();
        predicate_prop.insert("enum".to_owned(), Value::Array(enum_values));
    }

    let attributes = if domain.relations.is_empty() {
        json!({"type": "object"})
    } else {
        object_attributes_schema(&collect_attributes(
            domain
                .relations
                .iter()
                .flat_map(|relation| relation.attributes.iter())
                .map(|attribute| {
                    (
                        attribute.name.clone(),
                        relation_attr_schema_type(&attribute.attr_type),
                    )
                }),
        ))
    };

    let mut properties = Map::new();
    properties.insert("subject_type".to_owned(), json!({"type": "string"}));
    properties.insert("subject_name".to_owned(), json!({"type": "string"}));
    properties.insert("predicate".to_owned(), Value::Object(predicate_prop));
    properties.insert("object_type".to_owned(), json!({"type": "string"}));
    properties.insert("object_name".to_owned(), json!({"type": "string"}));
    properties.insert("attributes".to_owned(), attributes);

    json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": properties,
            "required": ["subject_type", "subject_name", "predicate", "object_type", "object_name"],
        },
    })
}

/// The `attributes` object schema over a collected attribute list.
fn object_attributes_schema(attributes: &[(String, &'static str)]) -> Value {
    let mut properties = Map::new();
    for (name, kind) in attributes {
        properties.insert(name.clone(), json!({"type": kind}));
    }
    json!({"type": "object", "properties": properties})
}

/// Collects the union of attribute definitions across definitions: first
/// occurrence wins on a name collision.
fn collect_attributes(
    pairs: impl Iterator<Item = (String, &'static str)>,
) -> Vec<(String, &'static str)> {
    let mut seen: Vec<(String, &'static str)> = Vec::new();
    for (name, kind) in pairs {
        if !seen.iter().any(|(existing, _)| existing == &name) {
            seen.push((name, kind));
        }
    }
    seen
}

/// The JSON-schema type word for an entity attribute: `number|int|float` →
/// number, `boolean` → boolean, everything else — `string`, `date`,
/// `datetime`, `ref` and unknown words — → string.
fn entity_attr_schema_type(kind: AttributeType) -> &'static str {
    match kind {
        AttributeType::Number => "number",
        AttributeType::Boolean => "boolean",
        AttributeType::String
        | AttributeType::Date
        | AttributeType::Ref
        | AttributeType::Unknown => "string",
    }
}

/// The JSON-schema type word for a relation attribute (matched over the raw
/// XML word; its `condition` arm is the default and folds into `_`).
fn relation_attr_schema_type(word: &str) -> &'static str {
    match word {
        "number" | "int" | "float" => "number",
        "boolean" => "boolean",
        _ => "string",
    }
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (the fixtures are valid).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use config::ontology::{
        AttributeDef, AttributeType, EntityDef, ExtractionDef, RelAttrDef, RelationDef,
    };
    use config::{ConfidencePolicy, DomainConfig};

    use super::*;

    /// Builds a domain config with the given entities/relations.
    fn domain(entities: Vec<EntityDef>, relations: Vec<RelationDef>) -> DomainConfig {
        DomainConfig {
            name: "test-domain".to_owned(),
            version: "1.0".to_owned(),
            description: String::new(),
            entities,
            relations,
            extraction: ExtractionDef::default(),
            confidence: ConfidencePolicy::default(),
        }
    }

    /// One entity definition with the given attributes.
    fn entity(id: &str, attributes: Vec<AttributeDef>) -> EntityDef {
        EntityDef {
            id: id.to_owned(),
            name: id.to_owned(),
            description: String::new(),
            attributes,
            synonyms: vec![],
        }
    }

    /// One entity attribute of the given kind.
    fn attr(name: &str, kind: AttributeType) -> AttributeDef {
        AttributeDef {
            name: name.to_owned(),
            attr_type: kind,
            required: false,
            target: String::new(),
        }
    }

    /// One relation definition with the given attributes.
    fn relation(predicate: &str, attributes: Vec<RelAttrDef>) -> RelationDef {
        RelationDef {
            source: "person".to_owned(),
            predicate: predicate.to_owned(),
            target: "person".to_owned(),
            description: String::new(),
            attributes,
        }
    }

    /// One relation attribute with the given raw type word.
    fn rel_attr(name: &str, kind: &str) -> RelAttrDef {
        RelAttrDef {
            name: name.to_owned(),
            attr_type: kind.to_owned(),
        }
    }

    /// A full test domain: three entities with attributes, two relations.
    fn full_domain() -> DomainConfig {
        domain(
            vec![
                entity(
                    "person",
                    vec![
                        attr("full_name", AttributeType::String),
                        attr("position", AttributeType::String),
                    ],
                ),
                entity(
                    "supplier",
                    vec![attr("company_name", AttributeType::String)],
                ),
                entity(
                    "contract",
                    vec![
                        attr("contract_number", AttributeType::String),
                        attr("date", AttributeType::Date),
                    ],
                ),
            ],
            vec![relation("owner_of", vec![]), relation("signed_by", vec![])],
        )
    }

    /// Full config — top-level object, entities + relations arrays,
    /// `entities` required.
    #[test]
    fn full_config_generates_object_with_entities_and_relations() {
        let schema: Value = serde_json::from_str(&generate_json_schema(&full_domain())).unwrap();

        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["entities"]["type"], "array");
        assert_eq!(schema["properties"]["relations"]["type"], "array");
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|field| field == "entities"));
    }

    /// Empty config — a non-empty schema, entities present, relations absent.
    #[test]
    fn empty_config_has_entities_but_no_relations() {
        let raw = generate_json_schema(&domain(vec![], vec![]));
        assert!(!raw.is_empty(), "schema must be non-empty");
        let schema: Value = serde_json::from_str(&raw).unwrap();

        assert_eq!(schema["properties"]["entities"]["type"], "array");
        assert!(schema["properties"].get("relations").is_none());
    }

    /// Entities only — no relations property.
    #[test]
    fn entities_only_have_no_relations_property() {
        let schema: Value = serde_json::from_str(&generate_json_schema(&domain(
            vec![entity("person", vec![]), entity("organization", vec![])],
            vec![],
        )))
        .unwrap();

        assert!(schema["properties"].get("entities").is_some());
        assert!(schema["properties"].get("relations").is_none());
    }

    /// Entities with attributes: array shape, required `name`/`type`, type
    /// enum, attributes object.
    #[test]
    fn entity_schema_carries_required_fields_and_type_enum() {
        let schema = entity_schema(&domain(
            vec![entity(
                "person",
                vec![
                    attr("full_name", AttributeType::String),
                    attr("position", AttributeType::String),
                    attr("age", AttributeType::Number),
                ],
            )],
            vec![],
        ));

        assert_eq!(schema["type"], "array");
        let items = &schema["items"];
        assert_eq!(items["type"], "object");
        let required = items["required"].as_array().unwrap();
        assert!(required.iter().any(|field| field == "name"));
        assert!(required.iter().any(|field| field == "type"));

        let properties = &items["properties"];
        assert_eq!(properties["type"]["enum"][0], "person");
        assert_eq!(properties["type"]["enum"].as_array().unwrap().len(), 1);
        assert_eq!(properties["attributes"]["type"], "object");
        let attrs = &properties["attributes"]["properties"];
        assert_eq!(attrs["full_name"]["type"], "string");
        assert_eq!(attrs["position"]["type"], "string");
        assert_eq!(attrs["age"]["type"], "number");
    }

    /// Different attribute types: the per-kind mapping (date/ref collapse to
    /// string).
    #[test]
    fn entity_attribute_kind_mapping() {
        let schema = entity_schema(&domain(
            vec![entity(
                "product",
                vec![
                    attr("name", AttributeType::String),
                    attr("price", AttributeType::Number),
                    attr("available", AttributeType::Boolean),
                    attr("created_at", AttributeType::Date),
                    attr("category_ref", AttributeType::Ref),
                ],
            )],
            vec![],
        ));

        let attrs = &schema["items"]["properties"]["attributes"]["properties"];
        assert_eq!(attrs["name"]["type"], "string");
        assert_eq!(attrs["price"]["type"], "number");
        assert_eq!(attrs["available"]["type"], "boolean");
        assert_eq!(attrs["created_at"]["type"], "string");
        assert_eq!(attrs["category_ref"]["type"], "string");
    }

    /// Empty entities: no type enum, and the attributes property stays the
    /// plain object base (no `properties`).
    #[test]
    fn empty_entities_have_no_enum_and_plain_attributes() {
        let schema = entity_schema(&domain(vec![], vec![]));

        let properties = &schema["items"]["properties"];
        assert!(properties["type"].get("enum").is_none());
        assert_eq!(properties["attributes"]["type"], "object");
        assert!(properties["attributes"].get("properties").is_none());
    }

    /// Attribute union: a name defined in two entities appears once, with the
    /// first definition's kind.
    #[test]
    fn attribute_union_first_definition_wins() {
        let schema = entity_schema(&domain(
            vec![
                entity("person", vec![attr("level", AttributeType::String)]),
                entity("contract", vec![attr("level", AttributeType::Number)]),
            ],
            vec![],
        ));

        let attrs = &schema["items"]["properties"]["attributes"]["properties"];
        assert_eq!(attrs["level"]["type"], "string");
        assert_eq!(attrs.as_object().unwrap().len(), 1);
    }

    /// Relations with attributes: all five fields required, predicate enum,
    /// attributes object.
    #[test]
    fn relation_schema_carries_required_fields_and_predicate_enum() {
        let schema = relation_schema(&domain(
            vec![entity("person", vec![]), entity("contract", vec![])],
            vec![relation(
                "signed_by",
                vec![
                    rel_attr("signature_date", "date"),
                    rel_attr("witness", "string"),
                ],
            )],
        ));

        assert_eq!(schema["type"], "array");
        let items = &schema["items"];
        let required = items["required"].as_array().unwrap();
        for expected in [
            "subject_type",
            "subject_name",
            "predicate",
            "object_type",
            "object_name",
        ] {
            assert!(
                required.iter().any(|field| field == expected),
                "missing {expected:?} in {required:?}"
            );
        }

        let properties = &items["properties"];
        assert_eq!(properties["predicate"]["enum"][0], "signed_by");
        let attrs = &properties["attributes"]["properties"];
        assert_eq!(attrs["signature_date"]["type"], "string");
        assert_eq!(attrs["witness"]["type"], "string");
    }

    /// Multiple relations: the enum lists every predicate.
    #[test]
    fn multiple_relations_fill_the_predicate_enum() {
        let schema = relation_schema(&domain(
            vec![entity("person", vec![])],
            vec![
                relation("owner_of", vec![]),
                relation("signed_by", vec![]),
                relation("manages", vec![]),
            ],
        ));

        let enum_values = &schema["items"]["properties"]["predicate"]["enum"];
        let values = enum_values.as_array().unwrap();
        assert_eq!(values.len(), 3);
        for expected in ["owner_of", "signed_by", "manages"] {
            assert!(values.iter().any(|value| value == expected));
        }
    }

    /// Empty relations: no predicate enum.
    #[test]
    fn empty_relations_have_no_predicate_enum() {
        let schema = relation_schema(&domain(vec![entity("person", vec![])], vec![]));
        assert!(
            schema["items"]["properties"]["predicate"]
                .get("enum")
                .is_none()
        );
    }

    /// Every builder emits valid JSON.
    #[test]
    fn generated_schemas_are_valid_json() {
        let domain = full_domain();
        for document in [
            generate_json_schema(&domain),
            serde_json::to_string(&entity_schema(&domain)).unwrap(),
            serde_json::to_string(&relation_schema(&domain)).unwrap(),
        ] {
            assert!(serde_json::from_str::<Value>(&document).is_ok());
        }
    }

    /// Entity ids and relation predicates are lowercased in the enums.
    #[test]
    fn enums_are_lowercased() {
        let schema = generate_json_schema(&domain(
            vec![entity("Person", vec![])],
            vec![relation("Signed_By", vec![])],
        ));
        let parsed: Value = serde_json::from_str(&schema).unwrap();
        assert_eq!(
            parsed["properties"]["entities"]["items"]["properties"]["type"]["enum"][0],
            "person"
        );
        assert_eq!(
            parsed["properties"]["relations"]["items"]["properties"]["predicate"]["enum"][0],
            "signed_by"
        );
    }
}
