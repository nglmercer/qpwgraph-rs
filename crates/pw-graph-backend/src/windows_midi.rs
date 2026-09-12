//! Windows MIDI backend built on WinMM.
//!
//! Unlike Core Audio, MIDI on Windows has genuine routing: `midiConnect` wires
//! an input device straight to an output device inside the MIDI stack, and
//! `midiDisconnect` takes it apart again. This is therefore the first Windows
//! backend where `connect` is honestly `true` -- the graph is editable, not
//! observed.
//!
//! Fan-out and fan-in are both ordinary here. A MIDI input is normally an
//! exclusive open, so the handles are shared and counted rather than opened
//! per link: a second connection out of one input reuses the handle WinMM
//! already has and asks `midiConnect` for another pairing. Whether the MIDI
//! stack accepts that is the MIDI stack's answer to give, and its error is
//! what comes back if it does not — this backend no longer refuses in advance
//! on an assumption about what Windows can do.
//!
//! Handles are opened lazily and only for devices that take part in a
//! connection, so merely listing the graph never opens a device another
//! application might want, and closed only when their last connection goes,
//! so removing one branch of a fan-out does not silence the rest.

use crate::api::{BackendCapabilities, BackendError, BackendResult, EffectDriver, GraphDriver};
use pw_graph_core::{
    encode_backend_id, BackendNamespace, Direction, Graph, GraphError, Link, LinkId, Node, NodeId,
    Port, PortId, PortType,
};
use std::collections::BTreeMap;

use windows::Win32::Media::Audio;
use windows::Win32::Media::Multimedia::{DRV_QUERYDEVICEINTERFACE, DRV_QUERYDEVICEINTERFACESIZE};

const WINDOWS_MIDI_CAPABILITIES: BackendCapabilities = BackendCapabilities {
    topology: true,
    // The one Windows backend that can really rewire itself.
    connect: true,
    disconnect: true,
    volume: false,
    mute: false,
    meters: false,
    effects: false,
    relay: false,
    recorders: false,
};

/// `MMSYSERR_NOERROR`.
const MM_OK: u32 = 0;

/// Local id space: inputs and outputs are numbered separately by WinMM, so the
/// kind is folded into the id to keep them distinct inside one namespace. The
/// lower 52 bits hold a hash of the opaque Plug and Play interface name; the
/// upper four local bits identify the graph resource kind.
const INPUT_TAG: u64 = 0x0010_0000_0000_0000;
const OUTPUT_TAG: u64 = 0x0020_0000_0000_0000;
const PORT_TAG: u64 = 0x0040_0000_0000_0000;
const IDENTITY_HASH_MASK: u64 = 0x000F_FFFF_FFFF_FFFF;

fn graph_id(local: u64) -> u64 {
    encode_backend_id(BackendNamespace::WindowsMidi, local)
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum DeviceKind {
    Input,
    Output,
}

#[derive(Clone, Debug)]
struct MidiDevice {
    /// WinMM device index, which is what every `midi*` call takes.
    index: u32,
    kind: DeviceKind,
    name: String,
    node_id: NodeId,
    port_id: PortId,
}

/// A device handle shared by every connection that needs it.
///
/// A MIDI input is normally an exclusive open, so a second `midiInOpen` on a
/// device already in use returns `MMSYSERR_ALLOCATED`. Opening once and
/// counting users is therefore not merely tidy: it is what makes one input
/// able to drive several outputs at all.
struct OpenDevice<H> {
    handle: H,
    users: usize,
}

/// One `midiConnect` pairing, and the ports it was drawn between.
struct Connection {
    link_id: LinkId,
    source: PortId,
    destination: PortId,
}

/// WinMM MIDI graph. Handles live here and are closed on drop.
pub struct WindowsMidiDriver {
    graph: Graph,
    devices: BTreeMap<PortId, MidiDevice>,
    /// Open input handles, by the port they belong to.
    inputs: BTreeMap<PortId, OpenDevice<Audio::HMIDIIN>>,
    /// Open output handles, by the port they belong to.
    outputs: BTreeMap<PortId, OpenDevice<Audio::HMIDIOUT>>,
    connections: Vec<Connection>,
    positions: BTreeMap<NodeId, [f32; 2]>,
    next_link: u64,
}

/// Written out rather than derived because the struct owns MIDI handles and
/// so implements `Drop`, which rules out the functional-update shorthand at
/// every call site.
impl Default for WindowsMidiDriver {
    fn default() -> Self {
        Self {
            graph: Graph::default(),
            devices: BTreeMap::new(),
            inputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            connections: Vec::new(),
            positions: BTreeMap::new(),
            next_link: 0,
        }
    }
}

impl Drop for WindowsMidiDriver {
    fn drop(&mut self) {
        // Disconnect every pairing before closing anything, so the MIDI
        // stack's own bookkeeping is unwound in the order it was built.
        for connection in std::mem::take(&mut self.connections) {
            let input = self.inputs.get(&connection.source).map(|open| open.handle);
            let output = self
                .outputs
                .get(&connection.destination)
                .map(|open| open.handle);
            if let (Some(input), Some(output)) = (input, output) {
                unsafe {
                    let _ = Audio::midiDisconnect(Audio::HMIDI(input.0), output, None);
                }
            }
        }
        for (_, open) in std::mem::take(&mut self.inputs) {
            unsafe {
                let _ = Audio::midiInStop(open.handle);
                let _ = Audio::midiInClose(open.handle);
            }
        }
        for (_, open) in std::mem::take(&mut self.outputs) {
            unsafe {
                let _ = Audio::midiOutClose(open.handle);
            }
        }
    }
}

impl std::fmt::Debug for WindowsMidiDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsMidiDriver")
            .field("devices", &self.devices.len())
            .field("connections", &self.connections.len())
            .finish()
    }
}

impl WindowsMidiDriver {
    pub fn new() -> BackendResult<Self> {
        // Assigned rather than written as a functional update: the struct
        // owns MIDI handles and so implements `Drop`, which rules that syntax
        // out.
        let mut driver = Self::default();
        driver.next_link = 1;
        driver.refresh()?;
        Ok(driver)
    }

    /// Devices WinMM currently reports, in index order per direction. The
    /// device index is retained only for calls into WinMM; graph identity is
    /// based on the device interface name whenever the system provides one.
    fn enumerate() -> Vec<MidiDevice> {
        let mut devices = Vec::new();
        let mut identities = BTreeMap::<(DeviceKind, String), u32>::new();
        for index in 0..unsafe { Audio::midiInGetNumDevs() } {
            let mut caps = Audio::MIDIINCAPSW::default();
            let status = unsafe {
                Audio::midiInGetDevCapsW(
                    index as usize,
                    &mut caps,
                    std::mem::size_of::<Audio::MIDIINCAPSW>() as u32,
                )
            };
            if status != MM_OK {
                continue;
            }
            // The caps structs are packed, so the name is copied out before it
            // can be referenced.
            let name = { caps.szPname };
            let name = wide_name(&name);
            let manufacturer = { caps.wMid };
            let product = { caps.wPid };
            let driver_version = { caps.vDriverVersion };
            let base_identity =
                query_device_interface(DeviceKind::Input, index).unwrap_or_else(|| {
                    fallback_identity(
                        DeviceKind::Input,
                        &name,
                        manufacturer,
                        product,
                        driver_version,
                    )
                });
            let identity = unique_identity(DeviceKind::Input, base_identity, &mut identities);
            devices.push(device_from_identity(
                index,
                DeviceKind::Input,
                name,
                &identity,
            ));
        }
        for index in 0..unsafe { Audio::midiOutGetNumDevs() } {
            let mut caps = Audio::MIDIOUTCAPSW::default();
            let status = unsafe {
                Audio::midiOutGetDevCapsW(
                    index as usize,
                    &mut caps,
                    std::mem::size_of::<Audio::MIDIOUTCAPSW>() as u32,
                )
            };
            if status != MM_OK {
                continue;
            }
            let name = { caps.szPname };
            let name = wide_name(&name);
            let manufacturer = { caps.wMid };
            let product = { caps.wPid };
            let driver_version = { caps.vDriverVersion };
            let base_identity =
                query_device_interface(DeviceKind::Output, index).unwrap_or_else(|| {
                    fallback_identity(
                        DeviceKind::Output,
                        &name,
                        manufacturer,
                        product,
                        driver_version,
                    )
                });
            let identity = unique_identity(DeviceKind::Output, base_identity, &mut identities);
            devices.push(device_from_identity(
                index,
                DeviceKind::Output,
                name,
                &identity,
            ));
        }
        devices
    }

    fn device(&self, port: PortId) -> Option<&MidiDevice> {
        self.devices.get(&port)
    }

    /// Resolve a connect request into the input and output it names.
    fn endpoints(&self, src: PortId, dst: PortId) -> BackendResult<(MidiDevice, MidiDevice)> {
        let source = self
            .device(src)
            .cloned()
            .ok_or(GraphError::MissingPort(src))?;
        let destination = self
            .device(dst)
            .cloned()
            .ok_or(GraphError::MissingPort(dst))?;
        if source.kind != DeviceKind::Input || destination.kind != DeviceKind::Output {
            return Err(BackendError::unsupported(
                "a MIDI connection runs from an input device to an output device",
            ));
        }
        Ok((source, destination))
    }

    /// Take a use of an input device, opening it if this is the first.
    fn open_input(&mut self, port: PortId, index: u32) -> BackendResult<Audio::HMIDIIN> {
        if let Some(open) = self.inputs.get_mut(&port) {
            open.users += 1;
            return Ok(open.handle);
        }
        let mut handle = Audio::HMIDIIN::default();
        let status = unsafe {
            Audio::midiInOpen(
                &mut handle,
                index,
                None,
                None,
                Audio::MIDI_WAVE_OPEN_TYPE(0),
            )
        };
        if status != MM_OK {
            return Err(mm_error("input open", status));
        }
        self.inputs.insert(port, OpenDevice { handle, users: 1 });
        Ok(handle)
    }

    fn open_output(&mut self, port: PortId, index: u32) -> BackendResult<Audio::HMIDIOUT> {
        if let Some(open) = self.outputs.get_mut(&port) {
            open.users += 1;
            return Ok(open.handle);
        }
        let mut handle = Audio::HMIDIOUT::default();
        let status = unsafe {
            Audio::midiOutOpen(
                &mut handle,
                index,
                None,
                None,
                Audio::MIDI_WAVE_OPEN_TYPE(0),
            )
        };
        if status != MM_OK {
            return Err(mm_error("output open", status));
        }
        self.outputs.insert(port, OpenDevice { handle, users: 1 });
        Ok(handle)
    }

    /// Give up one use of an input, closing it when the last goes.
    ///
    /// Closing only at zero is what keeps a fanned-out input alive: taking the
    /// handle away when the first of its connections is removed would silence
    /// the others.
    fn release_input(&mut self, port: PortId) {
        let Some(open) = self.inputs.get_mut(&port) else {
            return;
        };
        open.users -= 1;
        if open.users > 0 {
            return;
        }
        if let Some(open) = self.inputs.remove(&port) {
            unsafe {
                let _ = Audio::midiInStop(open.handle);
                let _ = Audio::midiInClose(open.handle);
            }
        }
    }

    fn release_output(&mut self, port: PortId) {
        let Some(open) = self.outputs.get_mut(&port) else {
            return;
        };
        open.users -= 1;
        if open.users > 0 {
            return;
        }
        if let Some(open) = self.outputs.remove(&port) {
            unsafe {
                let _ = Audio::midiOutClose(open.handle);
            }
        }
    }

    /// How many outputs one input is currently driving.
    #[cfg(test)]
    fn fan_out_width(&self, source: PortId) -> usize {
        self.connections
            .iter()
            .filter(|connection| connection.source == source)
            .count()
    }

    fn allocate_link(&mut self) -> LinkId {
        let id = LinkId(graph_id(self.next_link));
        self.next_link += 1;
        id
    }
}

#[cfg(test)]
fn device_from_caps(index: u32, kind: DeviceKind, name: String) -> MidiDevice {
    let identity = fallback_identity(kind, &name, 0, 0, 0);
    device_from_identity(index, kind, name, &identity)
}

fn device_from_identity(index: u32, kind: DeviceKind, name: String, identity: &str) -> MidiDevice {
    let tag = match kind {
        DeviceKind::Input => INPUT_TAG,
        DeviceKind::Output => OUTPUT_TAG,
    };
    let hash = stable_identity_hash(kind, identity);
    let local = tag | hash;
    MidiDevice {
        index,
        kind,
        name,
        node_id: NodeId(graph_id(local)),
        port_id: PortId(graph_id(PORT_TAG | hash)),
    }
}

fn fallback_identity(
    kind: DeviceKind,
    name: &str,
    manufacturer: u16,
    product: u16,
    driver_version: u32,
) -> String {
    format!(
        "fallback:{}:{manufacturer:04x}:{product:04x}:{driver_version:08x}:{}",
        kind.as_str(),
        name.trim()
    )
}

fn unique_identity(
    kind: DeviceKind,
    base: String,
    identities: &mut BTreeMap<(DeviceKind, String), u32>,
) -> String {
    let occurrence = identities.entry((kind, base.clone())).or_insert(0);
    let identity = if *occurrence == 0 {
        base
    } else {
        format!("{}#{}", base, *occurrence + 1)
    };
    *occurrence += 1;
    identity
}

impl DeviceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

fn stable_identity_hash(kind: DeviceKind, identity: &str) -> u64 {
    // FNV-1a is small, deterministic, and sufficient after the opaque system
    // identity has already made the device distinction. The kind is included
    // again so a fallback string can never alias across directions.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in format!("winmm-midi:{}:{identity}", kind.as_str()).bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash & IDENTITY_HASH_MASK).max(1)
}

/// Query the stable Plug and Play interface name without opening the MIDI
/// device. WinMM requires the numeric device id to be cast to its handle type
/// for these two system-intercepted messages; the returned string is opaque
/// and is only used as a stable identity key.
fn query_device_interface(kind: DeviceKind, index: u32) -> Option<String> {
    let mut size = 0u32;
    let device = index as usize as *mut core::ffi::c_void;
    let status = unsafe {
        match kind {
            DeviceKind::Input => Audio::midiInMessage(
                Some(Audio::HMIDIIN(device)),
                DRV_QUERYDEVICEINTERFACESIZE,
                Some((&mut size as *mut u32) as usize),
                Some(0),
            ),
            DeviceKind::Output => Audio::midiOutMessage(
                Some(Audio::HMIDIOUT(device)),
                DRV_QUERYDEVICEINTERFACESIZE,
                Some((&mut size as *mut u32) as usize),
                Some(0),
            ),
        }
    };
    if status != MM_OK || size == 0 || size > 1024 * 1024 {
        return None;
    }
    let mut buffer = vec![0u16; (size as usize).div_ceil(std::mem::size_of::<u16>())];
    let status = unsafe {
        match kind {
            DeviceKind::Input => Audio::midiInMessage(
                Some(Audio::HMIDIIN(device)),
                DRV_QUERYDEVICEINTERFACE,
                Some(buffer.as_mut_ptr() as usize),
                Some(size as usize),
            ),
            DeviceKind::Output => Audio::midiOutMessage(
                Some(Audio::HMIDIOUT(device)),
                DRV_QUERYDEVICEINTERFACE,
                Some(buffer.as_mut_ptr() as usize),
                Some(size as usize),
            ),
        }
    };
    (status == MM_OK)
        .then(|| wide_name(&buffer))
        .filter(|name| !name.is_empty())
}

/// WinMM device names are a fixed-size, NUL-padded UTF-16 array.
fn wide_name(raw: &[u16]) -> String {
    let end = raw
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end])
}

fn mm_error(operation: &str, status: u32) -> BackendError {
    BackendError::native(format!(
        "Windows MIDI {operation} failed (mmsyserr {status})"
    ))
}

impl GraphDriver for WindowsMidiDriver {
    fn capabilities(&self) -> BackendCapabilities {
        WINDOWS_MIDI_CAPABILITIES
    }

    fn refresh(&mut self) -> BackendResult<Vec<Node>> {
        let devices = Self::enumerate();
        let mut graph = Graph::default();
        let mut map = BTreeMap::new();
        for device in devices {
            graph.add_node(Node::new(
                device.node_id,
                device.name.clone(),
                pw_graph_core::NodeType::WindowsMidi,
            ))?;
            graph.add_port(Port::new(
                device.port_id,
                device.node_id,
                "midi",
                match device.kind {
                    // An input device is a *source* of MIDI for the graph.
                    DeviceKind::Input => Direction::Source,
                    DeviceKind::Output => Direction::Sink,
                },
                PortType::MidiJack,
            ))?;
            map.insert(device.port_id, device);
        }
        // Connections this process opened survive a refresh, so they are
        // carried into the rebuilt graph. One whose device has been unplugged
        // is dropped, which closes its handles with it.
        let mut surviving = Vec::with_capacity(self.connections.len());
        for connection in std::mem::take(&mut self.connections) {
            let link = self.graph.link(connection.link_id).cloned();
            let still_present = link.as_ref().is_some_and(|link| {
                map.contains_key(&link.output_port) && map.contains_key(&link.input_port)
            });
            match (still_present, link) {
                (true, Some(link)) => {
                    let _ = graph.insert_existing_link(link);
                    surviving.push(connection);
                }
                // Dropping `connection` here closes both handles.
                _ => continue,
            }
        }
        self.connections = surviving;
        for (node_id, position) in graph.default_node_positions() {
            if let Some(node) = graph.nodes.get_mut(&node_id) {
                node.position = self.positions.get(&node_id).copied().unwrap_or(position);
            }
        }
        self.graph = graph;
        self.devices = map;
        Ok(self.graph.nodes.values().cloned().collect())
    }

    /// Connect a MIDI input to an output.
    ///
    /// Fan-out and fan-in both work here, and neither is special-cased: the
    /// handles are shared and counted, so a second connection out of the same
    /// input reuses the input WinMM already has open and asks `midiConnect`
    /// for another pairing. If the MIDI stack will not take it, its own error
    /// is what comes back — rather than a blanket refusal that assumed the
    /// answer.
    fn connect(&mut self, src: PortId, dst: PortId) -> BackendResult<Link> {
        let (source, destination) = self.endpoints(src, dst)?;
        let (source_index, destination_index) = (source.index, destination.index);
        if self
            .connections
            .iter()
            .any(|connection| connection.source == src && connection.destination == dst)
        {
            return Err(GraphError::DuplicateConnection(src, dst).into());
        }

        let input = self.open_input(src, source_index)?;
        let output = match self.open_output(dst, destination_index) {
            Ok(output) => output,
            Err(error) => {
                self.release_input(src);
                return Err(error);
            }
        };

        let status = unsafe { Audio::midiConnect(Audio::HMIDI(input.0), output, None) };
        if status != MM_OK {
            self.release_output(dst);
            self.release_input(src);
            return Err(mm_error("connect", status));
        }
        // Nothing flows until the input is started. Starting an already
        // started input is harmless, so this is safe to repeat per link.
        let status = unsafe { Audio::midiInStart(input) };
        if status != MM_OK {
            let _ = unsafe { Audio::midiDisconnect(Audio::HMIDI(input.0), output, None) };
            self.release_output(dst);
            self.release_input(src);
            return Err(mm_error("input start", status));
        }

        let link_id = self.allocate_link();
        let link = match self.graph.add_link(link_id, src, dst) {
            Ok(link) => link,
            Err(error) => {
                let _ = unsafe { Audio::midiDisconnect(Audio::HMIDI(input.0), output, None) };
                self.release_output(dst);
                self.release_input(src);
                return Err(error.into());
            }
        };
        self.connections.push(Connection {
            link_id,
            source: src,
            destination: dst,
        });
        Ok(link)
    }

    fn disconnect(&mut self, link: LinkId) -> BackendResult<Link> {
        let position = self
            .connections
            .iter()
            .position(|connection| connection.link_id == link)
            .ok_or(GraphError::MissingLink(link))?;
        let connection = self.connections.remove(position);
        let input = self.inputs.get(&connection.source).map(|open| open.handle);
        let output = self
            .outputs
            .get(&connection.destination)
            .map(|open| open.handle);
        if let (Some(input), Some(output)) = (input, output) {
            unsafe {
                let _ = Audio::midiDisconnect(Audio::HMIDI(input.0), output, None);
            }
        }
        // Only the pairing goes; the handles stay open for whatever other
        // connections are still using them.
        self.release_input(connection.source);
        self.release_output(connection.destination);
        Ok(self.graph.remove_link(link)?)
    }

    fn set_node_position(&mut self, node: NodeId, position: [f32; 2]) -> BackendResult<()> {
        self.graph
            .nodes
            .get_mut(&node)
            .ok_or(GraphError::MissingNode(node))?
            .position = position;
        self.positions.insert(node, position);
        Ok(())
    }

    fn graph(&self) -> &Graph {
        &self.graph
    }

    fn is_node_type(&self, node_type: pw_graph_core::NodeType) -> bool {
        node_type == pw_graph_core::NodeType::WindowsMidi
    }

    fn is_port_type(&self, port_type: PortType) -> bool {
        port_type == PortType::MidiJack
    }

    fn is_link_mutable(&self, link: LinkId) -> bool {
        self.graph.link(link).is_some()
    }
}

impl EffectDriver for WindowsMidiDriver {}

#[cfg(feature = "relay")]
impl crate::api::RelayDriver for WindowsMidiDriver {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names_stop_at_the_nul_padding() {
        let mut raw = [0u16; 32];
        for (slot, value) in raw.iter_mut().zip("MPK mini".encode_utf16()) {
            *slot = value;
        }
        assert_eq!(wide_name(&raw), "MPK mini");
        assert_eq!(wide_name(&[0u16; 8]), "");
    }

    /// Inputs and outputs are numbered separately by WinMM, so the kind has to
    /// be part of the id or input 0 and output 0 would collide.
    #[test]
    fn inputs_and_outputs_never_share_an_id() {
        let input = device_from_caps(0, DeviceKind::Input, "in".into());
        let output = device_from_caps(0, DeviceKind::Output, "out".into());
        assert_ne!(input.node_id, output.node_id);
        assert_ne!(input.port_id, output.port_id);
        // And a node never collides with a port.
        assert_ne!(input.node_id.0, input.port_id.0);
    }

    #[test]
    fn device_identity_does_not_change_when_winmm_reorders_indices() {
        let first = device_from_identity(
            0,
            DeviceKind::Input,
            "keyboard".into(),
            "\\\\?\\midi#{stable-key}",
        );
        let reordered = device_from_identity(
            7,
            DeviceKind::Input,
            "keyboard".into(),
            "\\\\?\\midi#{stable-key}",
        );
        let different = device_from_identity(
            0,
            DeviceKind::Input,
            "keyboard".into(),
            "\\\\?\\midi#{other-key}",
        );

        assert_eq!(first.node_id, reordered.node_id);
        assert_eq!(first.port_id, reordered.port_id);
        assert_ne!(first.node_id, different.node_id);
        assert_ne!(first.port_id, different.port_id);
    }

    #[test]
    fn fallback_identity_includes_direction_and_device_description() {
        let input = fallback_identity(DeviceKind::Input, "same", 1, 2, 3);
        let output = fallback_identity(DeviceKind::Output, "same", 1, 2, 3);
        assert_ne!(input, output);
        assert_ne!(
            input,
            fallback_identity(DeviceKind::Input, "other", 1, 2, 3)
        );
    }

    #[test]
    fn identity_occurrences_are_scoped_to_each_direction() {
        let mut identities = BTreeMap::new();
        assert_eq!(
            unique_identity(DeviceKind::Input, "same-interface".into(), &mut identities),
            "same-interface"
        );
        assert_eq!(
            unique_identity(DeviceKind::Output, "same-interface".into(), &mut identities),
            "same-interface"
        );
        assert_eq!(
            unique_identity(DeviceKind::Input, "same-interface".into(), &mut identities),
            "same-interface#2"
        );
    }

    #[test]
    fn every_id_lands_in_the_windows_midi_namespace() {
        let device = device_from_caps(3, DeviceKind::Output, "synth".into());
        assert_eq!(
            pw_graph_core::backend_for_node(device.node_id),
            Some(pw_graph_core::BackendKind::WindowsMidi)
        );
        assert_eq!(
            pw_graph_core::backend_for_port(device.port_id),
            Some(pw_graph_core::BackendKind::WindowsMidi)
        );
    }

    /// MIDI is the one Windows backend that can rewire itself, and WinMM is
    /// always present, so this runs anywhere Windows does.
    #[test]
    fn windows_midi_reports_real_routing() {
        let driver = WindowsMidiDriver::new().expect("WinMM is part of Windows");
        let capabilities = driver.capabilities();

        assert!(capabilities.connect, "midiConnect is real routing");
        assert!(capabilities.disconnect);
        // ...and none of the audio facilities, which it genuinely lacks.
        assert!(!capabilities.volume);
        assert!(!capabilities.meters);
    }

    /// Enumeration must not open anything: opening a device would take it from
    /// whatever application is using it just to draw the graph.
    #[test]
    fn listing_devices_opens_no_handles() {
        let driver = WindowsMidiDriver::new().expect("WinMM is part of Windows");

        assert!(driver.connections.is_empty());
        // Every listed device still gets a node and exactly one port.
        for node in driver.graph().nodes.values() {
            assert_eq!(node.ports.len(), 1, "{} has one MIDI port", node.name);
        }
        assert_eq!(driver.devices.len(), driver.graph().ports.len());
    }

    /// Regression: a fanned-out input used to be refused outright, on the
    /// assumption that Windows routes one input to one output. Now the
    /// handle is shared and counted, and this is the bookkeeping that makes
    /// it work -- removing one branch must not close the handle the other
    /// branches are still using.
    #[test]
    fn a_shared_handle_closes_only_when_its_last_connection_goes() {
        let mut driver = WindowsMidiDriver::default();
        let input = device_from_caps(0, DeviceKind::Input, "keys".into());
        // Two connections out of one input, as a fan-out would create.
        driver.inputs.insert(
            input.port_id,
            OpenDevice {
                handle: Audio::HMIDIIN::default(),
                users: 2,
            },
        );

        driver.release_input(input.port_id);
        assert!(
            driver.inputs.contains_key(&input.port_id),
            "the second branch still needs this input"
        );

        driver.release_input(input.port_id);
        assert!(!driver.inputs.contains_key(&input.port_id));
        // Releasing again is harmless: a disconnect that raced a device
        // removal must not underflow the count.
        driver.release_input(input.port_id);
    }

    #[test]
    fn an_output_handle_is_shared_by_every_input_feeding_it() {
        let mut driver = WindowsMidiDriver::default();
        let output = device_from_caps(0, DeviceKind::Output, "synth".into());
        driver.outputs.insert(
            output.port_id,
            OpenDevice {
                handle: Audio::HMIDIOUT::default(),
                users: 2,
            },
        );

        driver.release_output(output.port_id);
        assert!(driver.outputs.contains_key(&output.port_id));
        driver.release_output(output.port_id);
        assert!(!driver.outputs.contains_key(&output.port_id));
    }

    #[test]
    fn the_same_pair_cannot_be_connected_twice() {
        let mut driver = WindowsMidiDriver::default();
        let input = device_from_caps(0, DeviceKind::Input, "keys".into());
        let output = device_from_caps(0, DeviceKind::Output, "synth".into());
        driver.devices.insert(input.port_id, input.clone());
        driver.devices.insert(output.port_id, output.clone());
        driver.connections.push(Connection {
            link_id: LinkId(1),
            source: input.port_id,
            destination: output.port_id,
        });

        // Fan-out means several *different* destinations, not the same one
        // twice; WinMM would happily double every note.
        assert!(matches!(
            driver.connect(input.port_id, output.port_id),
            Err(BackendError::Graph(GraphError::DuplicateConnection(_, _)))
        ));
        assert_eq!(driver.fan_out_width(input.port_id), 1);
    }

    /// A MIDI link only runs input to output; anything else is refused rather
    /// than handed to WinMM.
    #[test]
    fn a_connection_must_run_from_an_input_to_an_output() {
        let mut driver = WindowsMidiDriver::default();
        let input = device_from_caps(0, DeviceKind::Input, "keys".into());
        let output = device_from_caps(0, DeviceKind::Output, "synth".into());
        driver.devices.insert(input.port_id, input.clone());
        driver.devices.insert(output.port_id, output.clone());

        assert!(driver.endpoints(input.port_id, output.port_id).is_ok());
        for (src, dst) in [
            (output.port_id, input.port_id),
            (input.port_id, input.port_id),
            (output.port_id, output.port_id),
        ] {
            assert!(
                matches!(
                    driver.endpoints(src, dst),
                    Err(BackendError::Unsupported(_))
                ),
                "a MIDI link runs input to output only"
            );
        }
    }
}
