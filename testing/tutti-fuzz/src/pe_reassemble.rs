use tutti_midi_ci::Muid;
use tutti_midi_ci::{
    BodyLimit, BufferLimit, PeChunk, PeData, PeEncoding, PeFlowControl, PeHeader, PeJson,
    PeMessageKind, PeReassembler, PeReassemblyLimits, PeResource, PeStep, ResourceName,
    StreamLimit, decode_body, encode_body, mcoded7,
};
use tutti_platform::Instant;

/// The most Property Data bytes a decode may return.
const BODY: usize = 4096;
/// The most bytes the reassembler may hold.
const BUFFER: usize = 4096;
/// The most messages the reassembler may hold.
const STREAMS: u8 = 4;

const KINDS: [PeMessageKind; 6] = [
    PeMessageKind::GET_INQUIRY,
    PeMessageKind::GET_REPLY,
    PeMessageKind::SET_INQUIRY,
    PeMessageKind::SET_REPLY,
    PeMessageKind::SUBSCRIPTION_INQUIRY,
    PeMessageKind::SUBSCRIPTION_REPLY,
];

const RESOURCES: [&str; 17] = [
    "ResourceList",
    "DeviceInfo",
    "ChannelList",
    "JSONSchema",
    "ModeList",
    "CurrentMode",
    "ChannelMode",
    "BasicChannelRx",
    "BasicChannelTx",
    "ProgramList",
    "LocalOn",
    "ExternalSync",
    "AllCtrlList",
    "ChCtrlList",
    "CtrlMapList",
    "StateList",
    "State",
];

/// Property Exchange's receive path over `data`.
///
/// The first byte picks the message kind and Flow Control; the rest is read
/// whole as Header Data, Mcoded7, zlib and JSON Property Data, and as chunks:
/// each a length byte and that many bytes of chunk fields. The reassembler
/// never holds more than its limits, and a decoded body is never longer than
/// the body limit, so a zlib bomb is refused.
pub(crate) fn pe_reassemble(data: &[u8]) {
    let Some((&pick, rest)) = data.split_first() else {
        return;
    };
    let _ = PeHeader::try_from(rest);
    for encoding in [
        PeEncoding::Ascii,
        PeEncoding::Mcoded7,
        PeEncoding::ZlibMcoded7,
    ] {
        let limit = BodyLimit::new(BODY).expect("a positive limit");
        if let Ok(body) = decode_body(encoding, rest, limit) {
            assert!(body.len() <= BODY, "{encoding:?} body past its limit");
        }
    }
    let encoded = mcoded7::encode_vec(rest);
    assert_eq!(mcoded7::decode_vec(&encoded).as_deref(), Ok(rest));
    for encoding in [
        PeEncoding::Ascii,
        PeEncoding::Mcoded7,
        PeEncoding::ZlibMcoded7,
    ] {
        if let Ok(wire) = encode_body(encoding, rest) {
            let limit = BodyLimit::new(rest.len().max(1)).expect("a positive limit");
            assert_eq!(decode_body(encoding, &wire, limit).as_deref(), Ok(rest));
        }
    }
    if let Ok(json) = PeJson::try_from(rest) {
        for name in RESOURCES {
            let resource = PeResource::from_name(&ResourceName::new(name).expect("a name"));
            let _ = PeData::decode(&resource, &json);
        }
    }

    let kind = KINDS[usize::from(pick) % KINDS.len()];
    let flow = if pick & 0x80 == 0 {
        PeFlowControl::Off
    } else {
        PeFlowControl::On
    };
    let mut inbox = PeReassembler::new(PeReassemblyLimits {
        streams: StreamLimit::new(STREAMS).expect("a stream limit"),
        bytes: BufferLimit::new(BUFFER).expect("a buffer limit"),
    });
    let now = Instant::now();
    let mut records = rest;
    while let Some((&len, tail)) = records.split_first() {
        let (fields, tail) = tail.split_at(usize::from(len).min(tail.len()));
        records = tail;
        let Ok(chunk) = PeChunk::try_from(fields) else {
            continue;
        };
        if let Ok(PeStep::Complete { message, .. }) =
            inbox.push(Muid::of(1), kind, chunk, flow, now)
        {
            let limit = BodyLimit::new(BODY).expect("a positive limit");
            let _ = decode_body(PeEncoding::ZlibMcoded7, &message.body, limit);
        }
        let held = inbox.buffered();
        assert!(held.bytes <= BUFFER, "{} bytes held", held.bytes);
        assert!(held.streams <= usize::from(STREAMS));
    }
}
