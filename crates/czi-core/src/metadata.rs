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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataDocument {
    /// The parsed root, when one could be retained.
    pub root: Option<MetadataNode>,
    /// Non-fatal parsing and limit diagnostics.
    pub diagnostics: Vec<MetadataDiagnostic>,
    /// Original XML when requested and within the raw XML limit.
    pub raw_xml: Option<String>,
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
        let mut document = Self {
            root: None,
            diagnostics: Vec::new(),
            raw_xml: (options.retain_raw_xml && xml.len() <= options.limits.max_raw_xml_bytes)
                .then(|| xml.to_owned()),
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
    if let Some(decoded) = predefined_reference(reference) {
        node.text.push_str(decoded);
    } else {
        node.text.push('&');
        node.text.push_str(&String::from_utf8_lossy(reference));
        node.text.push(';');
    }
    true
}

fn predefined_reference(reference: &[u8]) -> Option<&'static str> {
    match reference {
        b"amp" => Some("&"),
        b"apos" => Some("'"),
        b"gt" => Some(">"),
        b"lt" => Some("<"),
        b"quot" => Some("\""),
        _ => None,
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
        message: format!("Metadata {name} limit of {limit} was reached; parsing stopped."),
    });
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
    /// Channel labels in metadata order.
    pub channels: Vec<ChannelMetadata>,
    /// Physical X/Y pixel size when both CZI scaling values are available.
    pub pixel_size: Option<PhysicalPixelSize>,
}

/// Extract common CZI metadata without coupling the generic tree to a vendor schema.
#[must_use]
pub fn summarize_metadata(document: &MetadataDocument) -> MetadataSummary {
    let mut summary = MetadataSummary::default();
    if let Some(root) = document.root.as_ref() {
        let mut path = Vec::new();
        collect_channels(root, &mut path, &mut summary.channels);
        let mut x_meters = None;
        let mut y_meters = None;
        collect_scaling(root, &mut path, &mut x_meters, &mut y_meters);
        summary.pixel_size = match (x_meters, y_meters) {
            (Some(x), Some(y)) => Some(PhysicalPixelSize {
                x_um: x * 1_000_000.0,
                y_um: y * 1_000_000.0,
            }),
            _ => None,
        };
    }
    summary
}

fn collect_channels<'a>(
    node: &'a MetadataNode,
    path: &mut Vec<&'a MetadataNode>,
    channels: &mut Vec<ChannelMetadata>,
) {
    path.push(node);
    if path_matches(
        path,
        &[
            "imagedocument",
            "metadata",
            "information",
            "image",
            "dimensions",
            "channels",
            "channel",
        ],
    ) {
        let id = attribute(node, "id").map(str::to_owned);
        let label = attribute(node, "name")
            .map(str::to_owned)
            .or_else(|| child_text(node, "name"))
            .or_else(|| id.clone())
            .unwrap_or_else(|| format!("Channel {}", channels.len()));
        let index = id
            .as_deref()
            .and_then(channel_index)
            .unwrap_or_else(|| i32::try_from(channels.len()).unwrap_or(i32::MAX));
        if !channels.iter().any(|channel| channel.index == index) {
            channels.push(ChannelMetadata { index, id, label });
        }
    }
    for child in &node.children {
        collect_channels(child, path, channels);
    }
    path.pop();
}

fn collect_scaling<'a>(
    node: &'a MetadataNode,
    path: &mut Vec<&'a MetadataNode>,
    x_meters: &mut Option<f64>,
    y_meters: &mut Option<f64>,
) {
    path.push(node);
    let standard_distance =
        path_matches(path, &["imagedocument", "metadata", "scaling", "distance"])
            || path_matches(
                path,
                &["imagedocument", "metadata", "scaling", "items", "distance"],
            );
    if standard_distance {
        let axis = attribute(node, "id").or_else(|| attribute(node, "axis"));
        let value = attribute(node, "value")
            .map(str::to_owned)
            .or_else(|| child_text(node, "value"))
            .or_else(|| (!node.text.is_empty()).then(|| node.text.clone()))
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value > 0.0);
        match (axis.map(str::trim), value) {
            (Some(axis), Some(value)) if axis.eq_ignore_ascii_case("x") && x_meters.is_none() => {
                *x_meters = Some(value);
            }
            (Some(axis), Some(value)) if axis.eq_ignore_ascii_case("y") && y_meters.is_none() => {
                *y_meters = Some(value);
            }
            _ => {}
        }
    }
    for child in &node.children {
        collect_scaling(child, path, x_meters, y_meters);
    }
    path.pop();
}

fn path_matches(path: &[&MetadataNode], expected: &[&str]) -> bool {
    path.len() == expected.len()
        && path
            .iter()
            .zip(expected)
            .all(|(node, expected)| node.name.eq_ignore_ascii_case(expected))
}

fn attribute<'a>(node: &'a MetadataNode, name: &str) -> Option<&'a str> {
    node.attributes
        .iter()
        .find(|attribute| attribute.name.eq_ignore_ascii_case(name))
        .map(|attribute| attribute.value.as_str())
}

fn child_text(node: &MetadataNode, name: &str) -> Option<String> {
    node.children
        .iter()
        .find(|child| named(child, name) && !child.text.is_empty())
        .map(|child| child.text.clone())
}

fn named(node: &MetadataNode, name: &str) -> bool {
    node.name.eq_ignore_ascii_case(name)
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
                    label: String::from("DAPI")
                },
                ChannelMetadata {
                    index: 1,
                    id: Some(String::from("Channel:1")),
                    label: String::from("FITC")
                },
                ChannelMetadata {
                    index: 2,
                    id: Some(String::from("Channel:2")),
                    label: String::from("Channel:2")
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
