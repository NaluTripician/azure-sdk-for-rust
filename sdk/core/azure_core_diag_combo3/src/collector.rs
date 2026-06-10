// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! A capturing `tracing-subscriber` [`Layer`] that materializes the span tree into a raw store.
//!
//! This is the B2 (`tracing`-native) capture mechanism: spans and events flow through the
//! normal `tracing` machinery, and this layer eagerly records every span, field, and event into
//! an in-memory store keyed by span id. The store is later projected into typed eager objects.

use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::Event;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// A span captured from the `tracing` pipeline before it is projected into a typed object.
#[derive(Clone, Debug, Default)]
pub struct RawSpan {
    /// The span's metadata name (e.g. `operation`, `attempt`, `routing`).
    pub name: String,
    /// The parent span id, if any.
    pub parent: Option<u64>,
    /// Recorded span fields.
    pub fields: BTreeMap<String, Value>,
    /// Events emitted with this span as their parent, in order.
    pub events: Vec<Map<String, Value>>,
    /// Insertion order, so children render deterministically.
    pub order: u64,
}

/// The raw capture store shared between the layer and the caller.
#[derive(Clone, Debug, Default)]
pub struct Store {
    /// All captured spans keyed by their numeric span id.
    pub spans: BTreeMap<u64, RawSpan>,
    next_order: u64,
}

impl Store {
    /// Returns the root span id (the one with no parent), if any.
    pub fn root(&self) -> Option<u64> {
        self.spans
            .iter()
            .filter(|(_, s)| s.parent.is_none())
            .min_by_key(|(_, s)| s.order)
            .map(|(id, _)| *id)
    }

    /// Returns the child span ids of `parent`, ordered by insertion.
    pub fn children(&self, parent: u64) -> Vec<u64> {
        let mut kids: Vec<(u64, u64)> = self
            .spans
            .iter()
            .filter(|(_, s)| s.parent == Some(parent))
            .map(|(id, s)| (*id, s.order))
            .collect();
        kids.sort_by_key(|(_, order)| *order);
        kids.into_iter().map(|(id, _)| id).collect()
    }
}

/// A shared handle to the capture [`Store`].
pub type SharedStore = Arc<Mutex<Store>>;

/// The capturing layer.
pub struct SpanCollector {
    store: SharedStore,
}

impl SpanCollector {
    /// Creates a collector writing into `store`.
    pub fn new(store: SharedStore) -> Self {
        Self { store }
    }
}

struct FieldVisitor<'a> {
    target: &'a mut Map<String, Value>,
}

impl Visit for FieldVisitor<'_> {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.target
            .insert(field.name().to_string(), json_f64(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.target
            .insert(field.name().to_string(), Value::from(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.target
            .insert(field.name().to_string(), Value::from(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.target
            .insert(field.name().to_string(), Value::from(value));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.target
            .insert(field.name().to_string(), Value::from(value));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.target
            .insert(field.name().to_string(), Value::from(format!("{value:?}")));
    }
}

fn json_f64(value: f64) -> Value {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn id_to_u64(id: &Id) -> u64 {
    id.into_u64()
}

impl<S> Layer<S> for SpanCollector
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = Map::new();
        attrs.record(&mut FieldVisitor {
            target: &mut fields,
        });
        let parent = attrs
            .parent()
            .map(id_to_u64)
            .or_else(|| ctx.current_span().id().map(id_to_u64));
        let mut store = self.store.lock().unwrap();
        let order = store.next_order;
        store.next_order += 1;
        store.spans.insert(
            id_to_u64(id),
            RawSpan {
                name: attrs.metadata().name().to_string(),
                parent,
                fields: fields.into_iter().collect(),
                events: Vec::new(),
                order,
            },
        );
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        let mut fields = Map::new();
        values.record(&mut FieldVisitor {
            target: &mut fields,
        });
        let mut store = self.store.lock().unwrap();
        if let Some(span) = store.spans.get_mut(&id_to_u64(id)) {
            for (k, v) in fields {
                span.fields.insert(k, v);
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut fields = Map::new();
        event.record(&mut FieldVisitor {
            target: &mut fields,
        });
        let parent = event
            .parent()
            .map(id_to_u64)
            .or_else(|| ctx.current_span().id().map(id_to_u64));
        if let Some(parent) = parent {
            let mut store = self.store.lock().unwrap();
            if let Some(span) = store.spans.get_mut(&parent) {
                span.events.push(fields);
            }
        }
    }
}
