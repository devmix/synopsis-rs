//! The shared 4-field entity wire shape (`id`, `name`, `type`, `domain`).
//!
//! The oracle carries this shape under three names — `EntityWithContext`
//! (fact tools), `EntityOut` (document tools) and `RelatedEntity` (the
//! entity dossier) — all with the identical field set and order. This module
//! hoists it into one type (task 5.8, reviewer guidance) so the wire JSON is
//! defined once.
//!
//! Oracle mapping: `../synopsis/internal/mcp/handlers/{get_fact_by_id.go,
//! get_document_context.go, get_entity_dossier.go}`.

use db::Entity;
use graph::EntityNode;
use serde::Serialize;

/// A brief entity reference (oracle `EntityWithContext` / `EntityOut` /
/// `RelatedEntity`): field order matches the Go structs, so the wire JSON
/// matches the oracle's marshal output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntityBrief {
    /// Entity row id.
    pub id: i64,
    /// Entity name.
    pub name: String,
    /// Entity type.
    #[serde(rename = "type")]
    pub r#type: String,
    /// Entity domain (`''` = global).
    pub domain: String,
}

/// Map a stored entity row to the wire brief.
pub fn entity_brief(entity: &Entity) -> EntityBrief {
    EntityBrief {
        id: entity.id,
        name: entity.name.clone(),
        r#type: entity.entity_type.clone(),
        domain: entity.domain.clone(),
    }
}

/// Map a graph node to the wire brief (the dossier's `related_entities` come
/// from traversal nodes, whose `domain` is normalized per the graph crate).
pub fn node_brief(node: &EntityNode) -> EntityBrief {
    EntityBrief {
        id: node.id,
        name: node.name.clone(),
        r#type: node.entity_type.clone(),
        domain: node.domain.clone(),
    }
}
