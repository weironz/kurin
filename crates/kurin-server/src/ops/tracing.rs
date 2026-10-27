//! Process-wide `tracing` → per-operation event bus routing (ADR-04).
//!
//! The CLI installs one `TracingLayer` bound to its single bus. The server
//! runs several operations concurrently, so it installs one router that
//! reads the `op` field from the current span (the executor sets it to the
//! operation id) and publishes `Body::Log` to that operation's bus.
//!
//! Events outside any operation span (HTTP request logs, startup messages)
//! are left to the `fmt` layer and never enter `op_events`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use kurin_core::events::{Body, EventBus, Level, Stream};
use tracing::field::{Field, Visit};
use tracing::{Event as TracingEvent, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// Operation id → bus, shared by the runner and the tracing router.
#[derive(Clone, Default)]
pub struct BusRegistry {
    buses: Arc<RwLock<HashMap<String, Arc<EventBus>>>>,
}

impl BusRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `bus` for `op_id`.
    pub fn insert(&self, op_id: impl Into<String>, bus: Arc<EventBus>) {
        self.buses
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(op_id.into(), bus);
    }

    /// The bus of a live operation.
    pub fn get(&self, op_id: &str) -> Option<Arc<EventBus>> {
        self.buses
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(op_id)
            .cloned()
    }
}

#[derive(Debug, Default, Clone)]
struct SpanFields {
    op: Option<String>,
    task: Option<String>,
    node: Option<String>,
}

#[derive(Default)]
struct FieldVisitor {
    message: Option<String>,
    node: Option<String>,
    task: Option<String>,
    stream: Option<String>,
    span: SpanFields,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.store(field.name(), value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.store(field.name(), &format!("{value:?}"));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.store(field.name(), &value.to_string());
    }
}

impl FieldVisitor {
    fn store(&mut self, name: &str, value: &str) {
        match name {
            "message" => self.message = Some(value.to_string()),
            "node" => {
                self.node = Some(value.to_string());
                self.span.node = Some(value.to_string());
            }
            "task" => {
                self.task = Some(value.to_string());
                self.span.task = Some(value.to_string());
            }
            "op" => self.span.op = Some(value.to_string()),
            "stream" => self.stream = Some(value.to_string()),
            _ => {}
        }
    }
}

/// Routes `tracing` events into the bus named by the `op` span field.
pub struct RouterLayer {
    registry: BusRegistry,
}

impl RouterLayer {
    pub fn new(registry: BusRegistry) -> Self {
        Self { registry }
    }
}

impl<S> Layer<S> for RouterLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor::default();
        attrs.record(&mut visitor);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(visitor.span);
        }
    }

    fn on_event(&self, event: &TracingEvent<'_>, ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let mut op = None;
        let mut task = visitor.task.clone();
        let mut node = visitor.node.clone();
        if let Some(scope) = ctx.event_scope(event) {
            // Root first, inner spans overwrite: the innermost field wins.
            for span in scope.from_root() {
                if let Some(fields) = span.extensions().get::<SpanFields>() {
                    if fields.op.is_some() {
                        op = fields.op.clone();
                    }
                    if fields.task.is_some() {
                        task = fields.task.clone();
                    }
                    if fields.node.is_some() {
                        node = fields.node.clone();
                    }
                }
            }
        }
        let Some(op) = op else {
            return;
        };
        let Some(bus) = self.registry.get(&op) else {
            return;
        };
        let stream = match visitor.stream.as_deref() {
            Some("stdout") => Stream::Stdout,
            Some("stderr") => Stream::Stderr,
            _ => Stream::Kurin,
        };
        bus.publish(Body::Log {
            level: level_of(event.metadata().level()),
            node,
            task,
            stream,
            line: visitor.message.unwrap_or_default(),
        });
    }
}

fn level_of(level: &tracing::Level) -> Level {
    match *level {
        tracing::Level::TRACE => Level::Trace,
        tracing::Level::DEBUG => Level::Debug,
        tracing::Level::INFO => Level::Info,
        tracing::Level::WARN => Level::Warn,
        tracing::Level::ERROR => Level::Error,
    }
}
