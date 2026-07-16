use std::collections::HashSet;
use std::fmt;

use serde::de::{DeserializeSeed, Error as DeError, MapAccess, SeqAccess, Visitor};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, PartialEq)]
pub struct StructuredDataNode {
    pub id: String,
    pub value: StructuredDataValue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StructuredDataObjectEntry {
    pub key: String,
    pub value: StructuredDataNode,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StructuredDataValue {
    Object(Vec<StructuredDataObjectEntry>),
    Array(Vec<StructuredDataNode>),
    Scalar(StructuredDataScalar),
}

#[derive(Debug, Clone, PartialEq)]
pub enum StructuredDataScalar {
    String(String),
    Number(String),
    Boolean(bool),
    Null,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredDataError {
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonOutlineRows {
    pub row_offset: usize,
    pub row_count: usize,
    pub content_width_columns: usize,
    pub rows: Vec<JsonOutlineRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonOutline {
    content_width_columns: usize,
    rows: Vec<JsonOutlineRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonOutlineRow {
    pub id: String,
    pub line_number: usize,
    pub depth: usize,
    pub prefix_segments: Vec<JsonSegment>,
    pub disclosure: Option<JsonDisclosure>,
    pub suffix_segments: Vec<JsonSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonDisclosure {
    pub node_id: String,
    pub collapsed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonSegment {
    pub text: String,
    pub role: JsonSegmentRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonSegmentRole {
    Key,
    String,
    Number,
    Boolean,
    Null,
    Punctuation,
}

impl StructuredDataError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for StructuredDataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StructuredDataError {}

pub fn parse_json(input: &str) -> Result<StructuredDataNode, StructuredDataError> {
    let mut deserializer = serde_json::Deserializer::from_str(input);
    let node = NodeSeed {
        path: ROOT_NODE_ID.to_string(),
    }
    .deserialize(&mut deserializer)
    .map_err(|error| StructuredDataError::new(error.to_string()))?;
    deserializer
        .end()
        .map_err(|error| StructuredDataError::new(error.to_string()))?;
    Ok(node)
}

pub fn json_outline_rows(
    root: &StructuredDataNode,
    collapsed_ids: &HashSet<String>,
    start: usize,
    len: usize,
) -> JsonOutlineRows {
    json_outline(root, collapsed_ids).page(start, len)
}

pub fn json_outline(root: &StructuredDataNode, collapsed_ids: &HashSet<String>) -> JsonOutline {
    let mut builder = JsonOutlineBuilder {
        collapsed_ids,
        rows: Vec::new(),
        content_width_columns: 0,
    };
    builder.append(root, None, 0, false);
    JsonOutline {
        content_width_columns: builder.content_width_columns,
        rows: builder.rows,
    }
}

impl JsonOutline {
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn content_width_columns(&self) -> usize {
        self.content_width_columns
    }

    pub fn rows(&self) -> &[JsonOutlineRow] {
        &self.rows
    }

    pub fn page(&self, start: usize, len: usize) -> JsonOutlineRows {
        let row_count = self.row_count();
        let row_offset = start.min(row_count);
        let rows = self.rows[row_offset..row_count.min(row_offset.saturating_add(len))].to_vec();
        JsonOutlineRows {
            row_offset,
            row_count,
            content_width_columns: self.content_width_columns,
            rows,
        }
    }
}

impl JsonOutlineRow {
    pub fn display_width_columns(&self) -> usize {
        self.depth * JSON_DEPTH_INDENT_COLUMNS
            + segments_width_columns(&self.prefix_segments)
            + self
                .disclosure
                .as_ref()
                .map(|_| JSON_DISCLOSURE_COLUMNS + JSON_DISCLOSURE_GAP_COLUMNS)
                .unwrap_or(0)
            + segments_width_columns(&self.suffix_segments)
    }

    pub fn selectable_text(&self) -> String {
        let mut text = String::with_capacity(
            self.prefix_segments
                .iter()
                .chain(&self.suffix_segments)
                .map(|segment| segment.text.len())
                .sum::<usize>()
                + if self.disclosure.is_some() {
                    JSON_DISCLOSURE_COLUMNS + JSON_DISCLOSURE_GAP_COLUMNS
                } else {
                    0
                },
        );
        for segment in &self.prefix_segments {
            text.push_str(&segment.text);
        }
        if self.disclosure.is_some() {
            text.push_str("  ");
        }
        for segment in &self.suffix_segments {
            text.push_str(&segment.text);
        }
        text
    }
}

const ROOT_NODE_ID: &str = "root";

struct NodeSeed {
    path: String,
}

impl<'de> DeserializeSeed<'de> for NodeSeed {
    type Value = StructuredDataNode;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(NodeVisitor { path: self.path })
    }
}

struct NodeVisitor {
    path: String,
}

impl<'de> Visitor<'de> for NodeVisitor {
    type Value = StructuredDataNode;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            let child_path = object_child_path(&self.path, &key);
            let value = map.next_value_seed(NodeSeed { path: child_path })?;
            entries.push(StructuredDataObjectEntry { key, value });
        }
        Ok(StructuredDataNode {
            id: self.path,
            value: StructuredDataValue::Object(entries),
        })
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut children = Vec::new();
        let mut index = 0;
        while let Some(value) = seq.next_element_seed(NodeSeed {
            path: array_child_path(&self.path, index),
        })? {
            children.push(value);
            index += 1;
        }
        Ok(StructuredDataNode {
            id: self.path,
            value: StructuredDataValue::Array(children),
        })
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        Ok(self.scalar(StructuredDataScalar::String(value.to_string())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        Ok(self.scalar(StructuredDataScalar::String(value)))
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        Ok(self.scalar(StructuredDataScalar::Boolean(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        Ok(self.scalar(StructuredDataScalar::Number(value.to_string())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        Ok(self.scalar(StructuredDataScalar::Number(value.to_string())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        Ok(self.scalar(StructuredDataScalar::Number(value.to_string())))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        Ok(self.scalar(StructuredDataScalar::Null))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        Ok(self.scalar(StructuredDataScalar::Null))
    }
}

impl NodeVisitor {
    fn scalar(self, scalar: StructuredDataScalar) -> StructuredDataNode {
        StructuredDataNode {
            id: self.path,
            value: StructuredDataValue::Scalar(scalar),
        }
    }
}

struct JsonOutlineBuilder<'a> {
    collapsed_ids: &'a HashSet<String>,
    rows: Vec<JsonOutlineRow>,
    content_width_columns: usize,
}

struct JsonContainer<'a> {
    opener: &'static str,
    closer: &'static str,
    children: Vec<(Option<&'a str>, &'a StructuredDataNode)>,
}

impl JsonOutlineBuilder<'_> {
    fn append(
        &mut self,
        node: &StructuredDataNode,
        key: Option<&str>,
        depth: usize,
        trailing_comma: bool,
    ) {
        match &node.value {
            StructuredDataValue::Object(children) => {
                let children = children
                    .iter()
                    .map(|child| (Some(child.key.as_str()), &child.value))
                    .collect::<Vec<_>>();
                self.append_container(
                    node,
                    key,
                    depth,
                    trailing_comma,
                    JsonContainer {
                        opener: "{",
                        closer: "}",
                        children,
                    },
                );
            }
            StructuredDataValue::Array(children) => {
                let children = children
                    .iter()
                    .map(|child| (None, child))
                    .collect::<Vec<_>>();
                self.append_container(
                    node,
                    key,
                    depth,
                    trailing_comma,
                    JsonContainer {
                        opener: "[",
                        closer: "]",
                        children,
                    },
                );
            }
            StructuredDataValue::Scalar(scalar) => {
                let mut suffix = Vec::from([scalar_segment(scalar)]);
                suffix.extend(comma(trailing_comma));
                self.append_row(
                    format!("{}.value", node.id),
                    depth,
                    key_prefix(key),
                    None,
                    suffix,
                );
            }
        }
    }

    fn append_container(
        &mut self,
        node: &StructuredDataNode,
        key: Option<&str>,
        depth: usize,
        trailing_comma: bool,
        container: JsonContainer<'_>,
    ) {
        let collapsed = self.collapsed_ids.contains(&node.id);
        let disclosure = Some(JsonDisclosure {
            node_id: node.id.clone(),
            collapsed,
        });
        if collapsed {
            let mut suffix = Vec::from([segment(
                format!("{} ... {}", container.opener, container.closer),
                JsonSegmentRole::Punctuation,
            )]);
            suffix.extend(comma(trailing_comma));
            self.append_row(
                format!("{}.collapsed", node.id),
                depth,
                key_prefix(key),
                disclosure,
                suffix,
            );
            return;
        }

        self.append_row(
            format!("{}.open", node.id),
            depth,
            key_prefix(key),
            disclosure,
            Vec::from([segment(container.opener, JsonSegmentRole::Punctuation)]),
        );

        for (idx, (child_key, child)) in container.children.iter().enumerate() {
            self.append(
                child,
                *child_key,
                depth + 1,
                idx < container.children.len().saturating_sub(1),
            );
        }

        let mut suffix = Vec::from([segment(container.closer, JsonSegmentRole::Punctuation)]);
        suffix.extend(comma(trailing_comma));
        self.append_row(
            format!("{}.close", node.id),
            depth,
            Vec::new(),
            None,
            suffix,
        );
    }

    fn append_row(
        &mut self,
        id: String,
        depth: usize,
        prefix_segments: Vec<JsonSegment>,
        disclosure: Option<JsonDisclosure>,
        suffix_segments: Vec<JsonSegment>,
    ) {
        let line_number = self.rows.len() + 1;
        let row = JsonOutlineRow {
            id,
            line_number,
            depth,
            prefix_segments,
            disclosure,
            suffix_segments,
        };
        self.content_width_columns = self.content_width_columns.max(row.display_width_columns());
        self.rows.push(row);
    }
}

const JSON_DEPTH_INDENT_COLUMNS: usize = 4;
const JSON_DISCLOSURE_COLUMNS: usize = 1;
const JSON_DISCLOSURE_GAP_COLUMNS: usize = 1;

fn segments_width_columns(segments: &[JsonSegment]) -> usize {
    segments
        .iter()
        .map(|segment| UnicodeWidthStr::width(segment.text.as_str()))
        .sum()
}

fn key_prefix(key: Option<&str>) -> Vec<JsonSegment> {
    match key {
        Some(key) => Vec::from([
            segment(json_quoted(key), JsonSegmentRole::Key),
            segment(" : ", JsonSegmentRole::Punctuation),
        ]),
        None => Vec::new(),
    }
}

fn scalar_segment(scalar: &StructuredDataScalar) -> JsonSegment {
    match scalar {
        StructuredDataScalar::String(value) => segment(json_quoted(value), JsonSegmentRole::String),
        StructuredDataScalar::Number(value) => segment(value, JsonSegmentRole::Number),
        StructuredDataScalar::Boolean(value) => segment(
            if *value { "true" } else { "false" },
            JsonSegmentRole::Boolean,
        ),
        StructuredDataScalar::Null => segment("null", JsonSegmentRole::Null),
    }
}

fn comma(needed: bool) -> Vec<JsonSegment> {
    if needed {
        Vec::from([segment(",", JsonSegmentRole::Punctuation)])
    } else {
        Vec::new()
    }
}

fn segment(text: impl Into<String>, role: JsonSegmentRole) -> JsonSegment {
    JsonSegment {
        text: text.into(),
        role,
    }
}

fn object_child_path(parent: &str, key: &str) -> String {
    format!("{parent}/{}", json_pointer_escape(key))
}

fn array_child_path(parent: &str, index: usize) -> String {
    format!("{parent}/{index}")
}

fn json_pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn json_quoted(value: &str) -> String {
    let mut result = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\u{08}' => result.push_str("\\b"),
            '\u{0C}' => result.push_str("\\f"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\u{00}'..='\u{1F}' => {
                result.push_str(&format!("\\u{:04X}", ch as u32));
            }
            _ => result.push(ch),
        }
    }
    result.push('"');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_preserves_object_order() {
        let root = parse_json(r#"{"b":1,"a":2,"nested":{"z":true}}"#).unwrap();
        let StructuredDataValue::Object(entries) = root.value else {
            panic!("root is object");
        };
        assert_eq!(entries[0].key, "b");
        assert_eq!(entries[1].key, "a");
        assert_eq!(entries[2].key, "nested");
    }

    #[test]
    fn json_outline_rows_are_left_aligned_and_paged() {
        let root = parse_json(r#"{"name":"reef","features":["preview","git"],"ok":true}"#).unwrap();
        let rows = json_outline_rows(&root, &HashSet::new(), 1, 3);
        assert_eq!(rows.row_offset, 1);
        assert_eq!(rows.row_count, 8);
        assert!(rows.content_width_columns >= 20);
        assert_eq!(rows.rows.len(), 3);
        assert_eq!(rows.rows[0].line_number, 2);
        assert_eq!(rows.rows[0].depth, 1);
        assert_eq!(rows.rows[0].prefix_segments[0].text, "\"name\"");
    }

    #[test]
    fn json_outline_respects_collapsed_ids() {
        let root = parse_json(r#"{"items":[{"id":1},{"id":2}]}"#).unwrap();
        let mut collapsed = HashSet::new();
        collapsed.insert("root/items".to_string());

        let rows = json_outline_rows(&root, &collapsed, 0, 16);

        assert!(rows.rows.iter().any(|row| row.id == "root/items.collapsed"));
        assert!(!rows.rows.iter().any(|row| row.id == "root/items/0.open"));
    }

    #[test]
    fn parse_json_uses_non_ambiguous_node_ids() {
        let root =
            parse_json(r#"{"a[0]":"object-key","a":["array-child"],"a/b":1,"a~b":2}"#).unwrap();
        let StructuredDataValue::Object(entries) = &root.value else {
            panic!("root is object");
        };

        let object_key_id = entries
            .iter()
            .find(|entry| entry.key == "a[0]")
            .map(|entry| entry.value.id.as_str())
            .unwrap();
        let array = entries
            .iter()
            .find(|entry| entry.key == "a")
            .map(|entry| &entry.value)
            .unwrap();
        let StructuredDataValue::Array(array_children) = &array.value else {
            panic!("a is array");
        };
        let slash_key_id = entries
            .iter()
            .find(|entry| entry.key == "a/b")
            .map(|entry| entry.value.id.as_str())
            .unwrap();
        let tilde_key_id = entries
            .iter()
            .find(|entry| entry.key == "a~b")
            .map(|entry| entry.value.id.as_str())
            .unwrap();

        assert_eq!(object_key_id, "root/a[0]");
        assert_eq!(array_children[0].id, "root/a/0");
        assert_ne!(object_key_id, array_children[0].id);
        assert_eq!(slash_key_id, "root/a~1b");
        assert_eq!(tilde_key_id, "root/a~0b");
    }

    #[test]
    fn json_outline_pages_from_cached_rows() {
        let root = parse_json(r#"{"name":"reef","features":["preview","git"],"ok":true}"#).unwrap();
        let outline = json_outline(&root, &HashSet::new());

        let first = outline.page(1, 3);
        let second = outline.page(3, 2);

        assert_eq!(outline.row_count(), 8);
        assert_eq!(outline.content_width_columns(), first.content_width_columns);
        assert_eq!(first.content_width_columns, second.content_width_columns);
        assert_eq!(first.row_offset, 1);
        assert_eq!(first.rows[0].line_number, 2);
        assert_eq!(second.row_offset, 3);
        assert_eq!(second.rows[0].line_number, 4);
    }

    #[test]
    fn outline_row_selectable_text_matches_rendered_columns_without_indentation() {
        let root = parse_json(r#"{"items":["reef"]}"#).unwrap();
        let outline = json_outline(&root, &HashSet::new());
        let items = outline
            .rows()
            .iter()
            .find(|row| row.id == "root/items.open")
            .unwrap();

        assert_eq!(items.selectable_text(), "\"items\" :   [");
    }
}
