//! What one UMP Endpoint declares, and the answers it gives (M2-104 §7.1).
//!
//! An [`Endpoint`] is a value: the UMP version, the protocols and Jitter
//! Reduction (JR) timestamps it supports and has active, who it is, and its
//! Function Blocks. [`Endpoint::reply`] is the answering side as one pure step
//! over a received message, with no allocation. The asking side is
//! [`Connection`](crate::Connection).

use tutti_ump::{
    BlockFilter, BlockSelect, DeviceIdentity, Direction as BlockDirection, DiscoveryFilter,
    FirstGroup, FunctionBlockId, FunctionBlockInfo, GroupSpan, Message, Midi1Port, Protocol,
    Stream, StreamConfig, U7, UiHint, Ump, UmpVersion, text_packets,
};

use crate::name::{BlockName, EndpointName, ProductInstanceId};

/// How many Function Blocks an Endpoint holds (M2-104 §7.1.7: numbers `0..=31`).
pub const MAX_FUNCTION_BLOCKS: usize = 32;

/// The UMP version this core speaks (M2-104 Table 5: 1.1).
pub const UMP_VERSION: UmpVersion = UmpVersion { major: 1, minor: 1 };

/// What an Endpoint supports (M2-104 §7.1.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Capabilities {
    /// Sends and receives the MIDI 1.0 Protocol (`M1`).
    pub midi1: bool,
    /// Sends and receives the MIDI 2.0 Protocol (`M2`).
    pub midi2: bool,
    /// Receives JR Timestamps (`RXJR`).
    pub rx_jr: bool,
    /// Sends JR Timestamps (`TXJR`).
    pub tx_jr: bool,
}

/// What an Endpoint that answers supports: [`Capabilities`] with at least one
/// Protocol (M2-104 §7.1.2: the Protocol it is in is one it supports, so an
/// Endpoint with neither has none to be in). A peer's report is a
/// [`Capabilities`] and may say neither; this is what an Endpoint declares of
/// itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Supports(Capabilities);

/// Why [`Supports::new`] refuses: neither Protocol is supported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[error("an Endpoint supports at least one of MIDI 1.0 and MIDI 2.0")]
pub struct NoProtocol;

impl Supports {
    /// `capabilities` as what an Endpoint supports, `None` with neither
    /// Protocol.
    #[must_use]
    pub const fn new(capabilities: Capabilities) -> Option<Self> {
        if capabilities.midi1 || capabilities.midi2 {
            Some(Self(capabilities))
        } else {
            None
        }
    }

    /// As [`new`](Self::new), for a literal.
    ///
    /// # Panics
    ///
    /// With neither Protocol.
    #[must_use]
    pub const fn of(capabilities: Capabilities) -> Self {
        match Self::new(capabilities) {
            Some(supports) => supports,
            None => panic!("an Endpoint supports at least one Protocol"),
        }
    }

    /// The capabilities.
    #[must_use]
    pub const fn get(self) -> Capabilities {
        self.0
    }
}

impl From<Supports> for Capabilities {
    fn from(supports: Supports) -> Self {
        supports.0
    }
}

impl TryFrom<Capabilities> for Supports {
    type Error = NoProtocol;

    fn try_from(capabilities: Capabilities) -> Result<Self, NoProtocol> {
        Self::new(capabilities).ok_or(NoProtocol)
    }
}

/// One Function Block (M2-104 §7.1.8): a set of Groups that act as one
/// function of the Endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionBlock {
    id: FunctionBlockId,
    active: bool,
    direction: BlockDirection,
    ui_hint: UiHint,
    midi1: Midi1Port,
    first_group: FirstGroup,
    groups: GroupSpan,
    midi_ci_version: u8,
    max_sysex8_streams: u8,
    pub(crate) name: Option<BlockName>,
}

/// Why a [`FunctionBlockBuilder`] cannot build a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BlockError {
    /// The direction is the reserved `0b00` (M2-104 §7.1.8).
    #[error("the block's direction is the reserved 0b00")]
    ReservedDirection,
    /// The block would run past Group 16: `first_group + groups > 16`.
    #[error("{} Groups from Group {} run past Group 16", .groups.get(), .first_group.get())]
    PastGroup16 {
        /// The first Group it was to span.
        first_group: FirstGroup,
        /// How many Groups it was to span.
        groups: GroupSpan,
    },
    /// The UI hint names a role the direction rules out: a receiver hint on
    /// an output-only block, or a sender hint on an input-only one (§7.1.8).
    #[error("the {hint:?} UI hint contradicts the {direction:?} direction")]
    UiHintContradictsDirection {
        /// The block's direction.
        direction: BlockDirection,
        /// The hint that contradicts it.
        hint: UiHint,
    },
    /// The MIDI 1.0 port is the reserved `0x03`.
    #[error("the block's MIDI 1.0 port is the reserved 3")]
    ReservedMidi1Port,
    /// A MIDI 1.0 port spans one Group (§7.1.8 requires Number of Groups Spanned
    /// to be 1), and this block spans `groups`.
    #[error("a MIDI 1.0 port spans one Group, not {}", .groups.get())]
    Midi1PortSpansGroups {
        /// How many Groups it was to span.
        groups: GroupSpan,
    },
}

/// A [`FunctionBlock`] being described: [`FunctionBlock::builder`] starts one
/// with the four fields every block has, the setters add the rest, and
/// [`build`](Self::build) checks them together.
#[derive(Clone, Debug)]
#[must_use = "a builder does nothing until `build`"]
pub struct FunctionBlockBuilder {
    block: FunctionBlock,
}

impl FunctionBlockBuilder {
    /// Whether the block is active (default: yes). A block of an Endpoint that
    /// declares its blocks static must stay active (§7.1.8); this core does not
    /// check it.
    pub const fn active(mut self, active: bool) -> Self {
        self.block.active = active;
        self
    }

    /// The block's UI hint (default: [`UiHint::Unknown`]).
    /// [`build`](Self::build) refuses a hint the direction rules out.
    pub const fn ui_hint(mut self, hint: UiHint) -> Self {
        self.block.ui_hint = hint;
        self
    }

    /// The block's UI hint as read off the wire: a hint the direction rules
    /// out is kept as [`UiHint::Unknown`], so a block a peer or an OS
    /// describes is never refused for it.
    pub const fn wire_ui_hint(self, hint: UiHint) -> Self {
        let hint = if contradicts(self.block.direction, hint) {
            UiHint::Unknown
        } else {
            hint
        };
        self.ui_hint(hint)
    }

    /// Whether the block is a MIDI 1.0 port (default: [`Midi1Port::No`]).
    /// [`build`](Self::build) refuses the reserved `0x03` and a port over more
    /// than one Group.
    pub const fn midi1(mut self, port: Midi1Port) -> Self {
        self.block.midi1 = port;
        self
    }

    /// The block's MIDI-CI Message Version/Format (`0` is none, the default,
    /// §7.1.8) and the number of simultaneous SysEx 8 streams it supports (`0`
    /// none, `1` one, `2..=255` that many).
    pub const fn midi_ci(mut self, version: u8, max_sysex8_streams: u8) -> Self {
        self.block.midi_ci_version = version;
        self.block.max_sysex8_streams = max_sysex8_streams;
        self
    }

    /// The block's name (default: none).
    pub const fn name(mut self, name: BlockName) -> Self {
        self.block.name = Some(name);
        self
    }

    /// The block, or why its fields do not go together.
    ///
    /// # Errors
    ///
    /// A [`BlockError`]: the reserved direction, Groups past Group 16, a UI hint
    /// that contradicts the direction, the reserved MIDI 1.0 port, or a MIDI 1.0
    /// port over several Groups.
    pub fn build(self) -> Result<FunctionBlock, BlockError> {
        let block = self.block;
        if matches!(block.direction, BlockDirection::Reserved(_)) {
            return Err(BlockError::ReservedDirection);
        }
        if u16::from(block.first_group.get()) + u16::from(block.groups.get()) > 16 {
            return Err(BlockError::PastGroup16 {
                first_group: block.first_group,
                groups: block.groups,
            });
        }
        if contradicts(block.direction, block.ui_hint) {
            return Err(BlockError::UiHintContradictsDirection {
                direction: block.direction,
                hint: block.ui_hint,
            });
        }
        match block.midi1 {
            Midi1Port::No => {}
            Midi1Port::Unrestricted | Midi1Port::Restricted if block.groups.get() == 1 => {}
            Midi1Port::Unrestricted | Midi1Port::Restricted => {
                return Err(BlockError::Midi1PortSpansGroups {
                    groups: block.groups,
                });
            }
            Midi1Port::Reserved(_) => return Err(BlockError::ReservedMidi1Port),
        }
        Ok(block)
    }
}

impl FunctionBlock {
    /// Starts a block of `groups` Groups from `first_group`: active, with no UI
    /// hint, not a MIDI 1.0 port, no MIDI-CI, no SysEx 8 and no name until the
    /// builder says otherwise.
    pub fn builder(
        id: FunctionBlockId,
        direction: BlockDirection,
        first_group: FirstGroup,
        groups: GroupSpan,
    ) -> FunctionBlockBuilder {
        FunctionBlockBuilder {
            block: Self {
                id,
                active: true,
                direction,
                ui_hint: UiHint::Unknown,
                midi1: Midi1Port::No,
                first_group,
                groups,
                midi_ci_version: 0,
                max_sysex8_streams: 0,
                name: None,
            },
        }
    }

    /// Its number.
    #[must_use]
    pub const fn id(&self) -> FunctionBlockId {
        self.id
    }

    /// Whether it is active.
    #[must_use]
    pub const fn active(&self) -> bool {
        self.active
    }

    /// Its direction.
    #[must_use]
    pub const fn direction(&self) -> BlockDirection {
        self.direction
    }

    /// Its UI hint.
    #[must_use]
    pub const fn ui_hint(&self) -> UiHint {
        self.ui_hint
    }

    /// Whether it is a MIDI 1.0 port.
    #[must_use]
    pub const fn midi1(&self) -> Midi1Port {
        self.midi1
    }

    /// The first Group it spans.
    #[must_use]
    pub const fn first_group(&self) -> FirstGroup {
        self.first_group
    }

    /// How many Groups it spans.
    #[must_use]
    pub const fn groups(&self) -> GroupSpan {
        self.groups
    }

    /// Its MIDI-CI Message Version/Format, `0` for none.
    #[must_use]
    pub const fn midi_ci_version(&self) -> u8 {
        self.midi_ci_version
    }

    /// Its simultaneous SysEx 8 streams.
    #[must_use]
    pub const fn max_sysex8_streams(&self) -> u8 {
        self.max_sysex8_streams
    }

    /// Its name, when it has one.
    #[must_use]
    pub const fn name(&self) -> Option<&BlockName> {
        self.name.as_ref()
    }

    fn info(&self) -> FunctionBlockInfo {
        FunctionBlockInfo {
            active: self.active,
            id: self.id,
            direction: self.direction,
            ui_hint: self.ui_hint,
            midi1: self.midi1,
            first_group: self.first_group,
            groups_spanned: self.groups,
            midi_ci_version: self.midi_ci_version,
            max_sysex8_streams: self.max_sysex8_streams,
        }
    }

    /// The block a peer's Function Block Info describes, or `None` when a
    /// field the wire kept is out of range: a number above 31, a Group span
    /// off the end, or a reserved direction.
    pub(crate) fn from_info(info: &FunctionBlockInfo) -> Option<Self> {
        let id = FunctionBlockId::new(info.id.get())?;
        let first_group = FirstGroup::new(info.first_group.get())?;
        let groups = GroupSpan::new(info.groups_spanned.get())?;
        // A peer's reserved MIDI 1.0 value, or a port over several Groups, is
        // kept as not a port: the block is still usable as MIDI 2.0.
        let midi1 = match info.midi1 {
            Midi1Port::Unrestricted | Midi1Port::Restricted if groups.get() == 1 => info.midi1,
            _ => Midi1Port::No,
        };
        Self::builder(id, info.direction, first_group, groups)
            .active(info.active)
            .wire_ui_hint(info.ui_hint)
            .midi1(midi1)
            .midi_ci(info.midi_ci_version, info.max_sysex8_streams)
            .build()
            .ok()
    }
}

/// Whether `hint` names a role `direction` rules out (§7.1.8): an
/// output-only block is not primarily a receiver, nor an input-only one
/// primarily a sender.
const fn contradicts(direction: BlockDirection, hint: UiHint) -> bool {
    matches!(
        (direction, hint),
        (BlockDirection::Output, UiHint::Receiver) | (BlockDirection::Input, UiHint::Sender)
    )
}

/// What [`Endpoint::reply`] did with one message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum Reply {
    /// The message is not one an Endpoint answers: nothing was sent and
    /// nothing changed.
    Ignored,
    /// A discovery was answered, with as many messages as the Endpoint has
    /// parts to report (none for a Function Block it does not have).
    Answered,
    /// A Stream Configuration Request was answered with a Notification, and
    /// the Endpoint's active configuration is now this.
    Configured(Configuration),
}

/// A change to what an [`Endpoint`] declares, applied by
/// [`Endpoint::change`], which sends the notification the spec asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EndpointChange {
    /// The protocols and JR Timestamps it supports: an Endpoint Info
    /// Notification (§7.1.2).
    Capabilities(Supports),
    /// A new Device Identity: a Device Identity Notification (§7.1.3).
    Identity(DeviceIdentity),
    /// A new name: an Endpoint Name Notification (§7.1.4).
    Name(EndpointName),
    /// A new Product Instance Id: a Product Instance Id Notification
    /// (§7.1.5).
    InstanceId(ProductInstanceId),
    /// A Function Block added, or replaced by the one of that number: an
    /// Endpoint Info Notification when the declared count grows (§7.1.2), a
    /// Function Block Info Notification when its properties change (§7.1.8,
    /// §6.1.2) and a Function Block Name Notification when its name does
    /// (§7.1.9).
    Block(FunctionBlock),
}

/// Why [`Endpoint::change`] refused a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum ChangeError {
    /// The Endpoint declared its Function Blocks static, so a block that is
    /// not the one held is refused: §7.1.2 fixes the set of blocks, and every
    /// property and name of each, after discovery. That includes a block
    /// with a number the Endpoint does not have yet.
    #[error("the Endpoint declared its Function Blocks static")]
    StaticBlocks,
}

/// How an Endpoint took a Stream Configuration Request (M2-104 §7.1.6.1 step
/// 3): the Protocol and JR Timestamps in use afterwards, and whether that is
/// what was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Configuration {
    /// Every part asked for is now active.
    Granted(StreamConfig),
    /// The Endpoint could not do some part (a Protocol or a JR direction it
    /// does not support, or a reserved Protocol): that part keeps its value,
    /// and the Notification reported it.
    Reduced {
        /// The configuration the request named.
        asked: StreamConfig,
        /// The configuration active now.
        active: StreamConfig,
    },
}

impl Configuration {
    /// The configuration active now, as the Notification reports it.
    #[must_use]
    pub const fn active(self) -> StreamConfig {
        match self {
            Self::Granted(active) | Self::Reduced { active, .. } => active,
        }
    }
}

/// What one UMP Endpoint declares and has active (M2-104 §7.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub(crate) version: UmpVersion,
    pub(crate) capabilities: Capabilities,
    pub(crate) active: StreamConfig,
    pub(crate) identity: Option<DeviceIdentity>,
    pub(crate) name: Option<EndpointName>,
    pub(crate) instance_id: Option<ProductInstanceId>,
    pub(crate) static_function_blocks: bool,
    /// The count Endpoint Info declares: `0..=32`.
    pub(crate) declared_blocks: u8,
    pub(crate) blocks: [Option<FunctionBlock>; MAX_FUNCTION_BLOCKS],
}

const MIDI1_NO_JR: StreamConfig = StreamConfig {
    protocol: Protocol::Midi1,
    rx_jr: false,
    tx_jr: false,
};

impl Endpoint {
    /// An Endpoint of UMP version [`UMP_VERSION`] with these capabilities, no
    /// Function Block and no name.
    ///
    /// It starts in the MIDI 1.0 Protocol (MIDI 2.0 when that is all it
    /// supports) with JR Timestamps off. The spec names no default; this is
    /// ours, and [`Endpoint::reply`] changes it on a Stream Configuration
    /// Request.
    #[must_use]
    pub const fn new(supports: Supports) -> Self {
        Self::declaring(supports.0)
    }

    /// An Endpoint declaring `capabilities`, which may be none that work: a
    /// discoverer's blank Endpoint, before the peer has said.
    const fn declaring(capabilities: Capabilities) -> Self {
        let protocol = if capabilities.midi2 && !capabilities.midi1 {
            Protocol::Midi2
        } else {
            Protocol::Midi1
        };
        Self {
            version: UMP_VERSION,
            capabilities,
            active: StreamConfig {
                protocol,
                rx_jr: false,
                tx_jr: false,
            },
            identity: None,
            name: None,
            instance_id: None,
            static_function_blocks: false,
            declared_blocks: 0,
            blocks: [None; MAX_FUNCTION_BLOCKS],
        }
    }

    /// A MIDI 1.0 device: no Stream messages exist for it, so
    /// it is an Endpoint with the MIDI 1.0 Protocol and one inferred Function
    /// Block, number 0, active, bidirectional, Group 0 alone, a MIDI 1.0 port
    /// of unrestricted bandwidth, with a UI hint of both. Its blocks are
    /// static.
    #[must_use]
    pub fn midi1_device(name: Option<EndpointName>) -> Self {
        let mut endpoint = Self::new(Supports::of(Capabilities {
            midi1: true,
            ..Capabilities::default()
        }))
        .with_static_function_blocks(true);
        endpoint.name = name;
        endpoint.insert_inferred_block();
        endpoint
    }

    fn insert_inferred_block(&mut self) {
        let (Some(id), Some(first), Some(span)) = (
            FunctionBlockId::new(0),
            FirstGroup::new(0),
            GroupSpan::new(1),
        ) else {
            return;
        };
        let block = FunctionBlock::builder(id, BlockDirection::Bidirectional, first, span)
            .ui_hint(UiHint::Both)
            .midi1(Midi1Port::Unrestricted)
            .build();
        if let Ok(block) = block {
            self.insert_block(block);
        }
    }

    /// The Endpoint a MIDI application presents for a port over `groups` from
    /// Group 1: UMP version 1.1, the MIDI 1.0 and MIDI 2.0 Protocols, no JR
    /// Timestamps either way, and one static, active, bidirectional Function
    /// Block, number 0, over those Groups. Its identity and names are the
    /// application's ([`with_identity`](Self::with_identity),
    /// [`with_name`](Self::with_name)).
    #[must_use]
    pub fn local(groups: GroupSpan) -> Self {
        let mut endpoint = Self::new(Supports::of(Capabilities {
            midi1: true,
            midi2: true,
            ..Capabilities::default()
        }))
        .with_static_function_blocks(true);
        if let (Some(id), Some(first)) = (FunctionBlockId::new(0), FirstGroup::new(0))
            && let Ok(block) =
                FunctionBlock::builder(id, BlockDirection::Bidirectional, first, groups).build()
        {
            endpoint.insert_block(block);
        }
        endpoint
    }

    /// An Endpoint with nothing known, for a discoverer to fill.
    pub(crate) const fn blank() -> Self {
        let mut endpoint = Self::declaring(Capabilities {
            midi1: false,
            midi2: false,
            rx_jr: false,
            tx_jr: false,
        });
        endpoint.version = UmpVersion { major: 0, minor: 0 };
        endpoint
    }

    /// The Endpoint with the Protocol and JR Timestamps in use now set, for an
    /// Endpoint whose state an OS reports rather than one that was asked
    /// (§7.1.6.3). Nothing is checked against its capabilities.
    #[must_use]
    pub const fn with_active(mut self, active: StreamConfig) -> Self {
        self.active = active;
        self
    }

    /// The Endpoint with a Device Identity (§7.1.3).
    #[must_use]
    pub const fn with_identity(mut self, identity: DeviceIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// The Endpoint with a name (§7.1.4).
    #[must_use]
    pub const fn with_name(mut self, name: EndpointName) -> Self {
        self.name = Some(name);
        self
    }

    /// The Endpoint with a Product Instance Id (§7.1.5).
    #[must_use]
    pub const fn with_instance_id(mut self, id: ProductInstanceId) -> Self {
        self.instance_id = Some(id);
        self
    }

    /// The Endpoint declaring whether its Function Blocks never change
    /// (§7.1.2).
    #[must_use]
    pub const fn with_static_function_blocks(mut self, is_static: bool) -> Self {
        self.static_function_blocks = is_static;
        self
    }

    /// The Endpoint with a Function Block, replacing the one of that number.
    /// The block count it declares is the highest number plus one.
    #[must_use]
    pub fn with_function_block(mut self, block: FunctionBlock) -> Self {
        self.insert_block(block);
        self
    }

    pub(crate) fn insert_block(&mut self, block: FunctionBlock) {
        let i = usize::from(block.id.get());
        self.blocks[i] = Some(block);
        let count = u8::try_from(i + 1).unwrap_or(u8::MAX);
        self.declared_blocks = self.declared_blocks.max(count);
    }

    /// The UMP version it declares.
    #[must_use]
    pub const fn version(&self) -> UmpVersion {
        self.version
    }

    /// The protocols and JR Timestamps it supports.
    #[must_use]
    pub const fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// The Protocol and JR Timestamps in use now (§7.1.6.3).
    #[must_use]
    pub const fn active(&self) -> StreamConfig {
        self.active
    }

    /// Its Device Identity.
    #[must_use]
    pub const fn identity(&self) -> Option<&DeviceIdentity> {
        self.identity.as_ref()
    }

    /// Its name.
    #[must_use]
    pub const fn name(&self) -> Option<&EndpointName> {
        self.name.as_ref()
    }

    /// Its Product Instance Id.
    #[must_use]
    pub const fn instance_id(&self) -> Option<&ProductInstanceId> {
        self.instance_id.as_ref()
    }

    /// Whether it declares its Function Blocks static.
    #[must_use]
    pub const fn static_function_blocks(&self) -> bool {
        self.static_function_blocks
    }

    /// The number of Function Blocks it declares (§7.1.2), `0..=32`.
    #[must_use]
    pub const fn declared_function_blocks(&self) -> u8 {
        self.declared_blocks
    }

    /// Function Block `id`, when it is known.
    #[must_use]
    pub fn function_block(&self, id: FunctionBlockId) -> Option<&FunctionBlock> {
        self.blocks[usize::from(id.get())].as_ref()
    }

    /// The known Function Blocks, by number.
    pub fn function_blocks(&self) -> impl Iterator<Item = &FunctionBlock> {
        self.blocks.iter().flatten()
    }

    /// Answers one received message into `out`, in the order of the spec; the
    /// step allocates nothing itself.
    ///
    /// - Endpoint Discovery (§7.1.1): one reply for each filter bit set, in
    ///   the order Endpoint Info, Device Identity, Endpoint Name, Product
    ///   Instance Id, Stream Configuration Notification. A reply for a part
    ///   the Endpoint does not have (no identity, no name) is left out. A name
    ///   that is empty is answered with one Complete message of zeros: each
    ///   Filter bit set is owed a reply (M2-104 section 7.1.1).
    /// - Function Block Discovery (§7.1.7): Function Block Info (§7.1.8) for
    ///   the block asked for, or for every known block for `0xFF`, then, if
    ///   asked, each Function Block Name (§7.1.9). A number above 31, or of a
    ///   block that is not known, answers nothing (the spec names no reply).
    /// - Stream Configuration Request (§7.1.6.2): each part it can honour is
    ///   taken (a Protocol it supports, JR directions it supports, JR off
    ///   always), and the Notification (§7.1.6.3) reports what is now
    ///   active, which is the current configuration for a part it could not
    ///   change (§7.1.6.1 step 3).
    ///
    /// Any other message answers nothing ([`Reply::Ignored`]). A Stream
    /// Configuration Request's [`Reply::Configured`] says whether the request was
    /// granted whole, so the owner of the port can switch to the Protocol and JR
    /// Timestamps now active.
    ///
    /// The requester's UMP version (§7.1.1) changes no answer, and that is the
    /// rule applied: Table 5 gives 1.1 as the first version, and every field
    /// this reads (the five filter bits) is defined in it, so a higher version
    /// is answered with the fields of 1.1 (its appended fields and reserved
    /// bits are not read), and the spec defines no lower one to answer in.
    pub fn reply(&mut self, message: Message, out: &mut impl Extend<Ump>) -> Reply {
        let Message::Stream(stream) = message else {
            return Reply::Ignored;
        };
        match stream {
            Stream::EndpointDiscovery { filter, .. } => {
                self.answer_discovery(filter, out);
                Reply::Answered
            }
            Stream::FunctionBlockDiscovery { block, filter } => {
                self.answer_block_discovery(block, filter, out);
                Reply::Answered
            }
            Stream::StreamConfigurationRequest(request) => {
                self.configure(request);
                send(out, Stream::StreamConfigurationNotification(self.active));
                Reply::Configured(if self.active == request {
                    Configuration::Granted(self.active)
                } else {
                    Configuration::Reduced {
                        asked: request,
                        active: self.active,
                    }
                })
            }
            _ => Reply::Ignored,
        }
    }

    fn answer_discovery(&self, filter: DiscoveryFilter, out: &mut impl Extend<Ump>) {
        if filter.endpoint_info {
            self.send_info(out);
        }
        if let (true, Some(identity)) = (filter.device_identity, self.identity) {
            send(out, Stream::DeviceIdentity(identity));
        }
        if let (true, Some(name)) = (filter.endpoint_name, &self.name) {
            send_name(out, name);
        }
        if let (true, Some(id)) = (filter.product_instance_id, &self.instance_id) {
            send_instance_id(out, id);
        }
        if filter.stream_configuration {
            send(out, Stream::StreamConfigurationNotification(self.active));
        }
    }

    fn send_info(&self, out: &mut impl Extend<Ump>) {
        send(
            out,
            Stream::EndpointInfo(tutti_ump::EndpointInfo {
                version: self.version,
                static_function_blocks: self.static_function_blocks,
                function_blocks: U7::masked(self.declared_blocks),
                midi2: self.capabilities.midi2,
                midi1: self.capabilities.midi1,
                rx_jr: self.capabilities.rx_jr,
                tx_jr: self.capabilities.tx_jr,
            }),
        );
    }

    /// Applies `change` and sends into `out` the notifications it calls for,
    /// in the order Endpoint Info, then the part that changed. A change that
    /// leaves the Endpoint as it was sends nothing.
    ///
    /// A [`EndpointChange::Capabilities`] that drops what is active (a
    /// Protocol, RXJR or TXJR) also changes the active configuration to what
    /// is left and sends its Stream Configuration Notification (§7.1.6.3).
    ///
    /// The spec has no message that removes a block name. A block replaced by
    /// one with no name sends one Complete Function Block Name of zeros, the
    /// way this crate sends an empty name (§7.1.1 does for an Endpoint's), so
    /// a peer that reads it as empty drops the old one; that is this crate's
    /// choice.
    ///
    /// # Errors
    ///
    /// [`ChangeError::StaticBlocks`] for a [`EndpointChange::Block`] that
    /// differs from the block held, when the Endpoint declared its blocks
    /// static (§7.1.2): nothing is changed or sent.
    pub fn change(
        &mut self,
        change: EndpointChange,
        out: &mut impl Extend<Ump>,
    ) -> Result<(), ChangeError> {
        match change {
            EndpointChange::Capabilities(supports) => {
                let capabilities = supports.get();
                if capabilities != self.capabilities {
                    self.capabilities = capabilities;
                    self.send_info(out);
                    // What it no longer supports does not stay active: the
                    // Protocol falls to the one left (the choice `new` makes
                    // for an Endpoint of those capabilities) and JR Timestamps
                    // go off. Its Stream Configuration Notification follows
                    // when that changed the active configuration (§7.1.6.3:
                    // sent when any property in it changes).
                    let was = self.active;
                    let supported = match self.active.protocol {
                        Protocol::Midi1 => capabilities.midi1,
                        Protocol::Midi2 => capabilities.midi2,
                        _ => false,
                    };
                    if !supported {
                        // One Protocol is supported: `Supports` has at least one.
                        self.active.protocol = if capabilities.midi2 && !capabilities.midi1 {
                            Protocol::Midi2
                        } else {
                            Protocol::Midi1
                        };
                    }
                    self.active.rx_jr &= capabilities.rx_jr;
                    self.active.tx_jr &= capabilities.tx_jr;
                    if self.active != was {
                        send(out, Stream::StreamConfigurationNotification(self.active));
                    }
                }
            }
            EndpointChange::Identity(identity) => {
                if self.identity != Some(identity) {
                    self.identity = Some(identity);
                    send(out, Stream::DeviceIdentity(identity));
                }
            }
            EndpointChange::Name(name) => {
                if self.name.as_ref() != Some(&name) {
                    send_name(out, &name);
                    self.name = Some(name);
                }
            }
            EndpointChange::InstanceId(id) => {
                if self.instance_id.as_ref() != Some(&id) {
                    send_instance_id(out, &id);
                    self.instance_id = Some(id);
                }
            }
            EndpointChange::Block(block) => {
                let old = self.blocks[usize::from(block.id.get())];
                if old == Some(block) {
                    return Ok(());
                }
                if self.static_function_blocks {
                    return Err(ChangeError::StaticBlocks);
                }
                let declared = self.declared_blocks;
                self.insert_block(block);
                if self.declared_blocks != declared {
                    self.send_info(out);
                }
                if old.map(|b| b.info()) != Some(block.info()) {
                    send(out, Stream::FunctionBlockInfo(block.info()));
                }
                let old_name = old.and_then(|b| b.name);
                if old_name != block.name && (block.name.is_some() || old_name.is_some()) {
                    let bytes = block.name.as_ref().map_or(&[][..], BlockName::as_bytes);
                    for (form, bytes) in text_packets::<13>(bytes) {
                        send(
                            out,
                            Stream::FunctionBlockName {
                                block: block.id,
                                form,
                                bytes,
                            },
                        );
                    }
                }
            }
        }
        Ok(())
    }

    fn answer_block_discovery(
        &self,
        select: BlockSelect,
        filter: BlockFilter,
        out: &mut impl Extend<Ump>,
    ) {
        let one;
        let asked: &[Option<FunctionBlock>] = match select {
            BlockSelect::All => &self.blocks,
            BlockSelect::One(id) => match self.blocks.get(usize::from(id.get())) {
                Some(slot) => {
                    one = [*slot];
                    &one
                }
                None => return,
            },
        };
        if filter.info {
            for block in asked.iter().flatten() {
                send(out, Stream::FunctionBlockInfo(block.info()));
            }
        }
        if filter.name {
            for block in asked.iter().flatten() {
                if let Some(name) = &block.name {
                    for (form, bytes) in text_packets::<13>(name.as_bytes()) {
                        send(
                            out,
                            Stream::FunctionBlockName {
                                block: block.id,
                                form,
                                bytes,
                            },
                        );
                    }
                }
            }
        }
    }

    fn configure(&mut self, request: StreamConfig) {
        let caps = self.capabilities;
        self.active.protocol = match request.protocol {
            Protocol::Midi1 if caps.midi1 => Protocol::Midi1,
            Protocol::Midi2 if caps.midi2 => Protocol::Midi2,
            _ => self.active.protocol,
        };
        // Turning JR off is always possible; on, only where it is supported.
        if !request.rx_jr || caps.rx_jr {
            self.active.rx_jr = request.rx_jr;
        }
        if !request.tx_jr || caps.tx_jr {
            self.active.tx_jr = request.tx_jr;
        }
    }

    /// Folds a peer's Endpoint Info into this (discoverer) value. A count of
    /// Function Blocks above [`MAX_FUNCTION_BLOCKS`] is refused (`false`, and
    /// nothing is folded): the numbers are 0 to 31, so there is no Endpoint
    /// with more.
    #[must_use]
    pub(crate) fn apply_info(&mut self, info: &tutti_ump::EndpointInfo) -> bool {
        if usize::from(info.function_blocks.get()) > MAX_FUNCTION_BLOCKS {
            return false;
        }
        self.version = info.version;
        self.static_function_blocks = info.static_function_blocks;
        self.declared_blocks = info.function_blocks.get();
        self.capabilities = Capabilities {
            midi1: info.midi1,
            midi2: info.midi2,
            rx_jr: info.rx_jr,
            tx_jr: info.tx_jr,
        };
        true
    }

    /// The block a peer's Function Block Info describes, read as this
    /// Endpoint's: `None` for one `from_info` refuses and for one outside the
    /// count the Endpoint declared (its numbers are `0..declared`, §7.1.7). A
    /// block a peer that declared its blocks static sends inactive is read
    /// active: §7.1.8 says such an Endpoint has no inactive
    /// Function Blocks, every one having the bit set. A MIDI 1.0 port over several Groups is already read as a plain
    /// block (`from_info`: §7.1.8 requires it to span one Group).
    pub(crate) fn read_block(&self, info: &FunctionBlockInfo) -> Option<FunctionBlock> {
        let mut block = FunctionBlock::from_info(info)?;
        if block.id.get() >= self.declared_blocks {
            return None;
        }
        if self.static_function_blocks {
            block.active = true;
        }
        Some(block)
    }

    /// What a MIDI 1.0 device is: the Endpoint as the discoverer knows it, but
    /// in the MIDI 1.0 Protocol with no JR Timestamps, and with the inferred
    /// block if no block is known.
    pub(crate) fn fall_back_to_midi1(&mut self) {
        self.active = MIDI1_NO_JR;
        self.capabilities.midi1 = true;
        if self.function_blocks().next().is_none() {
            self.declared_blocks = 0;
            self.insert_inferred_block();
        }
    }
}

fn send_name(out: &mut impl Extend<Ump>, name: &EndpointName) {
    for (form, b) in text_packets::<14>(name.as_bytes()) {
        send(out, Stream::EndpointName(form, b));
    }
}

fn send_instance_id(out: &mut impl Extend<Ump>, id: &ProductInstanceId) {
    for (form, b) in text_packets::<14>(id.as_str().as_bytes()) {
        send(out, Stream::ProductInstanceId(form, b));
    }
}

fn send(out: &mut impl Extend<Ump>, stream: Stream) {
    out.extend(std::iter::once(Ump::from(Message::Stream(stream))));
}

#[cfg(test)]
mod tests;
