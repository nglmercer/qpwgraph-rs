//! qpwgraph XML serialization and parsing.

use super::*;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer, XmlVersion};
use std::collections::BTreeMap;

impl Patchbay {
    pub(crate) fn to_xml(&self) -> Result<String, PatchbayError> {
        let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
        writer
            .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
            .map_err(PatchbayError::XmlWrite)?;
        writer
            .write_event(Event::DocType(BytesText::new("patchbay")))
            .map_err(PatchbayError::XmlWrite)?;
        let mut root = BytesStart::new("patchbay");
        root.push_attribute(("name", self.name.as_str()));
        root.push_attribute(("version", "0.8.3"));
        writer
            .write_event(Event::Start(root))
            .map_err(PatchbayError::XmlWrite)?;
        writer
            .write_event(Event::Start(BytesStart::new("items")))
            .map_err(PatchbayError::XmlWrite)?;
        for connection in &self.connections {
            let output_node_type = connection.effective_output_node_type();
            let mut item = BytesStart::new("item");
            // Keep the native qpwgraph fields only. Rich endpoint identity and
            // endpoint-specific node types live in the deterministic sidecar;
            // unknown XML attributes would make interoperability depend on the
            // particular qpwgraph version reading this file.
            item.push_attribute(("node-type", node_type_text(output_node_type)));
            item.push_attribute((
                "port-type",
                port_type_text(output_node_type, connection.port_type),
            ));
            writer
                .write_event(Event::Start(item))
                .map_err(PatchbayError::XmlWrite)?;

            let mut output = BytesStart::new("output");
            output.push_attribute(("node", connection.output_node.as_str()));
            output.push_attribute(("port", connection.output_name.as_str()));
            writer
                .write_event(Event::Empty(output))
                .map_err(PatchbayError::XmlWrite)?;

            let mut input = BytesStart::new("input");
            input.push_attribute(("node", connection.input_node.as_str()));
            input.push_attribute(("port", connection.input_name.as_str()));
            writer
                .write_event(Event::Empty(input))
                .map_err(PatchbayError::XmlWrite)?;
            writer
                .write_event(Event::End(BytesEnd::new("item")))
                .map_err(PatchbayError::XmlWrite)?;
        }
        writer
            .write_event(Event::End(BytesEnd::new("items")))
            .map_err(PatchbayError::XmlWrite)?;
        writer
            .write_event(Event::End(BytesEnd::new("patchbay")))
            .map_err(PatchbayError::XmlWrite)?;
        Ok(String::from_utf8(writer.into_inner()).expect("XML writer emits UTF-8"))
    }

    pub(crate) fn from_xml(text: &str) -> Result<Self, PatchbayError> {
        let mut reader = Reader::from_str(text);
        reader.config_mut().trim_text(true);
        let mut patchbay = Patchbay::new("patchbay");
        let mut current: Option<PatchConnection> = None;
        loop {
            match reader.read_event()? {
                Event::Start(element) if element.name().as_ref() == b"patchbay" => {
                    let attributes = attributes(&reader, &element)?;
                    if let Some(name) = attributes.get("name") {
                        patchbay.name = name.clone();
                    }
                }
                Event::Start(element) if element.name().as_ref() == b"item" => {
                    let attributes = attributes(&reader, &element)?;
                    let output_node_type =
                        endpoint_node_type_from_text(attributes.get("output-node-type"));
                    let node_type = output_node_type
                        .unwrap_or_else(|| node_type_from_text(attributes.get("node-type")));
                    // When our output-side extension is present and no input
                    // extension is needed, both endpoints have that same
                    // precise type. Retain the explicitness so a missing
                    // Effect cannot fall back to an unrelated same-named
                    // PipeWire node during activation.
                    let input_node_type =
                        endpoint_node_type_from_text(attributes.get("input-node-type"))
                            .or_else(|| output_node_type.map(|_| node_type));
                    current = Some(PatchConnection {
                        node_type,
                        output_node_type,
                        input_node_type,
                        port_type: port_type_from_text(attributes.get("port-type")),
                        ..PatchConnection::default()
                    });
                }
                Event::Empty(element) | Event::Start(element)
                    if element.name().as_ref() == b"output"
                        || element.name().as_ref() == b"input" =>
                {
                    let attributes = attributes(&reader, &element)?;
                    if let Some(connection) = current.as_mut() {
                        let node = attributes.get("node").cloned().unwrap_or_default();
                        let port = attributes.get("port").cloned().unwrap_or_default();
                        if element.name().as_ref() == b"output" {
                            connection.output_node = node;
                            connection.output_name = port;
                        } else {
                            connection.input_node = node;
                            connection.input_name = port;
                        }
                    }
                }
                Event::End(element) if element.name().as_ref() == b"item" => {
                    if let Some(connection) = current.take() {
                        if !connection.output_node.is_empty()
                            && !connection.output_name.is_empty()
                            && !connection.input_node.is_empty()
                            && !connection.input_name.is_empty()
                        {
                            patchbay.connections.push(connection);
                        }
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(patchbay)
    }
}

fn attributes(
    reader: &Reader<&[u8]>,
    element: &BytesStart<'_>,
) -> Result<BTreeMap<String, String>, PatchbayError> {
    element
        .attributes()
        .map(|attribute| {
            let attribute = attribute.map_err(|_| PatchbayError::XmlAttributes)?;
            let key = String::from_utf8_lossy(attribute.key.as_ref()).into_owned();
            let value = attribute
                .decoded_and_normalized_value(XmlVersion::default(), reader.decoder())
                .map_err(PatchbayError::Xml)?
                .into_owned();
            Ok((key, value))
        })
        .collect()
}

fn node_type_text(node_type: NodeType) -> &'static str {
    match node_type {
        NodeType::PipeWire
        | NodeType::Effect
        | NodeType::WindowsAudioEndpoint
        | NodeType::WindowsAudioSession
        | NodeType::WindowsMidi
        | NodeType::Unknown => "pipewire",
        NodeType::Recorder => "recorder",
        NodeType::AlsaMidi => "alsa",
    }
}

fn port_type_text(node_type: NodeType, port_type: PortType) -> &'static str {
    match (node_type, port_type) {
        (NodeType::AlsaMidi, PortType::MidiAlsa) => "alsa-midi",
        (_, PortType::Audio) => "pipewire-audio",
        (_, PortType::MidiJack) => "pipewire-midi",
        (_, PortType::Video) => "pipewire-video",
        _ => "pipewire-other",
    }
}

fn node_type_from_text(value: Option<&String>) -> NodeType {
    match value.map(String::as_str) {
        Some("alsa") => NodeType::AlsaMidi,
        Some("pipewire") => NodeType::PipeWire,
        Some("effect") => NodeType::Effect,
        Some("recorder") => NodeType::Recorder,
        Some("windows-audio-endpoint") => NodeType::WindowsAudioEndpoint,
        Some("windows-audio-session") => NodeType::WindowsAudioSession,
        Some("windows-midi") => NodeType::WindowsMidi,
        Some("unknown") => NodeType::Unknown,
        _ => NodeType::Unknown,
    }
}

fn endpoint_node_type_from_text(value: Option<&String>) -> Option<NodeType> {
    match value.map(String::as_str) {
        Some("pipewire") => Some(NodeType::PipeWire),
        Some("effect") => Some(NodeType::Effect),
        Some("recorder") => Some(NodeType::Recorder),
        Some("alsa") => Some(NodeType::AlsaMidi),
        Some("windows-audio-endpoint") => Some(NodeType::WindowsAudioEndpoint),
        Some("windows-audio-session") => Some(NodeType::WindowsAudioSession),
        Some("windows-midi") => Some(NodeType::WindowsMidi),
        Some("unknown") => Some(NodeType::Unknown),
        _ => None,
    }
}

fn port_type_from_text(value: Option<&String>) -> PortType {
    match value.map(String::as_str) {
        Some("pipewire-audio") => PortType::Audio,
        Some("pipewire-midi") => PortType::MidiJack,
        Some("pipewire-video") => PortType::Video,
        Some("alsa-midi") => PortType::MidiAlsa,
        _ => PortType::Unknown,
    }
}
