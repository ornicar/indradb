use super::super::{Datastore, EdgeQuery, Transaction, VertexQuery};
use chrono::offset::Utc;
use chrono::DateTime;
use errors::Result;
use models;
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, RwLock};
use uuid::Uuid;

// All of the data is actually stored in this struct, which is stored
// internally to the datastore itself. This way, we can wrap an rwlock around
// the entire datastore, rather than on a per-data structure basis, as the
// latter approach would risk deadlocking without extreme care.
#[derive(Debug)]
struct InternalMemoryDatastore {
    edge_properties: BTreeMap<(models::EdgeKey, String), JsonValue>,
    edges: BTreeMap<models::EdgeKey, DateTime<Utc>>,
    vertex_properties: BTreeMap<(Uuid, String), JsonValue>,
    vertices: BTreeMap<Uuid, models::Type>,
}

impl InternalMemoryDatastore {
    fn get_vertex_values_by_query(&self, q: &VertexQuery) -> Result<Vec<(Uuid, models::Type)>> {
        match *q {
            VertexQuery::All { start_id, limit } => {
                if let Some(start_id) = start_id {
                    Ok(self
                        .vertices
                        .range(start_id..)
                        .take(limit as usize)
                        .map(|(k, v)| (*k, v.clone()))
                        .collect())
                } else {
                    Ok(self
                        .vertices
                        .iter()
                        .take(limit as usize)
                        .map(|(k, v)| (*k, v.clone()))
                        .collect())
                }
            }
            VertexQuery::Vertices { ref ids } => {
                let mut results = Vec::new();

                for id in ids {
                    let value = self.vertices.get(id);

                    if let Some(value) = value {
                        results.push((*id, value.clone()));
                    }
                }

                Ok(results)
            }
            VertexQuery::Pipe {
                ref edge_query,
                converter,
                limit,
            } => {
                let edge_values = self.get_edge_values_by_query(&*edge_query)?;

                let ids: Vec<Uuid> = match converter {
                    models::EdgeDirection::Outbound => edge_values
                        .into_iter()
                        .take(limit as usize)
                        .map(|(key, _)| key.outbound_id)
                        .collect(),
                    models::EdgeDirection::Inbound => edge_values
                        .into_iter()
                        .take(limit as usize)
                        .map(|(key, _)| key.inbound_id)
                        .collect(),
                };

                let mut results = Vec::new();

                for id in ids {
                    let value = self.vertices.get(&id);
                    if let Some(value) = value {
                        results.push((id, value.clone()));
                    }
                }

                Ok(results)
            }
        }
    }

    fn get_edge_values_by_query(&self, q: &EdgeQuery) -> Result<Vec<(models::EdgeKey, DateTime<Utc>)>> {
        match *q {
            EdgeQuery::Edges { ref keys } => {
                let mut results = Vec::new();

                for key in keys {
                    let value = self.edges.get(key);

                    if let Some(update_datetime) = value {
                        results.push((key.clone(), *update_datetime));
                    }
                }

                Ok(results)
            }
            EdgeQuery::Pipe {
                ref vertex_query,
                converter,
                ref type_filter,
                high_filter,
                low_filter,
                limit,
            } => {
                let vertex_values = self.get_vertex_values_by_query(&*vertex_query)?;
                let mut results = Vec::new();

                if limit == 0 {
                    return Ok(results);
                }

                match converter {
                    models::EdgeDirection::Outbound => {
                        for (id, _) in vertex_values {
                            let lower_bound = match *type_filter {
                                Some(ref type_filter) => models::EdgeKey::new(id, type_filter.clone(), Uuid::default()),
                                None => {
                                    let empty_type = models::Type::default();
                                    models::EdgeKey::new(id, empty_type, Uuid::default())
                                }
                            };

                            for (key, update_datetime) in self.edges.range(lower_bound..) {
                                if key.outbound_id != id {
                                    break;
                                }

                                if let Some(ref type_filter) = *type_filter {
                                    if &key.t != type_filter {
                                        break;
                                    }
                                }

                                if let Some(high_filter) = high_filter {
                                    if *update_datetime > high_filter {
                                        continue;
                                    }
                                }

                                if let Some(low_filter) = low_filter {
                                    if *update_datetime < low_filter {
                                        continue;
                                    }
                                }

                                results.push((key.clone(), *update_datetime));

                                if results.len() == limit as usize {
                                    return Ok(results);
                                }
                            }
                        }
                    }
                    models::EdgeDirection::Inbound => {
                        let mut candidate_ids = HashSet::new();
                        for (id, _) in vertex_values {
                            candidate_ids.insert(id);
                        }

                        for (key, update_datetime) in &self.edges {
                            if !candidate_ids.contains(&key.inbound_id) {
                                continue;
                            }

                            if let Some(ref type_filter) = *type_filter {
                                if &key.t != type_filter {
                                    continue;
                                }
                            }

                            if let Some(high_filter) = high_filter {
                                if *update_datetime > high_filter {
                                    continue;
                                }
                            }

                            if let Some(low_filter) = low_filter {
                                if *update_datetime < low_filter {
                                    continue;
                                }
                            }

                            results.push((key.clone(), *update_datetime));

                            if results.len() == limit as usize {
                                return Ok(results);
                            }
                        }
                    }
                }

                Ok(results)
            }
        }
    }

    fn delete_vertices(&mut self, vertices: Vec<Uuid>) {
        for vertex_id in vertices {
            self.vertices.remove(&vertex_id);

            let mut deletable_vertex_properties: Vec<(Uuid, String)> = Vec::new();

            for (property_key, _) in self.vertex_properties.range((vertex_id, "".to_string())..) {
                let &(ref property_vertex_id, _) = property_key;

                if &vertex_id != property_vertex_id {
                    break;
                }

                deletable_vertex_properties.push(property_key.clone());
            }

            for property_key in deletable_vertex_properties {
                self.vertex_properties.remove(&property_key);
            }

            let mut deletable_edges: Vec<models::EdgeKey> = Vec::new();

            for edge_key in self.edges.keys() {
                if edge_key.outbound_id == vertex_id || edge_key.inbound_id == vertex_id {
                    deletable_edges.push(edge_key.clone());
                }
            }

            self.delete_edges(deletable_edges);
        }
    }

    fn delete_edges(&mut self, edges: Vec<models::EdgeKey>) {
        for edge_key in edges {
            self.edges.remove(&edge_key);

            let mut deletable_edge_properties: Vec<(models::EdgeKey, String)> = Vec::new();

            for (property_key, _) in self.edge_properties.range((edge_key.clone(), "".to_string())..) {
                let &(ref property_edge_key, _) = property_key;

                if &edge_key != property_edge_key {
                    break;
                }

                deletable_edge_properties.push(property_key.clone());
            }

            for property_key in deletable_edge_properties {
                self.edge_properties.remove(&property_key);
            }
        }
    }
}

/// An in-memory-only datastore.
#[derive(Debug)]
pub struct MemoryDatastore(Arc<RwLock<InternalMemoryDatastore>>);

impl MemoryDatastore {
    /// Creates a new in-memory datastore.
    pub fn default() -> MemoryDatastore {
        Self {
            0: Arc::new(RwLock::new(InternalMemoryDatastore {
                edge_properties: BTreeMap::new(),
                edges: BTreeMap::new(),
                vertex_properties: BTreeMap::new(),
                vertices: BTreeMap::new(),
            })),
        }
    }
}

impl Datastore for MemoryDatastore {
    type Trans = MemoryTransaction;

    fn transaction(&self) -> Result<Self::Trans> {
        Ok(MemoryTransaction {
            datastore: Arc::clone(&self.0),
        })
    }
}

/// A transaction for manipulating in-memory-only datastores.
#[derive(Debug)]
pub struct MemoryTransaction {
    datastore: Arc<RwLock<InternalMemoryDatastore>>,
}

impl Transaction for MemoryTransaction {
    fn create_vertex(&self, vertex: &models::Vertex) -> Result<bool> {
        let mut datastore = self.datastore.write().unwrap();
        let mut inserted = false;

        datastore.vertices.entry(vertex.id).or_insert_with(|| {
            inserted = true;
            vertex.t.clone()
        });

        Ok(inserted)
    }

    fn get_vertices(&self, q: &VertexQuery) -> Result<Vec<models::Vertex>> {
        let vertex_values = self.datastore.read().unwrap().get_vertex_values_by_query(q)?;
        let iter = vertex_values
            .into_iter()
            .map(|(uuid, t)| models::Vertex::with_id(uuid, t));
        Ok(iter.collect())
    }

    fn delete_vertices(&self, q: &VertexQuery) -> Result<()> {
        let mut datastore = self.datastore.write().unwrap();
        let deletable_vertices = datastore
            .get_vertex_values_by_query(q)?
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        datastore.delete_vertices(deletable_vertices);
        Ok(())
    }

    fn get_vertex_count(&self) -> Result<u64> {
        let datastore = self.datastore.read().unwrap();
        Ok(datastore.vertices.len() as u64)
    }

    fn create_edge(&self, key: &models::EdgeKey) -> Result<bool> {
        let mut datastore = self.datastore.write().unwrap();

        if !datastore.vertices.contains_key(&key.outbound_id) || !datastore.vertices.contains_key(&key.inbound_id) {
            return Ok(false);
        }

        datastore.edges.insert(key.clone(), Utc::now());
        Ok(true)
    }

    fn get_edges(&self, q: &EdgeQuery) -> Result<Vec<models::Edge>> {
        let edge_values = {
            let datastore = self.datastore.read().unwrap();
            datastore.get_edge_values_by_query(q)?
        };

        let iter = edge_values
            .into_iter()
            .map(|(key, update_datetime)| models::Edge::new(key, update_datetime));
        Ok(iter.collect())
    }

    fn delete_edges(&self, q: &EdgeQuery) -> Result<()> {
        let mut datastore = self.datastore.write().unwrap();
        let deletable_edges: Vec<models::EdgeKey> = datastore
            .get_edge_values_by_query(q)?
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        datastore.delete_edges(deletable_edges);
        Ok(())
    }

    fn get_edge_count(
        &self,
        id: Uuid,
        type_filter: Option<&models::Type>,
        direction: models::EdgeDirection,
    ) -> Result<u64> {
        let datastore = self.datastore.read().unwrap();

        if direction == models::EdgeDirection::Outbound {
            let lower_bound = match type_filter {
                Some(type_filter) => models::EdgeKey::new(id, type_filter.clone(), Uuid::default()),
                None => {
                    let empty_type = models::Type::default();
                    models::EdgeKey::new(id, empty_type, Uuid::default())
                }
            };
            let range = datastore.edges.range(lower_bound..);

            let range = range.take_while(|&(k, _)| {
                if let Some(type_filter) = type_filter {
                    k.outbound_id == id && &k.t == type_filter
                } else {
                    k.outbound_id == id
                }
            });

            Ok(range.count() as u64)
        } else {
            let range = datastore.edges.iter().filter(|&(k, _)| {
                if let Some(type_filter) = type_filter {
                    k.inbound_id == id && &k.t == type_filter
                } else {
                    k.inbound_id == id
                }
            });

            Ok(range.count() as u64)
        }
    }

    fn get_vertex_properties(&self, q: &VertexQuery, name: &str) -> Result<Vec<models::VertexProperty>> {
        let mut result = Vec::new();
        let datastore = self.datastore.read().unwrap();
        let vertex_values = datastore.get_vertex_values_by_query(q)?;

        for (id, _) in vertex_values {
            let property_value = datastore.vertex_properties.get(&(id, name.to_string()));

            if let Some(property_value) = property_value {
                result.push(models::VertexProperty::new(id, property_value.clone()));
            }
        }

        Ok(result)
    }

    fn set_vertex_properties(&self, q: &VertexQuery, name: &str, value: &JsonValue) -> Result<()> {
        let mut datastore = self.datastore.write().unwrap();

        let vertex_values = datastore.get_vertex_values_by_query(q)?;

        for (id, _) in vertex_values {
            datastore
                .vertex_properties
                .insert((id, name.to_string()), value.clone());
        }

        Ok(())
    }

    fn delete_vertex_properties(&self, q: &VertexQuery, name: &str) -> Result<()> {
        let mut datastore = self.datastore.write().unwrap();

        let vertex_values = datastore.get_vertex_values_by_query(q)?;

        for (id, _) in vertex_values {
            datastore.vertex_properties.remove(&(id, name.to_string()));
        }

        Ok(())
    }

    fn get_edge_properties(&self, q: &EdgeQuery, name: &str) -> Result<Vec<models::EdgeProperty>> {
        let mut result = Vec::new();
        let datastore = self.datastore.read().unwrap();
        let edge_values = datastore.get_edge_values_by_query(q)?;

        for (key, _) in edge_values {
            let property_value = datastore.edge_properties.get(&(key.clone(), name.to_string()));

            if let Some(property_value) = property_value {
                result.push(models::EdgeProperty::new(key, property_value.clone()));
            }
        }

        Ok(result)
    }

    fn set_edge_properties(&self, q: &EdgeQuery, name: &str, value: &JsonValue) -> Result<()> {
        let mut datastore = self.datastore.write().unwrap();

        let edge_values = datastore.get_edge_values_by_query(q)?;

        for (key, _) in edge_values {
            datastore.edge_properties.insert((key, name.to_string()), value.clone());
        }

        Ok(())
    }

    fn delete_edge_properties(&self, q: &EdgeQuery, name: &str) -> Result<()> {
        let mut datastore = self.datastore.write().unwrap();

        let edge_values = datastore.get_edge_values_by_query(q)?;

        for (key, _) in edge_values {
            datastore.edge_properties.remove(&(key, name.to_string()));
        }

        Ok(())
    }
}
