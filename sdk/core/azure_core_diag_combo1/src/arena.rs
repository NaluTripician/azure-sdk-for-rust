// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! The C2 arena: a span tree stored as a flat `Vec<Node>` with index-linked parents.
//!
//! Adding a span is a `Vec::push`; "passing" a span is a `u32` id. There are no per-node heap
//! nodes and no `Rc`/`RefCell`. On success the whole arena can be dropped without serializing;
//! on error (or when verbose) it is projected into a [`WireTree`] and encoded to binary.

use azure_core_diag_common::wire::{NodeKind, WireNode, WireTree};

/// A single span in the arena.
#[derive(Clone, Debug)]
pub struct Node {
    /// Parent node index, or `None` for the root operation.
    pub parent: Option<u32>,
    /// The kind of span.
    pub kind: NodeKind,
    /// Start tick (ns).
    pub start_ns: u64,
    /// Duration (ns).
    pub duration_ns: u64,
    /// HTTP status code, or `0` when not applicable.
    pub status: u16,
    /// Attribute key/value pairs.
    pub attrs: Vec<(String, String)>,
}

impl Node {
    fn new(parent: Option<u32>, kind: NodeKind, start_ns: u64, duration_ns: u64) -> Self {
        Self {
            parent,
            kind,
            start_ns,
            duration_ns,
            status: 0,
            attrs: Vec::new(),
        }
    }
}

/// The arena tree for one operation.
#[derive(Clone, Debug, Default)]
pub struct Arena {
    /// Operation name.
    pub operation: String,
    /// All nodes; the root operation is index `0`.
    pub nodes: Vec<Node>,
    /// Whether the operation ultimately succeeded (drives the drop-on-success path).
    pub success: bool,
}

impl Arena {
    /// Creates an empty arena.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pushes a node and returns its index (the "span id").
    pub fn push(
        &mut self,
        parent: Option<u32>,
        kind: NodeKind,
        start_ns: u64,
        duration_ns: u64,
    ) -> u32 {
        let id = self.nodes.len() as u32;
        self.nodes
            .push(Node::new(parent, kind, start_ns, duration_ns));
        id
    }

    /// Adds an attribute to the node at `id`.
    pub fn attr(&mut self, id: u32, key: &str, value: impl Into<String>) {
        if let Some(node) = self.nodes.get_mut(id as usize) {
            node.attrs.push((key.to_string(), value.into()));
        }
    }

    /// Sets the HTTP status on the node at `id`.
    pub fn set_status(&mut self, id: u32, status: u16) {
        if let Some(node) = self.nodes.get_mut(id as usize) {
            node.status = status;
        }
    }

    /// Sets the duration on the node at `id`.
    pub fn set_duration(&mut self, id: u32, duration_ns: u64) {
        if let Some(node) = self.nodes.get_mut(id as usize) {
            node.duration_ns = duration_ns;
        }
    }

    /// Projects the arena into the shared [`WireTree`] (the C2 "construct" step).
    ///
    /// This is intentionally a flat, cheap copy — no intermediate JSON tree is built. The
    /// resulting [`WireTree`] feeds the binary encoder directly.
    pub fn to_wire(&self) -> WireTree {
        WireTree {
            operation: self.operation.clone(),
            nodes: self
                .nodes
                .iter()
                .map(|n| WireNode {
                    parent: n.parent,
                    kind: n.kind as u8,
                    start_ns: n.start_ns,
                    duration_ns: n.duration_ns,
                    status: n.status,
                    attrs: n.attrs.clone(),
                })
                .collect(),
        }
    }
}
