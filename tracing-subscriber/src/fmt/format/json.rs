use super::{Format, FormatEvent, FormatFields, FormatTime};
use crate::{
    field::{RecordFields, VisitOutput},
    fmt::fmt_subscriber::{FmtContext, FormattedFields},
    registry::LookupSpan,
};
use serde::{
    ser::{SerializeMap, Serializer as SerdeSerializer},
    Serialize,
};
use serde_json::Serializer;
use std::{
    collections::BTreeMap,
    fmt::{self, Write},
    io,
};
use tracing_core::{
    field::{self, Field},
    span::Record,
    Collect, Event,
};
use tracing_serde::AsSerde;

#[cfg(feature = "tracing-log")]
use tracing_log::NormalizeEvent;

/// Marker for `Format` that indicates that the verbose json log format should be used.
///
/// The full format includes fields from all entered spans.
///
/// # Example Output
///
/// ```json
/// {
///     "timestamp":"Feb 20 11:28:15.096",
///     "level":"INFO",
///     "fields":{"message":"some message","key":"value"}
///     "target":"mycrate",
///     "span":{"name":"leaf"},
///     "spans":[{"name":"root"},{"name":"leaf"}],
/// }
/// ```
///
/// # Options
///
/// - [`Json::flatten_event`] can be used to enable flattening event fields into
/// the root
/// - [`Json::with_current_span`] can be used to control logging of the current
/// span
/// - [`Json::with_span_list`] can be used to control logging of the span list
/// object.
///
/// By default, event fields are not flattened, and both current span and span
/// list are logged.
///
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct Json {
    pub(crate) flatten_event: bool,
    pub(crate) display_current_span: bool,
    pub(crate) display_span_list: bool,
    pub(crate) merge_parent_fields: bool,
    pub(crate) namespace_parent_fields: bool,
}

impl Json {
    /// If set to `true` event metadata will be flattened into the root object.
    pub fn flatten_event(&mut self, flatten_event: bool) {
        self.flatten_event = flatten_event;
    }

    /// If set to `false`, formatted events won't contain a field for the current span.
    pub fn with_current_span(&mut self, display_current_span: bool) {
        self.display_current_span = display_current_span;
    }

    /// If set to `false`, formatted events won't contain a list of all currently
    /// entered spans. Spans are logged in a list from root to leaf.
    pub fn with_span_list(&mut self, display_span_list: bool) {
        self.display_span_list = display_span_list;
    }

    /// If is set to `true`, formatted events will contain every field of their
    /// parent spans
    pub fn merge_parent_fields(&mut self, merge_parent_fields: bool) {
        self.merge_parent_fields = merge_parent_fields;
    }

    /// If is set to `true`, and if merging parent fields, all fields from parents
    /// will be namespaced with the span's name
    pub fn namespace_parent_fields(&mut self, namespace_parent_fields: bool) {
        self.namespace_parent_fields = namespace_parent_fields;
    }
}

struct SerializableContext<'a, 'b, Span, N>(
    &'b crate::subscribe::Context<'a, Span>,
    std::marker::PhantomData<N>,
)
where
    Span: Collect + for<'lookup> crate::registry::LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static;

impl<'a, 'b, Span, N> serde::ser::Serialize for SerializableContext<'a, 'b, Span, N>
where
    Span: Collect + for<'lookup> crate::registry::LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn serialize<Ser>(&self, serializer_o: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: serde::ser::Serializer,
    {
        use serde::ser::SerializeSeq;
        let mut serializer = serializer_o.serialize_seq(None)?;

        for span in self.0.scope() {
            serializer.serialize_element(&SerializableSpan {
                span: &span,
                namespace: false,
                _phantom: self.1,
            })?;
        }

        serializer.end()
    }
}

struct Key<'a> {
    prefix: Option<&'static str>,
    key: &'a str,
}

impl<'a> Serialize for Key<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: SerdeSerializer,
    {
        if let Some(prefix) = self.prefix {
            return serializer.collect_str(&format_args!("{}.{}", prefix, self.key));
        }

        serializer.serialize_str(self.key)
    }
}

struct SerializableSpan<
    'a,
    'b,
    Span: for<'lookup> crate::registry::LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
> {
    span: &'b crate::registry::SpanRef<'a, Span>,
    namespace: bool,
    _phantom: std::marker::PhantomData<N>,
}

impl<'a, 'b, Span, N> SerializableSpan<'a, 'b, Span, N>
where
    Span: for<'lookup> crate::registry::LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn serialize_formatted_fields<SerMap>(&self, map: &mut SerMap) -> Result<(), SerMap::Error>
    where
        SerMap: SerializeMap,
    {
        let ext = self.span.extensions();
        let data = ext
            .get::<FormattedFields<N>>()
            .expect("Unable to find FormattedFields in extensions; this is a bug");

        // TODO: let's _not_ do this, but this resolves
        // https://github.com/tokio-rs/tracing/issues/391.
        // We should probably rework this to use a `serde_json::Value` or something
        // similar in a JSON-specific layer, but I'd (david)
        // rather have a uglier fix now rather than shipping broken JSON.
        match serde_json::from_str::<serde_json::Value>(&data) {
            Ok(serde_json::Value::Object(fields)) => {
                for field in fields {
                    map.serialize_key(&Key {
                        key: &field.0,
                        prefix: if self.namespace { Some(self.span.name()) } else { None },
                    })
                    .and_then(|_| map.serialize_value(&field.1))?;
                }
            }
            // We have fields for this span which are valid JSON but not an object.
            // This is probably a bug, so panic if we're in debug mode
            Ok(_) if cfg!(debug_assertions) => panic!(
                "span '{}' had malformed fields! this is a bug.\n  error: invalid JSON object\n  fields: {:?}",
                self.span.metadata().name(),
                data
            ),
            // If we *aren't* in debug mode, it's probably best not to
            // crash the program, let's log the field found but also an
            // message saying it's type  is invalid
            Ok(value) => {
                map.serialize_entry("field", &value)?;
                map.serialize_entry("field_error", "field was no a valid object")?
            }
            // We have previously recorded fields for this span
            // should be valid JSON. However, they appear to *not*
            // be valid JSON. This is almost certainly a bug, so
            // panic if we're in debug mode
            Err(e) if cfg!(debug_assertions) => panic!(
                "span '{}' had malformed fields! this is a bug.\n  error: {}\n  fields: {:?}",
                self.span.metadata().name(),
                e,
                data
            ),
            // If we *aren't* in debug mode, it's probably best not
            // crash the program, but let's at least make sure it's clear
            // that the fields are not supposed to be missing.
            Err(e) => map.serialize_entry("field_error", &format!("{}", e))?,
        };
        Ok(())
    }
}

impl<'a, 'b, Span, N> serde::ser::Serialize for SerializableSpan<'a, 'b, Span, N>
where
    Span: for<'lookup> crate::registry::LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn serialize<Ser>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: serde::ser::Serializer,
    {
        let mut serializer = serializer.serialize_map(None)?;

        self.serialize_formatted_fields(&mut serializer)?;

        serializer.serialize_entry("name", self.span.metadata().name())?;
        serializer.end()
    }
}

struct MergeParentFieldsMap<'a, S, N> {
    event: &'a Event<'a>,
    ctx: &'a FmtContext<'a, S, N>,
    namespace: bool,
    phantom: std::marker::PhantomData<N>,
}

impl<'a, S, N> MergeParentFieldsMap<'a, S, N>
where
    S: Collect + for<'lookup> LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn merge_parent_fields<SerMap>(&self, map: &mut SerMap) -> Result<(), SerMap::Error>
    where
        SerMap: SerializeMap,
    {
        let current = match self.ctx.lookup_current() {
            Some(current) => current,
            None => return Ok(()),
        };
        let parents = current.parents();

        for span in std::iter::once(current).chain(parents) {
            SerializableSpan {
                span: &span,
                namespace: self.namespace,
                _phantom: self.phantom,
            }
            .serialize_formatted_fields(map)?;
        }
        Ok(())
    }
}

impl<'a, S, N> Serialize for MergeParentFieldsMap<'a, S, N>
where
    S: Collect + for<'lookup> LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn serialize<Ser>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: SerdeSerializer,
    {
        let map = serializer.serialize_map(None)?;
        let mut visitor = tracing_serde::SerdeMapVisitor::new(map);
        self.event.record(&mut visitor);

        let mut map = visitor.take_serializer()?;
        self.merge_parent_fields(&mut map)?;

        map.end()
    }
}

impl<S, N, T> FormatEvent<S, N> for Format<Json, T>
where
    S: Collect + for<'lookup> LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
    T: FormatTime,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        writer: &mut dyn fmt::Write,
        event: &Event<'_>,
    ) -> fmt::Result
    where
        S: Collect + for<'a> LookupSpan<'a>,
    {
        let mut timestamp = String::new();
        self.timer.format_time(&mut timestamp)?;

        #[cfg(feature = "tracing-log")]
        let normalized_meta = event.normalized_metadata();
        #[cfg(feature = "tracing-log")]
        let meta = normalized_meta.as_ref().unwrap_or_else(|| event.metadata());
        #[cfg(not(feature = "tracing-log"))]
        let meta = event.metadata();

        let mut visit = || {
            let mut serializer = Serializer::new(WriteAdaptor::new(writer));

            let mut serializer = serializer.serialize_map(None)?;

            serializer.serialize_entry("timestamp", &timestamp)?;
            serializer.serialize_entry("level", &meta.level().as_serde())?;

            let format_field_marker: std::marker::PhantomData<N> = std::marker::PhantomData;

            let current_span = if self.format.display_current_span || self.format.display_span_list
            {
                ctx.ctx.current_span().id().and_then(|id| ctx.ctx.span(id))
            } else {
                None
            };

            if self.format.flatten_event {
                let mut visitor = tracing_serde::SerdeMapVisitor::new(serializer);
                event.record(&mut visitor);

                serializer = visitor.take_serializer()?;

                if self.format.merge_parent_fields {
                    MergeParentFieldsMap {
                        event,
                        ctx,
                        namespace: self.format.namespace_parent_fields,
                        phantom: format_field_marker,
                    }
                    .merge_parent_fields(&mut serializer)?;
                }
            } else if self.format.merge_parent_fields {
                serializer.serialize_entry(
                    "fields",
                    &MergeParentFieldsMap {
                        event,
                        ctx,
                        namespace: self.format.namespace_parent_fields,
                        phantom: format_field_marker,
                    },
                )?;
            } else {
                use tracing_serde::fields::AsMap;
                serializer.serialize_entry("fields", &event.field_map())?;
            };

            if self.display_target {
                serializer.serialize_entry("target", meta.target())?;
            }

            if self.format.display_current_span {
                if let Some(ref span) = current_span {
                    serializer
                        .serialize_entry(
                            "span",
                            &SerializableSpan {
                                span,
                                namespace: false,
                                _phantom: format_field_marker,
                            },
                        )
                        .unwrap_or(());
                }
            }

            if self.format.display_span_list && current_span.is_some() {
                serializer.serialize_entry(
                    "spans",
                    &SerializableContext(&ctx.ctx, format_field_marker),
                )?;
            }

            if self.display_thread_name {
                let current_thread = std::thread::current();
                match current_thread.name() {
                    Some(name) => {
                        serializer.serialize_entry("threadName", name)?;
                    }
                    // fall-back to thread id when name is absent and ids are not enabled
                    None if !self.display_thread_id => {
                        serializer
                            .serialize_entry("threadName", &format!("{:?}", current_thread.id()))?;
                    }
                    _ => {}
                }
            }

            if self.display_thread_id {
                serializer
                    .serialize_entry("threadId", &format!("{:?}", std::thread::current().id()))?;
            }

            serializer.end()
        };

        visit().map_err(|_| fmt::Error)?;
        writeln!(writer)
    }
}

impl Default for Json {
    fn default() -> Json {
        Json {
            flatten_event: false,
            display_current_span: true,
            display_span_list: true,
            merge_parent_fields: false,
            namespace_parent_fields: true,
        }
    }
}

/// The JSON [`FormatFields`] implementation.
///
#[derive(Debug)]
pub struct JsonFields {
    // reserve the ability to add fields to this without causing a breaking
    // change in the future.
    _private: (),
}

impl JsonFields {
    /// Returns a new JSON [`FormatFields`] implementation.
    ///
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for JsonFields {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> FormatFields<'a> for JsonFields {
    /// Format the provided `fields` to the provided `writer`, returning a result.
    fn format_fields<R: RecordFields>(
        &self,
        writer: &'a mut dyn fmt::Write,
        fields: R,
    ) -> fmt::Result {
        let mut v = JsonVisitor::new(writer);
        fields.record(&mut v);
        v.finish()
    }

    /// Record additional field(s) on an existing span.
    ///
    /// By default, this appends a space to the current set of fields if it is
    /// non-empty, and then calls `self.format_fields`. If different behavior is
    /// required, the default implementation of this method can be overridden.
    fn add_fields(&self, current: &'a mut String, fields: &Record<'_>) -> fmt::Result {
        if !current.is_empty() {
            // If fields were previously recorded on this span, we need to parse
            // the current set of fields as JSON, add the new fields, and
            // re-serialize them. Otherwise, if we just appended the new fields
            // to a previously serialized JSON object, we would end up with
            // malformed JSON.
            //
            // XXX(eliza): this is far from efficient, but unfortunately, it is
            // necessary as long as the JSON formatter is implemented on top of
            // an interface that stores all formatted fields as strings.
            //
            // We should consider reimplementing the JSON formatter as a
            // separate layer, rather than a formatter for the `fmt` layer —
            // then, we could store fields as JSON values, and add to them
            // without having to parse and re-serialize.
            let mut new = String::new();
            let map: BTreeMap<&'_ str, serde_json::Value> =
                serde_json::from_str(current).map_err(|_| fmt::Error)?;
            let mut v = JsonVisitor::new(&mut new);
            v.values = map;
            fields.record(&mut v);
            v.finish()?;
            *current = new;
        } else {
            // If there are no previously recorded fields, we can just reuse the
            // existing string.
            let mut v = JsonVisitor::new(current);
            fields.record(&mut v);
            v.finish()?;
        }

        Ok(())
    }
}

/// The [visitor] produced by [`JsonFields`]'s [`MakeVisitor`] implementation.
///
/// [visitor]: crate::field::Visit
/// [`MakeVisitor`]: crate::field::MakeVisitor
pub struct JsonVisitor<'a> {
    values: BTreeMap<&'a str, serde_json::Value>,
    writer: &'a mut dyn Write,
}

impl<'a> fmt::Debug for JsonVisitor<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("JsonVisitor {{ values: {:?} }}", self.values))
    }
}

impl<'a> JsonVisitor<'a> {
    /// Returns a new default visitor that formats to the provided `writer`.
    ///
    /// # Arguments
    /// - `writer`: the writer to format to.
    /// - `is_empty`: whether or not any fields have been previously written to
    ///   that writer.
    pub fn new(writer: &'a mut dyn Write) -> Self {
        Self {
            values: BTreeMap::new(),
            writer,
        }
    }
}

impl<'a> crate::field::VisitFmt for JsonVisitor<'a> {
    fn writer(&mut self) -> &mut dyn fmt::Write {
        self.writer
    }
}

impl<'a> crate::field::VisitOutput<fmt::Result> for JsonVisitor<'a> {
    fn finish(self) -> fmt::Result {
        let inner = || {
            let mut serializer = Serializer::new(WriteAdaptor::new(self.writer));
            let mut ser_map = serializer.serialize_map(None)?;

            for (k, v) in self.values {
                ser_map.serialize_entry(k, &v)?;
            }

            ser_map.end()
        };

        if inner().is_err() {
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

impl<'a> field::Visit for JsonVisitor<'a> {
    /// Visit a signed 64-bit integer value.
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.values
            .insert(&field.name(), serde_json::Value::from(value));
    }

    /// Visit an unsigned 64-bit integer value.
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.values
            .insert(&field.name(), serde_json::Value::from(value));
    }

    /// Visit a boolean value.
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.values
            .insert(&field.name(), serde_json::Value::from(value));
    }

    /// Visit a string value.
    fn record_str(&mut self, field: &Field, value: &str) {
        self.values
            .insert(&field.name(), serde_json::Value::from(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        match field.name() {
            // Skip fields that are actually log metadata that have already been handled
            #[cfg(feature = "tracing-log")]
            name if name.starts_with("log.") => (),
            name if name.starts_with("r#") => {
                self.values
                    .insert(&name[2..], serde_json::Value::from(format!("{:?}", value)));
            }
            name => {
                self.values
                    .insert(name, serde_json::Value::from(format!("{:?}", value)));
            }
        };
    }
}

/// A bridge between `fmt::Write` and `io::Write`.
///
/// This is needed because tracing-subscriber's FormatEvent expects a fmt::Write
/// while serde_json's Serializer expects an io::Write.
struct WriteAdaptor<'a> {
    fmt_write: &'a mut dyn fmt::Write,
}

impl<'a> WriteAdaptor<'a> {
    fn new(fmt_write: &'a mut dyn fmt::Write) -> Self {
        Self { fmt_write }
    }
}

impl<'a> io::Write for WriteAdaptor<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let s =
            std::str::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        self.fmt_write
            .write_str(&s)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        Ok(s.as_bytes().len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> fmt::Debug for WriteAdaptor<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("WriteAdaptor { .. }")
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::fmt::{test::MockMakeWriter, time::FormatTime, CollectorBuilder};
    use tracing::{self, collect::with_default};

    use std::fmt;

    struct MockTime;
    impl FormatTime for MockTime {
        fn format_time(&self, w: &mut dyn fmt::Write) -> fmt::Result {
            write!(w, "fake time")
        }
    }

    fn collector() -> CollectorBuilder<JsonFields, Format<Json>> {
        crate::fmt::CollectorBuilder::default().json()
    }

    #[test]
    fn json() {
        let expected =
        "{\"timestamp\":\"fake time\",\"level\":\"INFO\",\"span\":{\"answer\":42,\"name\":\"json_span\",\"number\":3},\"spans\":[{\"answer\":42,\"name\":\"json_span\",\"number\":3}],\"target\":\"tracing_subscriber::fmt::format::json::test\",\"fields\":{\"message\":\"some json test\"}}\n";
        let collector = collector()
            .flatten_event(false)
            .with_current_span(true)
            .with_span_list(true);
        test_json(expected, collector, || {
            let span = tracing::span!(tracing::Level::INFO, "json_span", answer = 42, number = 3);
            let _guard = span.enter();
            tracing::info!("some json test");
        });
    }

    #[test]
    fn json_flattened_event() {
        let expected =
        "{\"timestamp\":\"fake time\",\"level\":\"INFO\",\"span\":{\"answer\":42,\"name\":\"json_span\",\"number\":3},\"spans\":[{\"answer\":42,\"name\":\"json_span\",\"number\":3}],\"target\":\"tracing_subscriber::fmt::format::json::test\",\"message\":\"some json test\"}\n";

        let collector = collector()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(true);
        test_json(expected, collector, || {
            let span = tracing::span!(tracing::Level::INFO, "json_span", answer = 42, number = 3);
            let _guard = span.enter();
            tracing::info!("some json test");
        });
    }

    #[test]
    fn json_merged_parent_fields() {
        let expected =
        "{\"timestamp\":\"fake time\",\"level\":\"INFO\",\"fields\":{\"message\":\"some json test\",\"answer\":42,\"number\":4,\"json_span.answer\":42,\"json_span.number\":3,\"parent.is_parent\":true},\"target\":\"tracing_subscriber::fmt::format::json::test\"}\n";

        let collector = collector()
            .with_current_span(false)
            .with_span_list(false)
            .merge_parent_fields(true);
        test_json(expected, collector, || {
            let parent = tracing::span!(tracing::Level::INFO, "parent", is_parent = true);
            let _parent_guard = parent.enter();
            let span = tracing::span!(tracing::Level::INFO, "json_span", answer = 42, number = 3);
            let _guard = span.enter();
            tracing::info!(answer = 42, number = 4, "some json test");
        });
    }

    #[test]
    fn json_merged_parent_fields_no_namespace() {
        let expected =
        "{\"timestamp\":\"fake time\",\"level\":\"INFO\",\"fields\":{\"message\":\"some json test\",\"answer\":42,\"number\":4,\"answer\":42,\"number\":3,\"foo\":\"bar\",\"is_true\":true},\"target\":\"tracing_subscriber::fmt::format::json::test\"}\n";

        let collector = collector()
            .with_current_span(false)
            .with_span_list(false)
            .merge_parent_fields(true)
            .namespace_parent_fields(false);
        test_json(expected, collector, || {
            let parent =
                tracing::span!(tracing::Level::INFO, "parent", foo = "bar", is_true = true);
            let _parent_guard = parent.enter();
            let span = tracing::span!(tracing::Level::INFO, "json_span", answer = 42, number = 3);
            let _guard = span.enter();
            tracing::info!(answer = 42, number = 4, "some json test");
        });
    }

    #[test]
    fn json_merged_parent_fields_flattened() {
        let expected =
        "{\"timestamp\":\"fake time\",\"level\":\"INFO\",\"message\":\"some json test\",\"answer\":42,\"number\":4,\"json_span.answer\":42,\"json_span.number\":3,\"parent.is_parent\":true,\"target\":\"tracing_subscriber::fmt::format::json::test\"}\n";

        let collector = collector()
            .with_current_span(false)
            .with_span_list(false)
            .flatten_event(true)
            .merge_parent_fields(true);
        test_json(expected, collector, || {
            let parent = tracing::span!(tracing::Level::INFO, "parent", is_parent = true);
            let _parent_guard = parent.enter();
            let span = tracing::span!(tracing::Level::INFO, "json_span", answer = 42, number = 3);
            let _guard = span.enter();
            tracing::info!(answer = 42, number = 4, "some json test");
        });
    }

    #[test]
    fn json_disabled_current_span_event() {
        let expected =
        "{\"timestamp\":\"fake time\",\"level\":\"INFO\",\"spans\":[{\"answer\":42,\"name\":\"json_span\",\"number\":3}],\"target\":\"tracing_subscriber::fmt::format::json::test\",\"fields\":{\"message\":\"some json test\"}}\n";
        let collector = collector()
            .flatten_event(false)
            .with_current_span(false)
            .with_span_list(true);
        test_json(expected, collector, || {
            let span = tracing::span!(tracing::Level::INFO, "json_span", answer = 42, number = 3);
            let _guard = span.enter();
            tracing::info!("some json test");
        });
    }

    #[test]
    fn json_disabled_span_list_event() {
        let expected =
        "{\"timestamp\":\"fake time\",\"level\":\"INFO\",\"span\":{\"answer\":42,\"name\":\"json_span\",\"number\":3},\"target\":\"tracing_subscriber::fmt::format::json::test\",\"fields\":{\"message\":\"some json test\"}}\n";
        let collector = collector()
            .flatten_event(false)
            .with_current_span(true)
            .with_span_list(false);
        test_json(expected, collector, || {
            let span = tracing::span!(tracing::Level::INFO, "json_span", answer = 42, number = 3);
            let _guard = span.enter();
            tracing::info!("some json test");
        });
    }

    #[test]
    fn json_nested_span() {
        let expected =
        "{\"timestamp\":\"fake time\",\"level\":\"INFO\",\"span\":{\"answer\":43,\"name\":\"nested_json_span\",\"number\":4},\"spans\":[{\"answer\":42,\"name\":\"json_span\",\"number\":3},{\"answer\":43,\"name\":\"nested_json_span\",\"number\":4}],\"target\":\"tracing_subscriber::fmt::format::json::test\",\"fields\":{\"message\":\"some json test\"}}\n";
        let collector = collector()
            .flatten_event(false)
            .with_current_span(true)
            .with_span_list(true);
        test_json(expected, collector, || {
            let span = tracing::span!(tracing::Level::INFO, "json_span", answer = 42, number = 3);
            let _guard = span.enter();
            let span = tracing::span!(
                tracing::Level::INFO,
                "nested_json_span",
                answer = 43,
                number = 4
            );
            let _guard = span.enter();
            tracing::info!("some json test");
        });
    }

    #[test]
    fn json_no_span() {
        let expected =
        "{\"timestamp\":\"fake time\",\"level\":\"INFO\",\"target\":\"tracing_subscriber::fmt::format::json::test\",\"fields\":{\"message\":\"some json test\"}}\n";
        let collector = collector()
            .flatten_event(false)
            .with_current_span(true)
            .with_span_list(true);
        test_json(expected, collector, || {
            tracing::info!("some json test");
        });
    }

    #[test]
    fn record_works() {
        // This test reproduces issue #707, where using `Span::record` causes
        // any events inside the span to be ignored.

        let make_writer = MockMakeWriter::default();
        let subscriber = crate::fmt()
            .json()
            .with_writer(make_writer.clone())
            .finish();

        let parse_buf = || -> serde_json::Value {
            let buf = String::from_utf8(make_writer.buf().to_vec()).unwrap();
            let json = buf
                .lines()
                .last()
                .expect("expected at least one line to be written!");
            match serde_json::from_str(&json) {
                Ok(v) => v,
                Err(e) => panic!(
                    "assertion failed: JSON shouldn't be malformed\n  error: {}\n  json: {}",
                    e, json
                ),
            }
        };

        with_default(subscriber, || {
            tracing::info!("an event outside the root span");
            assert_eq!(
                parse_buf()["fields"]["message"],
                "an event outside the root span"
            );

            let span = tracing::info_span!("the span", na = tracing::field::Empty);
            span.record("na", &"value");
            let _enter = span.enter();

            tracing::info!("an event inside the root span");
            assert_eq!(
                parse_buf()["fields"]["message"],
                "an event inside the root span"
            );
        });
    }

    fn test_json<T>(
        expected: &str,
        builder: crate::fmt::CollectorBuilder<JsonFields, Format<Json>>,
        producer: impl FnOnce() -> T,
    ) {
        let make_writer = MockMakeWriter::default();
        let collector = builder
            .with_writer(make_writer.clone())
            .with_timer(MockTime)
            .finish();

        with_default(collector, producer);

        let buf = make_writer.buf();
        let actual = std::str::from_utf8(&buf[..]).unwrap();
        assert_eq!(
            serde_json::from_str::<std::collections::HashMap<&str, serde_json::Value>>(expected)
                .unwrap(),
            serde_json::from_str(actual).unwrap()
        );
    }
}
