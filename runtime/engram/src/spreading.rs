//! Graph spreading activation — multi-hop score propagation for retrieval.

use crate::fact::EntityId;
use crate::graph::GraphStore;
use crate::store::MemoryError;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// Configuration for spreading activation.
#[derive(Debug, Clone)]
pub struct SpreadingActivationConfig {
    pub max_depth: usize,
    pub decay_factor: f32,
    pub min_activation: f32,
    pub max_activated: usize,
}

impl Default for SpreadingActivationConfig {
    fn default() -> Self {
        Self {
            max_depth: 3,
            decay_factor: 0.5,
            min_activation: 0.05,
            max_activated: 50,
        }
    }
}

/// Run spreading activation from seed entities through the graph.
/// Returns a map of EntityId -> activation_score for all activated entities.
pub async fn spread(
    graph: &Arc<dyn GraphStore>,
    seeds: &[(EntityId, f32)],
    config: &SpreadingActivationConfig,
) -> Result<HashMap<EntityId, f32>, MemoryError> {
    let mut activations: HashMap<EntityId, f32> = HashMap::new();
    let mut visited: HashSet<EntityId> = HashSet::new();
    let mut queue: VecDeque<(EntityId, f32, usize)> = VecDeque::new();

    for (id, score) in seeds {
        activations.insert(*id, *score);
        queue.push_back((*id, *score, 0));
        visited.insert(*id);
    }

    while let Some((entity_id, activation, depth)) = queue.pop_front() {
        if depth >= config.max_depth || activations.len() >= config.max_activated {
            break;
        }

        let child_activation = activation * config.decay_factor;
        if child_activation < config.min_activation {
            continue;
        }

        let subgraph = graph.neighbors(entity_id, 1, None).await?;
        for entity in &subgraph.entities {
            if visited.contains(&entity.id) {
                continue;
            }
            visited.insert(entity.id);
            let entry = activations.entry(entity.id).or_insert(0.0);
            *entry = entry.max(child_activation);
            queue.push_back((entity.id, child_activation, depth + 1));
        }
    }

    Ok(activations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg = SpreadingActivationConfig::default();
        assert_eq!(cfg.max_depth, 3);
        assert!((cfg.decay_factor - 0.5).abs() < f32::EPSILON);
        assert_eq!(cfg.max_activated, 50);
    }
}
