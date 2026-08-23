//! Bounded, schema-tolerant CZI metadata XML parsing.
//!
//! CZI metadata is vendor XML and is intentionally optional for opening an image. This module
//! retains only a bounded ordered tree and records diagnostics instead of failing callers when
//! XML is malformed or exceeds a configured limit.
//! It deliberately uses plain [`Reader`], not `NsReader`: namespace declarations remain ordinary
//! bounded attributes, while retained names use their namespace-local suffixes.

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

/// A field on a [`MetadataNode`]. Names are namespace-local.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataAttribute {
    /// Attribute name without its XML namespace prefix.
    pub name: String,
    /// Decoded attribute value.
    pub value: String,
}

/// An ordered XML element represented without namespace prefixes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataNode {
    /// Element name without its XML namespace prefix.
    pub name: String,
    /// Element attributes in document order.
    pub attributes: Vec<MetadataAttribute>,
    /// Direct text content, including CDATA, in document order.
    pub text: String,
    /// Child elements in document order.
    pub children: Vec<MetadataNode>,
}

/// A non-fatal metadata parsing or retention problem.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataDiagnostic {
    /// Human-readable diagnostic suitable for an inspector.
    pub message: String,
}

/// A bounded, optionally raw-preserving XML document.
#[derive(Clone, Debug, PartialEq)]
pub struct MetadataDocument {
    /// The parsed root, when one could be retained.
    pub root: Option<MetadataNode>,
    /// Non-fatal parsing and limit diagnostics.
    pub diagnostics: Vec<MetadataDiagnostic>,
    /// Original XML when requested and within the raw XML limit.
    pub raw_xml: Option<String>,
    /// High-value fields extracted independently of generic tree retention.
    pub summary: MetadataSummary,
}

/// Limits applied while retaining parsed metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetadataParseLimits {
    /// Maximum retained element count.
    pub max_nodes: usize,
    /// Maximum retained element nesting depth.
    pub max_depth: usize,
    /// Maximum retained decoded text bytes across all nodes.
    pub max_text_bytes: usize,
    /// Maximum retained decoded attribute bytes across all nodes.
    pub max_attribute_bytes: usize,
    /// Maximum bytes retained for XML names, values, and text.
    pub max_allocation_bytes: usize,
    /// Maximum original XML bytes copied into [`MetadataDocument::raw_xml`].
    pub max_raw_xml_bytes: usize,
    /// Maximum XML input bytes inspected for high-value summary fields.
    pub max_summary_input_bytes: usize,
    /// Maximum XML events inspected for high-value summary fields.
    pub max_summary_events: usize,
    /// Maximum nesting depth inspected for high-value summary fields.
    pub max_summary_depth: usize,
    /// Maximum decoded bytes retained for one high-value summary field.
    pub max_summary_value_bytes: usize,
    /// Maximum channels retained in the high-value summary.
    pub max_summary_channels: usize,
}

impl Default for MetadataParseLimits {
    fn default() -> Self {
        Self {
            max_nodes: 10_000,
            max_depth: 64,
            max_text_bytes: 512 * 1024,
            max_attribute_bytes: 512 * 1024,
            max_allocation_bytes: 2 * 1024 * 1024,
            max_raw_xml_bytes: 2 * 1024 * 1024,
            max_summary_input_bytes: 16 * 1024 * 1024,
            max_summary_events: 100_000,
            max_summary_depth: 64,
            max_summary_value_bytes: 1024,
            max_summary_channels: 256,
        }
    }
}

/// Options for [`MetadataDocument::parse`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MetadataParseOptions {
    /// Retain a bounded copy of the original XML for a raw inspector disclosure.
    pub retain_raw_xml: bool,
    /// Bounds for the parsed representation.
    pub limits: MetadataParseLimits,
}

impl MetadataDocument {
    /// Parse vendor metadata without allowing malformed or oversized XML to abort image opening.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn parse(xml: &str, options: MetadataParseOptions) -> Self {
        let (summary, summary_diagnostics) = extract_summary(xml, options.limits);
        let mut document = Self {
            root: None,
            diagnostics: summary_diagnostics,
            raw_xml: (options.retain_raw_xml && xml.len() <= options.limits.max_raw_xml_bytes)
                .then(|| xml.to_owned()),
            summary,
        };
        if options.retain_raw_xml && xml.len() > options.limits.max_raw_xml_bytes {
            document.diagnostics.push(MetadataDiagnostic {
                message: format!(
                    "Raw XML was not retained because it exceeds the {} byte limit.",
                    options.limits.max_raw_xml_bytes
                ),
            });
        }

        let mut reader = Reader::from_str(xml);
        let config = reader.config_mut();
        config.trim_text(true);
        config.check_end_names = true;
        config.check_comments = true;
        let mut stack = Vec::<MetadataNode>::new();
        let mut roots = Vec::<MetadataNode>::new();
        let mut counts = ParseCounts::default();

        loop {
            let event = match reader.read_event() {
                Ok(event) => event,
                Err(error) => {
                    document.diagnostics.push(MetadataDiagnostic {
                        message: format!("Malformed metadata XML: {error}"),
                    });
                    break;
                }
            };
            match event {
                Event::Start(element) => {
                    let Some(node) = build_node(
                        &element,
                        &reader,
                        options.limits,
                        &mut counts,
                        &mut document.diagnostics,
                        stack.len() + 1,
                    ) else {
                        finish_partial_tree(&mut stack, &mut roots);
                        break;
                    };
                    stack.push(node);
                }
                Event::Empty(element) => {
                    let Some(node) = build_node(
                        &element,
                        &reader,
                        options.limits,
                        &mut counts,
                        &mut document.diagnostics,
                        stack.len() + 1,
                    ) else {
                        finish_partial_tree(&mut stack, &mut roots);
                        break;
                    };
                    append_node(node, &mut stack, &mut roots, &mut document.diagnostics);
                }
                Event::End(_) => {
                    let Some(node) = stack.pop() else {
                        document.diagnostics.push(MetadataDiagnostic {
                            message: String::from(
                                "Malformed metadata XML: unexpected closing element.",
                            ),
                        });
                        break;
                    };
                    append_node(node, &mut stack, &mut roots, &mut document.diagnostics);
                }
                Event::Text(text) => {
                    let Some(node) = stack.last_mut() else {
                        continue;
                    };
                    if !retain_text(
                        node,
                        text.as_ref(),
                        options.limits,
                        &mut counts,
                        &mut document.diagnostics,
                    ) {
                        finish_partial_tree(&mut stack, &mut roots);
                        break;
                    }
                }
                Event::CData(text) => {
                    let Some(node) = stack.last_mut() else {
                        continue;
                    };
                    if !retain_text(
                        node,
                        text.as_ref(),
                        options.limits,
                        &mut counts,
                        &mut document.diagnostics,
                    ) {
                        finish_partial_tree(&mut stack, &mut roots);
                        break;
                    }
                }
                Event::GeneralRef(reference) => {
                    let Some(node) = stack.last_mut() else {
                        continue;
                    };
                    if !retain_general_reference(
                        node,
                        reference.as_ref(),
                        options.limits,
                        &mut counts,
                        &mut document.diagnostics,
                    ) {
                        finish_partial_tree(&mut stack, &mut roots);
                        break;
                    }
                }
                Event::Eof => {
                    if !stack.is_empty() {
                        document.diagnostics.push(MetadataDiagnostic {
                            message: String::from("Malformed metadata XML: unclosed element."),
                        });
                    }
                    break;
                }
                Event::Decl(_) | Event::PI(_) | Event::Comment(_) | Event::DocType(_) => {}
            }
        }
        if roots.len() > 1 {
            document.diagnostics.push(MetadataDiagnostic {
                message: String::from("Malformed metadata XML: multiple root elements."),
            });
        }
        document.root = roots.into_iter().next();
        document
    }
}

#[derive(Default)]
struct ParseCounts {
    nodes: usize,
    text_bytes: usize,
    attribute_bytes: usize,
    allocation_bytes: usize,
}

fn build_node(
    element: &BytesStart<'_>,
    reader: &Reader<&[u8]>,
    limits: MetadataParseLimits,
    counts: &mut ParseCounts,
    diagnostics: &mut Vec<MetadataDiagnostic>,
    depth: usize,
) -> Option<MetadataNode> {
    if counts.nodes >= limits.max_nodes {
        diagnostic_limit(diagnostics, "node", limits.max_nodes);
        return None;
    }
    if depth > limits.max_depth {
        diagnostic_limit(diagnostics, "depth", limits.max_depth);
        return None;
    }
    let qualified_name = element.name();
    let name_bytes = qualified_name.as_ref();
    if !reserve_allocation(counts, limits, name_bytes.len(), diagnostics) {
        return None;
    }
    let name = local_name(name_bytes);
    let mut attributes = Vec::new();
    for attribute in element.attributes() {
        let attribute = match attribute {
            Ok(attribute) => attribute,
            Err(error) => {
                diagnostics.push(MetadataDiagnostic {
                    message: format!("Malformed metadata XML attribute: {error}"),
                });
                return None;
            }
        };
        let bytes = attribute
            .key
            .as_ref()
            .len()
            .saturating_add(attribute.value.as_ref().len());
        if counts.attribute_bytes.saturating_add(bytes) > limits.max_attribute_bytes {
            diagnostic_limit(diagnostics, "attribute text", limits.max_attribute_bytes);
            return None;
        }
        if !reserve_allocation(counts, limits, bytes, diagnostics) {
            return None;
        }
        counts.attribute_bytes = counts.attribute_bytes.saturating_add(bytes);
        let attribute_name = local_name(attribute.key.as_ref());
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_or_else(
                |_| decode_text(attribute.value.as_ref()),
                std::borrow::Cow::into_owned,
            );
        attributes.push(MetadataAttribute {
            name: attribute_name,
            value,
        });
    }
    counts.nodes = counts.nodes.saturating_add(1);
    Some(MetadataNode {
        name,
        attributes,
        text: String::new(),
        children: Vec::new(),
    })
}

fn retain_text(
    node: &mut MetadataNode,
    bytes: &[u8],
    limits: MetadataParseLimits,
    counts: &mut ParseCounts,
    diagnostics: &mut Vec<MetadataDiagnostic>,
) -> bool {
    if counts.text_bytes.saturating_add(bytes.len()) > limits.max_text_bytes {
        diagnostic_limit(diagnostics, "text", limits.max_text_bytes);
        return false;
    }
    if !reserve_allocation(counts, limits, bytes.len(), diagnostics) {
        return false;
    }
    counts.text_bytes = counts.text_bytes.saturating_add(bytes.len());
    node.text.push_str(&decode_text(bytes));
    true
}

fn retain_general_reference(
    node: &mut MetadataNode,
    reference: &[u8],
    limits: MetadataParseLimits,
    counts: &mut ParseCounts,
    diagnostics: &mut Vec<MetadataDiagnostic>,
) -> bool {
    let bytes = reference.len().saturating_add(2);
    if counts.text_bytes.saturating_add(bytes) > limits.max_text_bytes {
        diagnostic_limit(diagnostics, "text", limits.max_text_bytes);
        return false;
    }
    if !reserve_allocation(counts, limits, bytes, diagnostics) {
        return false;
    }
    counts.text_bytes = counts.text_bytes.saturating_add(bytes);
    if let Some(decoded) = decode_xml_reference(reference) {
        decoded.push_to(&mut node.text);
    } else {
        node.text.push('&');
        node.text.push_str(&String::from_utf8_lossy(reference));
        node.text.push(';');
        invalid_reference_diagnostic(diagnostics, reference, false);
    }
    true
}

enum XmlReference {
    Named(&'static str),
    Character(char),
}

impl XmlReference {
    fn push_to(self, output: &mut String) {
        match self {
            Self::Named(value) => output.push_str(value),
            Self::Character(value) => output.push(value),
        }
    }
}

fn decode_xml_reference(reference: &[u8]) -> Option<XmlReference> {
    let named = match reference {
        b"amp" => Some("&"),
        b"apos" => Some("'"),
        b"gt" => Some(">"),
        b"lt" => Some("<"),
        b"quot" => Some("\""),
        _ => None,
    };
    if let Some(named) = named {
        return Some(XmlReference::Named(named));
    }
    let (digits, radix) = if let Some(digits) = reference
        .strip_prefix(b"#x")
        .or_else(|| reference.strip_prefix(b"#X"))
    {
        (digits, 16)
    } else {
        (reference.strip_prefix(b"#")?, 10)
    };
    let digits = std::str::from_utf8(digits).ok()?;
    let value = u32::from_str_radix(digits, radix).ok()?;
    char::from_u32(value)
        .filter(|character| is_xml_character(*character))
        .map(XmlReference::Character)
}

fn is_xml_character(character: char) -> bool {
    matches!(character, '\u{9}' | '\u{A}' | '\u{D}')
        || ('\u{20}'..='\u{D7FF}').contains(&character)
        || ('\u{E000}'..='\u{FFFD}').contains(&character)
        || ('\u{10000}'..='\u{10FFFF}').contains(&character)
}

fn invalid_reference_diagnostic(
    diagnostics: &mut Vec<MetadataDiagnostic>,
    reference: &[u8],
    summary: bool,
) {
    let prefix = if summary {
        "Invalid metadata summary XML reference"
    } else {
        "Invalid metadata XML reference"
    };
    let preview = String::from_utf8_lossy(&reference[..reference.len().min(64)]);
    let suffix = if reference.len() > 64 { "…" } else { "" };
    let message = format!("{prefix} '&{preview}{suffix};' was preserved literally.");
    if summary {
        summary_diagnostic_once(diagnostics, message);
    } else if !diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message.starts_with(prefix))
    {
        diagnostics.push(MetadataDiagnostic { message });
    }
}

fn reserve_allocation(
    counts: &mut ParseCounts,
    limits: MetadataParseLimits,
    bytes: usize,
    diagnostics: &mut Vec<MetadataDiagnostic>,
) -> bool {
    if counts.allocation_bytes.saturating_add(bytes) > limits.max_allocation_bytes {
        diagnostic_limit(
            diagnostics,
            "metadata allocation",
            limits.max_allocation_bytes,
        );
        return false;
    }
    counts.allocation_bytes = counts.allocation_bytes.saturating_add(bytes);
    true
}

fn diagnostic_limit(diagnostics: &mut Vec<MetadataDiagnostic>, name: &str, limit: usize) {
    diagnostics.push(MetadataDiagnostic {
        message: format!(
            "Metadata {name} limit of {limit} was reached; the structured view is partial."
        ),
    });
}

fn finish_partial_tree(stack: &mut Vec<MetadataNode>, roots: &mut Vec<MetadataNode>) {
    while let Some(node) = stack.pop() {
        if let Some(parent) = stack.last_mut() {
            parent.children.push(node);
        } else if roots.is_empty() {
            roots.push(node);
        }
    }
}

fn append_node(
    node: MetadataNode,
    stack: &mut [MetadataNode],
    roots: &mut Vec<MetadataNode>,
    diagnostics: &mut Vec<MetadataDiagnostic>,
) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else if roots.is_empty() {
        roots.push(node);
    } else {
        roots.push(node);
        diagnostics.push(MetadataDiagnostic {
            message: String::from("Malformed metadata XML: multiple root elements."),
        });
    }
}

fn local_name(name: &[u8]) -> String {
    let name = name.rsplit(|byte| *byte == b':').next().unwrap_or(name);
    String::from_utf8_lossy(name).into_owned()
}

fn decode_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    quick_xml::escape::unescape(&text)
        .map_or_else(|_| text.to_string(), std::borrow::Cow::into_owned)
}

/// Metadata associated with one logical C channel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelMetadata {
    /// C dimension index inferred from `Channel Id`, or document order when absent.
    pub index: i32,
    /// Original channel identifier, when present.
    pub id: Option<String>,
    /// Human-readable label from `Name`, child `Name`, `Id`, or a generated fallback.
    pub label: String,
    /// Dye or fluorophore name when explicitly recorded.
    pub fluor: Option<String>,
}

/// Physical pixel calibration in micrometers per pixel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysicalPixelSize {
    /// Physical X pixel width in micrometers.
    pub x_um: f64,
    /// Physical Y pixel height in micrometers.
    pub y_um: f64,
}

/// Application-oriented semantic metadata derived from a generic document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MetadataSummary {
    /// Image or document name from the standard information section.
    pub name: Option<String>,
    /// Channel labels in metadata order.
    pub channels: Vec<ChannelMetadata>,
    /// Physical X/Y pixel size when both CZI scaling values are available.
    pub pixel_size: Option<PhysicalPixelSize>,
    /// Acquisition timestamp as recorded by the CZI metadata.
    pub acquisition_date: Option<String>,
    /// Selected objective name when explicitly recorded.
    pub objective: Option<String>,
}

/// Extract common CZI metadata without coupling the generic tree to a vendor schema.
#[must_use]
pub fn summarize_metadata(document: &MetadataDocument) -> MetadataSummary {
    document.summary.clone()
}

#[derive(Default)]
struct SummaryChannel {
    depth: usize,
    id: Option<String>,
    name: Option<String>,
    fluor: Option<String>,
}

#[derive(Default)]
struct SummaryDistance {
    depth: usize,
    axis: Option<String>,
    value: Option<String>,
}

#[derive(Default)]
struct SummaryCapture {
    value: String,
    truncated: bool,
}

const MAX_SUMMARY_DIAGNOSTICS: usize = 8;

struct SummaryState {
    limits: MetadataParseLimits,
    diagnostics: Vec<MetadataDiagnostic>,
    path: Vec<String>,
    captures: Vec<Option<SummaryCapture>>,
    summary: MetadataSummary,
    channel: Option<SummaryChannel>,
    distance: Option<SummaryDistance>,
    x_meters: Option<f64>,
    y_meters: Option<f64>,
}

impl SummaryState {
    fn new(limits: MetadataParseLimits) -> Self {
        Self {
            limits,
            diagnostics: Vec::new(),
            path: Vec::new(),
            captures: Vec::new(),
            summary: MetadataSummary::default(),
            channel: None,
            distance: None,
            x_meters: None,
            y_meters: None,
        }
    }
}

#[allow(clippy::too_many_lines)]
fn extract_summary(
    xml: &str,
    limits: MetadataParseLimits,
) -> (MetadataSummary, Vec<MetadataDiagnostic>) {
    let mut state = SummaryState::new(limits);
    if xml.len() > limits.max_summary_input_bytes {
        summary_diagnostic_once(
            &mut state.diagnostics,
            format!(
                "Metadata summary input limit of {} bytes was exceeded; summary fields are unavailable.",
                limits.max_summary_input_bytes
            ),
        );
        return (state.summary, state.diagnostics);
    }
    let mut reader = Reader::from_str(xml);
    let config = reader.config_mut();
    config.trim_text(false);
    config.check_end_names = true;
    config.check_comments = true;
    let mut events = 0_usize;

    loop {
        if events >= limits.max_summary_events {
            summary_diagnostic_once(
                &mut state.diagnostics,
                format!(
                    "Metadata summary event limit of {} was reached; summary fields may be incomplete.",
                    limits.max_summary_events
                ),
            );
            break;
        }
        let event = match reader.read_event() {
            Ok(event) => event,
            Err(error) => {
                summary_diagnostic_once(
                    &mut state.diagnostics,
                    format!("Malformed metadata summary XML: {error}"),
                );
                break;
            }
        };
        events = events.saturating_add(1);
        match event {
            Event::Start(element) => {
                if state.path.len() >= limits.max_summary_depth {
                    summary_diagnostic_once(
                        &mut state.diagnostics,
                        format!(
                            "Metadata summary depth limit of {} was reached; summary fields may be incomplete.",
                            limits.max_summary_depth
                        ),
                    );
                    break;
                }
                push_summary_name(&mut state.path, element.name().as_ref());
                summary_start(&element, &reader, &mut state);
                state.captures.push(summary_capture(
                    &state.path,
                    state.channel.as_ref(),
                    state.distance.as_ref(),
                ));
            }
            Event::Empty(element) => {
                if state.path.len() >= limits.max_summary_depth {
                    summary_diagnostic_once(
                        &mut state.diagnostics,
                        format!(
                            "Metadata summary depth limit of {} was reached; summary fields may be incomplete.",
                            limits.max_summary_depth
                        ),
                    );
                    break;
                }
                push_summary_name(&mut state.path, element.name().as_ref());
                summary_start(&element, &reader, &mut state);
                summary_end(state.path.len(), &mut state);
                state.path.pop();
            }
            Event::Text(text) => {
                append_summary_fragment(
                    state.captures.last_mut().and_then(Option::as_mut),
                    &decode_text(text.as_ref()),
                    limits,
                    &mut state.diagnostics,
                );
            }
            Event::CData(text) => {
                append_summary_fragment(
                    state.captures.last_mut().and_then(Option::as_mut),
                    &String::from_utf8_lossy(text.as_ref()),
                    limits,
                    &mut state.diagnostics,
                );
            }
            Event::GeneralRef(reference) => {
                let reference = reference.as_ref();
                let mut value = String::new();
                if let Some(decoded) = decode_xml_reference(reference) {
                    decoded.push_to(&mut value);
                } else {
                    value.push('&');
                    value.push_str(&String::from_utf8_lossy(reference));
                    value.push(';');
                    invalid_reference_diagnostic(&mut state.diagnostics, reference, true);
                }
                append_summary_fragment(
                    state.captures.last_mut().and_then(Option::as_mut),
                    &value,
                    limits,
                    &mut state.diagnostics,
                );
            }
            Event::End(_) => {
                if state.path.is_empty() {
                    summary_diagnostic_once(
                        &mut state.diagnostics,
                        String::from("Malformed metadata summary XML: unexpected closing element."),
                    );
                    break;
                }
                if let Some(Some(capture)) = state.captures.pop()
                    && !capture.truncated
                {
                    summary_value(
                        &state.path,
                        capture.value.trim(),
                        &mut state.summary,
                        &mut state.channel,
                        &mut state.distance,
                    );
                }
                summary_end(state.path.len(), &mut state);
                state.path.pop();
            }
            Event::Eof => {
                if !state.path.is_empty() {
                    summary_diagnostic_once(
                        &mut state.diagnostics,
                        String::from("Malformed metadata summary XML: unclosed element."),
                    );
                }
                break;
            }
            _ => {}
        }
    }
    state.summary.pixel_size =
        physical_pixel_size(state.x_meters, state.y_meters, &mut state.diagnostics);
    (state.summary, state.diagnostics)
}

fn physical_pixel_size(
    x_meters: Option<f64>,
    y_meters: Option<f64>,
    diagnostics: &mut Vec<MetadataDiagnostic>,
) -> Option<PhysicalPixelSize> {
    let (Some(x_meters), Some(y_meters)) = (x_meters, y_meters) else {
        return None;
    };
    let x_um = x_meters * 1_000_000.0;
    let y_um = y_meters * 1_000_000.0;
    if x_um.is_finite() && x_um > 0.0 && y_um.is_finite() && y_um > 0.0 {
        Some(PhysicalPixelSize { x_um, y_um })
    } else {
        summary_diagnostic_once(
            diagnostics,
            String::from(
                "Metadata summary X/Y calibration was invalid after conversion to micrometers; pixel size is unavailable.",
            ),
        );
        None
    }
}

fn push_summary_name(path: &mut Vec<String>, bytes: &[u8]) {
    if bytes.len() <= 128 {
        path.push(local_name(bytes));
    } else {
        path.push(String::new());
    }
}

fn summary_start(element: &BytesStart<'_>, reader: &Reader<&[u8]>, state: &mut SummaryState) {
    if canonical_path(
        &state.path,
        &["information", "image", "dimensions", "channels", "channel"],
    ) {
        state.channel = Some(SummaryChannel {
            depth: state.path.len(),
            id: summary_attribute(element, reader, "id", state.limits, &mut state.diagnostics),
            name: summary_attribute(
                element,
                reader,
                "name",
                state.limits,
                &mut state.diagnostics,
            ),
            fluor: None,
        });
    }
    if canonical_path(&state.path, &["scaling", "items", "distance"])
        || canonical_path(&state.path, &["scaling", "distance"])
    {
        state.distance = Some(SummaryDistance {
            depth: state.path.len(),
            axis: summary_attribute(element, reader, "id", state.limits, &mut state.diagnostics)
                .or_else(|| {
                    summary_attribute(
                        element,
                        reader,
                        "axis",
                        state.limits,
                        &mut state.diagnostics,
                    )
                }),
            value: summary_attribute(
                element,
                reader,
                "value",
                state.limits,
                &mut state.diagnostics,
            ),
        });
    }
    if state.summary.objective.is_none()
        && canonical_path(
            &state.path,
            &["information", "instrument", "objectives", "objective"],
        )
    {
        state.summary.objective = summary_attribute(
            element,
            reader,
            "name",
            state.limits,
            &mut state.diagnostics,
        );
    }
}

fn summary_attribute(
    element: &BytesStart<'_>,
    reader: &Reader<&[u8]>,
    wanted: &str,
    limits: MetadataParseLimits,
    diagnostics: &mut Vec<MetadataDiagnostic>,
) -> Option<String> {
    for attribute in element.attributes() {
        let attribute = match attribute {
            Ok(attribute) => attribute,
            Err(error) => {
                summary_diagnostic_once(
                    diagnostics,
                    format!("Malformed metadata summary XML attribute: {error}"),
                );
                continue;
            }
        };
        let name = local_name(attribute.key.as_ref());
        if !name.eq_ignore_ascii_case(wanted) {
            continue;
        }
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_or_else(
                |_| decode_text(attribute.value.as_ref()),
                std::borrow::Cow::into_owned,
            );
        if value.len() > limits.max_summary_value_bytes {
            summary_value_limit_diagnostic(diagnostics, limits.max_summary_value_bytes);
            return None;
        }
        return Some(value);
    }
    None
}

fn summary_capture(
    path: &[String],
    channel: Option<&SummaryChannel>,
    distance: Option<&SummaryDistance>,
) -> Option<SummaryCapture> {
    let global_value = canonical_path(path, &["information", "document", "name"])
        || canonical_path(path, &["information", "document", "title"])
        || canonical_path(path, &["information", "image", "name"])
        || canonical_path(path, &["information", "image", "title"])
        || canonical_path(path, &["information", "image", "acquisitiondateandtime"])
        || canonical_path(path, &["information", "image", "acquisitiondate"])
        || canonical_path(path, &["scaling", "autoscaling", "objectivename"]);
    let channel_value = channel.is_some_and(|channel| {
        (path.len() == channel.depth.saturating_add(1)
            && path.last().is_some_and(|name| {
                name.eq_ignore_ascii_case("name")
                    || name.eq_ignore_ascii_case("fluor")
                    || name.eq_ignore_ascii_case("dyename")
            }))
            || (path.len() == channel.depth.saturating_add(2)
                && path[path.len() - 2].eq_ignore_ascii_case("fluorescencedye")
                && path[path.len() - 1].eq_ignore_ascii_case("name"))
    });
    let distance_value = distance.is_some_and(|distance| {
        path.len() == distance.depth
            || (path.len() == distance.depth.saturating_add(1)
                && path
                    .last()
                    .is_some_and(|name| name.eq_ignore_ascii_case("value")))
    });
    (global_value || channel_value || distance_value).then(SummaryCapture::default)
}

fn append_summary_fragment(
    capture: Option<&mut SummaryCapture>,
    fragment: &str,
    limits: MetadataParseLimits,
    diagnostics: &mut Vec<MetadataDiagnostic>,
) {
    let Some(capture) = capture else {
        return;
    };
    if capture.truncated {
        return;
    }
    if capture.value.len().saturating_add(fragment.len()) > limits.max_summary_value_bytes {
        capture.value.clear();
        capture.truncated = true;
        summary_value_limit_diagnostic(diagnostics, limits.max_summary_value_bytes);
        return;
    }
    capture.value.push_str(fragment);
}

fn summary_value(
    path: &[String],
    value: &str,
    summary: &mut MetadataSummary,
    channel: &mut Option<SummaryChannel>,
    distance: &mut Option<SummaryDistance>,
) {
    if value.is_empty() {
        return;
    }
    if summary.name.is_none()
        && (canonical_path(path, &["information", "document", "name"])
            || canonical_path(path, &["information", "document", "title"])
            || canonical_path(path, &["information", "image", "name"])
            || canonical_path(path, &["information", "image", "title"]))
    {
        summary.name = Some(value.to_owned());
    }
    if summary.acquisition_date.is_none()
        && (canonical_path(path, &["information", "image", "acquisitiondateandtime"])
            || canonical_path(path, &["information", "image", "acquisitiondate"]))
    {
        summary.acquisition_date = Some(value.to_owned());
    }
    if summary.objective.is_none()
        && canonical_path(path, &["scaling", "autoscaling", "objectivename"])
    {
        summary.objective = Some(value.to_owned());
    }
    if let Some(channel) = channel.as_mut() {
        if (path.len() == channel.depth.saturating_add(2)
            && path[path.len() - 2].eq_ignore_ascii_case("fluorescencedye")
            && path[path.len() - 1].eq_ignore_ascii_case("name"))
            || (path.len() == channel.depth.saturating_add(1)
                && path
                    .last()
                    .is_some_and(|name| name.eq_ignore_ascii_case("dyename")))
        {
            channel.fluor.get_or_insert_with(|| value.to_owned());
        } else if path.len() == channel.depth.saturating_add(1)
            && path
                .last()
                .is_some_and(|name| name.eq_ignore_ascii_case("name"))
        {
            channel.name.get_or_insert_with(|| value.to_owned());
        } else if path.len() == channel.depth.saturating_add(1)
            && path
                .last()
                .is_some_and(|name| name.eq_ignore_ascii_case("fluor"))
        {
            channel.fluor.get_or_insert_with(|| value.to_owned());
        }
    }
    if let Some(distance) = distance.as_mut()
        && (path
            .last()
            .is_some_and(|name| name.eq_ignore_ascii_case("value"))
            || (path
                .last()
                .is_some_and(|name| name.eq_ignore_ascii_case("distance"))
                && path.len() == distance.depth))
    {
        distance.value.get_or_insert_with(|| value.to_owned());
    }
}

fn summary_end(depth: usize, state: &mut SummaryState) {
    if state
        .channel
        .as_ref()
        .is_some_and(|pending| pending.depth == depth)
    {
        let pending = state.channel.take().expect("checked channel");
        if state.summary.channels.len() < state.limits.max_summary_channels {
            let index = pending
                .id
                .as_deref()
                .and_then(channel_index)
                .unwrap_or_else(|| i32::try_from(state.summary.channels.len()).unwrap_or(i32::MAX));
            if !state
                .summary
                .channels
                .iter()
                .any(|existing| existing.index == index)
            {
                let label = pending
                    .name
                    .clone()
                    .or_else(|| pending.fluor.clone())
                    .or_else(|| pending.id.clone())
                    .unwrap_or_else(|| format!("Channel {index}"));
                state.summary.channels.push(ChannelMetadata {
                    index,
                    id: pending.id,
                    label,
                    fluor: pending.fluor,
                });
            }
        } else {
            summary_diagnostic_once(
                &mut state.diagnostics,
                format!(
                    "Metadata summary channel limit of {} was reached; additional channels were omitted.",
                    state.limits.max_summary_channels
                ),
            );
        }
    }
    if state
        .distance
        .as_ref()
        .is_some_and(|pending| pending.depth == depth)
    {
        let pending = state.distance.take().expect("checked distance");
        let value = pending
            .value
            .as_deref()
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value > 0.0);
        match (pending.axis.as_deref().map(str::trim), value) {
            (Some(axis), Some(value))
                if axis.eq_ignore_ascii_case("x") && state.x_meters.is_none() =>
            {
                state.x_meters = Some(value);
            }
            (Some(axis), Some(value))
                if axis.eq_ignore_ascii_case("y") && state.y_meters.is_none() =>
            {
                state.y_meters = Some(value);
            }
            _ => {}
        }
    }
}

fn canonical_path(path: &[String], after_metadata: &[&str]) -> bool {
    path.len() == after_metadata.len().saturating_add(2)
        && path[0].eq_ignore_ascii_case("imagedocument")
        && path[1].eq_ignore_ascii_case("metadata")
        && path[2..]
            .iter()
            .zip(after_metadata)
            .all(|(actual, expected)| actual.eq_ignore_ascii_case(expected))
}

fn summary_value_limit_diagnostic(diagnostics: &mut Vec<MetadataDiagnostic>, limit: usize) {
    summary_diagnostic_once(
        diagnostics,
        format!(
            "Metadata summary value limit of {limit} bytes was reached; some fields were omitted."
        ),
    );
}

fn summary_diagnostic_once(diagnostics: &mut Vec<MetadataDiagnostic>, message: String) {
    if diagnostics.len() < MAX_SUMMARY_DIAGNOSTICS
        && !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == message)
    {
        diagnostics.push(MetadataDiagnostic { message });
    }
}

fn channel_index(id: &str) -> Option<i32> {
    id.rsplit(|character: char| !character.is_ascii_digit())
        .next()
        .filter(|digits| !digits.is_empty())
        .and_then(|digits| digits.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    fn parse(xml: &str) -> MetadataDocument {
        MetadataDocument::parse(
            xml,
            MetadataParseOptions {
                retain_raw_xml: true,
                ..MetadataParseOptions::default()
            },
        )
    }

    #[test]
    fn preserves_ordered_namespace_local_tree_and_raw_xml() {
        let document = parse(
            r#"<z:ImageDocument xmlns:z="urn:zeiss"><z:Metadata type="global"><z:Known>one</z:Known><z:Unknown flag="yes">two</z:Unknown><z:Unknown>three</z:Unknown></z:Metadata></z:ImageDocument>"#,
        );
        let root = document.root.expect("root");
        assert_eq!(root.name, "ImageDocument");
        assert_eq!(root.children[0].name, "Metadata");
        assert_eq!(root.children[0].attributes[0].name, "type");
        assert_eq!(root.children[0].children[0].name, "Known");
        assert_eq!(root.children[0].children[1].name, "Unknown");
        assert_eq!(root.children[0].children[2].text, "three");
        assert_eq!(
            document.raw_xml.as_deref(),
            Some(
                r#"<z:ImageDocument xmlns:z="urn:zeiss"><z:Metadata type="global"><z:Known>one</z:Known><z:Unknown flag="yes">two</z:Unknown><z:Unknown>three</z:Unknown></z:Metadata></z:ImageDocument>"#
            )
        );
    }

    #[test]
    fn summarizes_realistic_channels_and_scaling_with_namespaces() {
        let document = parse(
            r#"<ImageDocument xmlns="http://www.zeiss.com/czi"><Metadata><Information><Image><Dimensions><Channels><Channel Id="Channel:0" Name="DAPI"/><Channel Id="Channel:1"><Name>FITC</Name></Channel><Channel Id="Channel:2"/></Channels></Dimensions></Image></Information><Scaling><Items><Distance Id="X"><Value>2.5e-7</Value></Distance><Distance id="y" Value="5e-7"/></Items></Scaling></Metadata></ImageDocument>"#,
        );
        let summary = summarize_metadata(&document);
        assert_eq!(
            summary.channels,
            vec![
                ChannelMetadata {
                    index: 0,
                    id: Some(String::from("Channel:0")),
                    label: String::from("DAPI"),
                    fluor: None,
                },
                ChannelMetadata {
                    index: 1,
                    id: Some(String::from("Channel:1")),
                    label: String::from("FITC"),
                    fluor: None,
                },
                ChannelMetadata {
                    index: 2,
                    id: Some(String::from("Channel:2")),
                    label: String::from("Channel:2"),
                    fluor: None,
                },
            ]
        );
        assert_eq!(
            summary.pixel_size,
            Some(PhysicalPixelSize {
                x_um: 0.25,
                y_um: 0.5
            })
        );
    }

    #[test]
    fn summary_uses_only_standard_paths_and_first_calibration_values() {
        let document = parse(
            r#"<ImageDocument><Metadata><Extension><Channel Id="Channel:9" Name="ignore"/><Scaling><Distance Id="X" Value="9"/></Scaling></Extension><Information><Image><Dimensions><Channels><Channel Id="Channel:0" Name="DAPI"/></Channels></Dimensions></Image></Information><Scaling><Items><Distance Id="X" Value="2e-7"/><Distance Id="X" Value="9e-7"/><Distance Id="Y" Value="3e-7"/></Items></Scaling></Metadata></ImageDocument>"#,
        );
        let summary = summarize_metadata(&document);
        assert_eq!(summary.channels.len(), 1);
        assert_eq!(summary.channels[0].label, "DAPI");
        let pixel_size = summary.pixel_size.expect("standard calibration");
        assert!((pixel_size.x_um - 0.2).abs() < f64::EPSILON);
        assert!((pixel_size.y_um - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn malformed_xml_returns_partial_document_and_diagnostic() {
        let document = parse("<ImageDocument><Channel Name=\"DAPI\"></ImageDocument>");
        assert!(document.root.is_none());
        assert!(
            document
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("Malformed"))
        );
    }

    #[test]
    fn parser_stops_at_node_depth_text_and_allocation_limits() {
        let node_limit = MetadataDocument::parse(
            "<Root><A/><B/></Root>",
            MetadataParseOptions {
                limits: MetadataParseLimits {
                    max_nodes: 2,
                    ..MetadataParseLimits::default()
                },
                ..MetadataParseOptions::default()
            },
        );
        assert!(node_limit.diagnostics[0].message.contains("node limit"));

        let depth_limit = MetadataDocument::parse(
            "<Root><A><B/></A></Root>",
            MetadataParseOptions {
                limits: MetadataParseLimits {
                    max_depth: 2,
                    ..MetadataParseLimits::default()
                },
                ..MetadataParseOptions::default()
            },
        );
        assert!(depth_limit.diagnostics[0].message.contains("depth limit"));

        let text_limit = MetadataDocument::parse(
            "<Root>1234</Root>",
            MetadataParseOptions {
                limits: MetadataParseLimits {
                    max_text_bytes: 3,
                    ..MetadataParseLimits::default()
                },
                ..MetadataParseOptions::default()
            },
        );
        assert!(text_limit.diagnostics[0].message.contains("text limit"));

        let allocation_limit = MetadataDocument::parse(
            "<Root attribute=\"long\"/>",
            MetadataParseOptions {
                limits: MetadataParseLimits {
                    max_allocation_bytes: 4,
                    ..MetadataParseLimits::default()
                },
                ..MetadataParseOptions::default()
            },
        );
        assert!(
            allocation_limit.diagnostics[0]
                .message
                .contains("allocation limit")
        );
        assert_eq!(allocation_limit.root, None);
    }

    #[test]
    fn node_limit_keeps_partial_tree_and_summary_beyond_ten_thousand_nodes() {
        let mut xml = String::from("<ImageDocument><Metadata><Experiment>");
        for index in 0..10_050 {
            write!(xml, "<Setting Index=\"{index}\"/>").expect("write synthetic XML");
        }
        xml.push_str(
            "</Experiment><Information><Document><Name>HADA bridge</Name></Document><Image><AcquisitionDateAndTime>2025-06-02T18:23:49Z</AcquisitionDateAndTime><Dimensions><Channels><Channel Id=\"Channel:0\" Name=\"Phase PH3\"><Fluor>TL Phase</Fluor></Channel><Channel Id=\"Channel:1\"><Name>AF405</Name><DyeName>Alexa Fluor 405</DyeName></Channel></Channels></Dimensions></Image><Instrument><Objectives><Objective Id=\"Objective:1\" Name=\"Plan-Apochromat 63x/1.40 Oil\"/></Objectives></Instrument></Information><Scaling><Items><Distance Id=\"X\"><Value>1.031746e-7</Value><DefaultUnitFormat>µm</DefaultUnitFormat></Distance><Distance Id=\"Y\" Value=\"1.031746e-7\"/></Items></Scaling></Metadata></ImageDocument>",
        );

        let document = parse(&xml);
        let root = document.root.as_ref().expect("partial root");
        assert_eq!(root.name, "ImageDocument");
        assert!(!root.children.is_empty());
        assert!(document.diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("node limit of 10000")
                && diagnostic.message.contains("partial")
        }));
        assert!(!has_summary_diagnostic(&document, "event limit"));
        let summary = summarize_metadata(&document);
        assert_eq!(summary.name.as_deref(), Some("HADA bridge"));
        assert_eq!(summary.channels.len(), 2);
        assert_eq!(summary.channels[1].label, "AF405");
        assert_eq!(
            summary.channels[1].fluor.as_deref(),
            Some("Alexa Fluor 405")
        );
        assert_eq!(
            summary.objective.as_deref(),
            Some("Plan-Apochromat 63x/1.40 Oil")
        );
        assert_eq!(
            summary.acquisition_date.as_deref(),
            Some("2025-06-02T18:23:49Z")
        );
        let pixel_size = summary.pixel_size.expect("X/Y scaling");
        assert!((pixel_size.x_um - 0.103_174_6).abs() < 1e-12);
        assert!((pixel_size.y_um - 0.103_174_6).abs() < 1e-12);
    }

    #[test]
    fn summary_tolerates_namespaced_channel_and_calibration_variants() {
        let document = parse(
            r#"<z:ImageDocument xmlns:z="urn:zeiss"><z:Metadata><z:Information><z:Image><z:AcquisitionDate>2024-01-02</z:AcquisitionDate><z:Dimensions><z:Channels><z:Channel id="C0"><z:Detector><z:Name>Nested camera name</z:Name></z:Detector><z:Name>HADA</z:Name><z:FluorescenceDye><z:Name>Alexa Fluor 405</z:Name></z:FluorescenceDye></z:Channel></z:Channels></z:Dimensions></z:Image><z:Instrument><z:Objectives><z:Objective Name="63x Oil"/></z:Objectives></z:Instrument></z:Information><z:Scaling><z:Distance axis="x" value="2.5e-7"/><z:Distance Axis="Y">5e-7</z:Distance></z:Scaling></z:Metadata></z:ImageDocument>"#,
        );
        let summary = summarize_metadata(&document);
        assert_eq!(summary.channels[0].label, "HADA");
        assert_eq!(
            summary.channels[0].fluor.as_deref(),
            Some("Alexa Fluor 405")
        );
        assert_eq!(summary.objective.as_deref(), Some("63x Oil"));
        assert_eq!(summary.acquisition_date.as_deref(), Some("2024-01-02"));
        assert_eq!(
            summary.pixel_size,
            Some(PhysicalPixelSize {
                x_um: 0.25,
                y_um: 0.5
            })
        );
    }

    #[test]
    fn summary_accumulates_text_cdata_and_references_per_field() {
        let document = parse(
            r#"<ImageDocument><Metadata><Information><Document><Name>H&amp;<![CDATA[E]]></Name></Document><Image><Dimensions><Channels><Channel Id="Channel:0"><Name>AF<![CDATA[405]]></Name><Fluor>Alexa &amp; Fluor</Fluor></Channel></Channels></Dimensions></Image></Information><Scaling><Items><Distance Id="X"><Value>2.5<![CDATA[e-7]]></Value></Distance><Distance Id="Y">5<![CDATA[e-7]]></Distance></Items></Scaling></Metadata></ImageDocument>"#,
        );
        let summary = summarize_metadata(&document);
        assert_eq!(summary.name.as_deref(), Some("H&E"));
        assert_eq!(summary.channels[0].label, "AF405");
        assert_eq!(summary.channels[0].fluor.as_deref(), Some("Alexa & Fluor"));
        assert_eq!(
            summary.pixel_size,
            Some(PhysicalPixelSize {
                x_um: 0.25,
                y_um: 0.5
            })
        );
    }

    #[test]
    fn numeric_xml_references_decode_in_tree_and_summary_fields() {
        let document = parse(
            r#"<ImageDocument><Metadata><Information><Document><Name>H&#38;E / H&#x26;E</Name></Document></Information><Scaling><Distance Id="X">2.5&#101;-7</Distance><Distance Id="Y">5&#x65;-7</Distance></Scaling></Metadata></ImageDocument>"#,
        );
        let root = document.root.as_ref().expect("tree root");
        let name = &root.children[0].children[0].children[0].children[0];
        assert_eq!(name.text, "H&E / H&E");
        assert_eq!(document.summary.name.as_deref(), Some("H&E / H&E"));
        assert_eq!(
            document.summary.pixel_size,
            Some(PhysicalPixelSize {
                x_um: 0.25,
                y_um: 0.5
            })
        );
    }

    #[test]
    fn invalid_xml_references_are_preserved_and_diagnosed() {
        let document = parse(
            r"<ImageDocument><Metadata><Information><Document><Name>H&#0;E</Name></Document></Information></Metadata></ImageDocument>",
        );
        assert_eq!(document.summary.name.as_deref(), Some("H&#0;E"));
        assert!(document.diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("Invalid metadata XML reference")
        }));
        assert!(document.diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("Invalid metadata summary XML reference")
        }));
    }

    #[test]
    fn overflowing_micrometer_conversion_is_unavailable_and_diagnostic() {
        let document = parse(
            r#"<ImageDocument><Metadata><Scaling><Distance Id="X" Value="1e308"/><Distance Id="Y" Value="1e308"/></Scaling></Metadata></ImageDocument>"#,
        );
        assert_eq!(document.summary.pixel_size, None);
        assert!(has_summary_diagnostic(
            &document,
            "after conversion to micrometers"
        ));
    }

    #[test]
    fn summary_requires_the_canonical_image_document_metadata_root() {
        let document = parse(
            r#"<ImageDocument><Metadata><Extension><Metadata><Information><Document><Name>False nested name</Name></Document><Image><Dimensions><Channels><Channel Id="Channel:9" Name="False channel"/></Channels></Dimensions></Image></Information><Scaling><Distance Id="X" Value="9e-7"/><Distance Id="Y" Value="9e-7"/></Scaling></Metadata></Extension><Information><Document><Name>Real global name</Name></Document><Image><Dimensions><Channels><Channel Id="Channel:0" Name="Real channel"/></Channels></Dimensions></Image></Information><Scaling><Distance Id="X" Value="2e-7"/><Distance Id="Y" Value="3e-7"/></Scaling></Metadata></ImageDocument>"#,
        );
        let summary = summarize_metadata(&document);
        assert_eq!(summary.name.as_deref(), Some("Real global name"));
        assert_eq!(summary.channels.len(), 1);
        assert_eq!(summary.channels[0].label, "Real channel");
        let pixel_size = summary.pixel_size.expect("real global scaling");
        assert!((pixel_size.x_um - 0.2).abs() < f64::EPSILON);
        assert!((pixel_size.y_um - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn summary_reports_input_event_depth_and_malformed_limits() {
        let xml = "<ImageDocument><Metadata><Information><Document><Name>bounded</Name></Document></Information></Metadata></ImageDocument>";
        let input = MetadataDocument::parse(
            xml,
            MetadataParseOptions {
                limits: MetadataParseLimits {
                    max_summary_input_bytes: xml.len() - 1,
                    ..MetadataParseLimits::default()
                },
                ..MetadataParseOptions::default()
            },
        );
        assert!(input.summary.name.is_none());
        assert!(has_summary_diagnostic(&input, "input limit"));

        let events = MetadataDocument::parse(
            xml,
            MetadataParseOptions {
                limits: MetadataParseLimits {
                    max_summary_events: 3,
                    ..MetadataParseLimits::default()
                },
                ..MetadataParseOptions::default()
            },
        );
        assert!(has_summary_diagnostic(&events, "event limit"));

        let depth = MetadataDocument::parse(
            xml,
            MetadataParseOptions {
                limits: MetadataParseLimits {
                    max_summary_depth: 2,
                    ..MetadataParseLimits::default()
                },
                ..MetadataParseOptions::default()
            },
        );
        assert!(has_summary_diagnostic(&depth, "depth limit"));

        let malformed = parse(
            "<ImageDocument><Metadata><Information><Document><Name>broken</Document></Information></Metadata></ImageDocument>",
        );
        assert!(has_summary_diagnostic(
            &malformed,
            "Malformed metadata summary XML"
        ));
    }

    #[test]
    fn summary_reports_value_and_channel_retention_limits() {
        let value = MetadataDocument::parse(
            "<ImageDocument><Metadata><Information><Document><Name>too long</Name></Document></Information></Metadata></ImageDocument>",
            MetadataParseOptions {
                limits: MetadataParseLimits {
                    max_summary_value_bytes: 3,
                    ..MetadataParseLimits::default()
                },
                ..MetadataParseOptions::default()
            },
        );
        assert!(value.summary.name.is_none());
        assert!(has_summary_diagnostic(&value, "value limit"));

        let channels = MetadataDocument::parse(
            "<ImageDocument><Metadata><Information><Image><Dimensions><Channels><Channel Id=\"Channel:0\" Name=\"zero\"/><Channel Id=\"Channel:1\" Name=\"one\"/><Channel Id=\"Channel:2\" Name=\"two\"/></Channels></Dimensions></Image></Information></Metadata></ImageDocument>",
            MetadataParseOptions {
                limits: MetadataParseLimits {
                    max_summary_channels: 2,
                    ..MetadataParseLimits::default()
                },
                ..MetadataParseOptions::default()
            },
        );
        assert_eq!(channels.summary.channels.len(), 2);
        assert!(has_summary_diagnostic(&channels, "channel limit"));
    }

    fn has_summary_diagnostic(document: &MetadataDocument, text: &str) -> bool {
        document
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains(text))
    }

    #[test]
    fn raw_xml_is_optional_and_bounded() {
        let document = MetadataDocument::parse("<Root/>", MetadataParseOptions::default());
        assert_eq!(document.raw_xml, None);
        let document = MetadataDocument::parse(
            "<Root>long</Root>",
            MetadataParseOptions {
                retain_raw_xml: true,
                limits: MetadataParseLimits {
                    max_raw_xml_bytes: 8,
                    ..MetadataParseLimits::default()
                },
            },
        );
        assert_eq!(document.raw_xml, None);
        assert!(document.diagnostics[0].message.contains("Raw XML"));
    }
}
