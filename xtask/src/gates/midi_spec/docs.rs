//! The documents the extractor reads: one [`Doc`] each, with its tables.

use super::{Doc, Named, Rows, Tables};

/// The documents, one inventory file each.
pub(in crate::gates) const DOCS: &[Doc] = &[
    M2_101, M2_102, M2_103, M2_104, M2_115, M2_116, M2_105, M2_106, M2_107, M2_108, M2_109, M2_111,
    M2_112, M2_117, M2_113, M2_118, M2_119, M2_120, M2_121, M2_122, M2_123, M2_125, M2_124, SMF,
];

/// The header note of a document with its own Table 2 and no tables of rows.
const SAME_TABLES: &str = "\
# This document has its own conformance-word table (its Table 2), the same three
# rows as M2-104's, and no negated forms either. Its Table 3 lists must,
# will, can and might as words that are never conformance statements: a
# sentence with only those has no row. Sentence rows only: no table of the
# document has a row per item.
";

/// The header note of a document whose own conformance-word table is its
/// Table 1 and whose Table 2 lists the words that are not.
const TABLE_1: &str = "\
# This document has its own conformance-word table (its Table 1), the same three
# rows as M2-104's, and no negated forms either. Its Table 2 lists must,
# will, can and might as words that are never conformance statements: a
# sentence with only those has no row. Sentence rows only: no table of the
# document has a row per item.
";

pub(in crate::gates) const M2_101: Doc = Doc {
    prefix: "m2-101",
    label: "M2-101",
    title: "MIDI Capability Inquiry (M2-101 v1.2.1)",
    file: "midi-m2-101.txt",
    source: "M2-101-UM",
    sides: "initiator, responder or both",
    note: SAME_TABLES,
    implicit_levels: false,
    dotted_top: false,
    tables: Tables::None,
};

pub(in crate::gates) const M2_102: Doc = Doc {
    prefix: "m2-102",
    label: "M2-102",
    title: "Common Rules for MIDI-CI Profiles (M2-102 v1.1)",
    file: "midi-m2-102.txt",
    source: "M2-102-U",
    sides: "initiator, responder or both",
    note: SAME_TABLES,
    implicit_levels: false,
    dotted_top: false,
    tables: Tables::None,
};

pub(in crate::gates) const M2_103: Doc = Doc {
    prefix: "m2-103",
    label: "M2-103",
    title: "Common Rules for MIDI-CI Property Exchange (M2-103 v1.2)",
    file: "midi-m2-103.txt",
    source: "M2-103-UM",
    sides: "initiator, responder or both",
    note: SAME_TABLES,
    implicit_levels: false,
    dotted_top: false,
    tables: Tables::None,
};

pub(in crate::gates) const M2_115: Doc = Doc {
    prefix: "m2-115",
    label: "M2-115",
    title: "MIDI 2.0 Bit Scaling and Resolution (M2-115 v1.0.1)",
    file: "midi-m2-115.txt",
    source: "M2-115-U",
    sides: "sender, receiver or both",
    note: SAME_TABLES,
    implicit_levels: false,
    dotted_top: false,
    tables: Tables::None,
};

pub(in crate::gates) const M2_104: Doc = Doc {
    prefix: "m2-104",
    label: "M2-104",
    title: "MIDI 2.0 UMP and Protocol (M2-104 v1.1.2)",
    file: "midi-m2-104.txt",
    source: "M2-104-UM",
    sides: "sender, receiver or both",
    note: "",
    implicit_levels: false,
    dotted_top: false,
    tables: Tables::Messages {
        mt_table: 4,
        messages: &[(34, Rows::Formats), (16, Rows::FlexStatus)],
    },
};

pub(in crate::gates) const M2_116: Doc = Doc {
    prefix: "m2-116",
    label: "M2-116",
    title: "MIDI Clip File (M2-116 v1.0)",
    file: "midi-m2-116.txt",
    source: "M2-116-U",
    sides: "writer, reader or both",
    note: "\
# M2-116 has its own conformance-word table (its Table 2), the same three
# rows as M2-104's, and no negated forms either. Section 6.2.1.1 follows 6.2
# with no 6.2.1 between; it keeps its own number as the section.
#   m2-116/hdr/<n>                the nth byte of the file header: facts
#                                 byte (1-based) and value
",
    implicit_levels: true,
    dotted_top: false,
    tables: Tables::FileHeader { table: 5 },
};

pub(in crate::gates) const M2_105: Doc = Doc {
    prefix: "m2-105",
    label: "M2-105",
    title: "Property Exchange Foundational Resources (M2-105 v1.1.1)",
    file: "midi-m2-105.txt",
    source: "M2-105-UM",
    sides: "initiator, responder or both",
    note: SAME_TABLES,
    implicit_levels: false,
    dotted_top: false,
    tables: Tables::None,
};

pub(in crate::gates) const M2_106: Doc = Doc {
    prefix: "m2-106",
    label: "M2-106",
    title: "Property Exchange Mode Resources (M2-106 v1.01)",
    file: "midi-m2-106.txt",
    source: "M2-106-UM",
    sides: "initiator, responder or both",
    note: TABLE_1,
    implicit_levels: false,
    dotted_top: true,
    tables: Tables::None,
};

pub(in crate::gates) const M2_107: Doc = Doc {
    prefix: "m2-107",
    label: "M2-107",
    title: "Property Exchange ProgramList Resource (M2-107 v1.01)",
    file: "midi-m2-107.txt",
    source: "M2-107-UM",
    sides: "initiator, responder or both",
    note: TABLE_1,
    implicit_levels: false,
    dotted_top: true,
    tables: Tables::None,
};

pub(in crate::gates) const M2_108: Doc = Doc {
    prefix: "m2-108",
    label: "M2-108",
    title: "Property Exchange Channel Resources (M2-108 v1.01)",
    file: "midi-m2-108.txt",
    source: "M2-108-UM",
    sides: "initiator, responder or both",
    note: TABLE_1,
    implicit_levels: false,
    dotted_top: true,
    tables: Tables::None,
};

pub(in crate::gates) const M2_109: Doc = Doc {
    prefix: "m2-109",
    label: "M2-109",
    title: "Property Exchange LocalOn Resource (M2-109 v1.01)",
    file: "midi-m2-109.txt",
    source: "M2-109-UM",
    sides: "initiator, responder or both",
    note: TABLE_1,
    implicit_levels: false,
    dotted_top: true,
    tables: Tables::None,
};

pub(in crate::gates) const M2_111: Doc = Doc {
    prefix: "m2-111",
    label: "M2-111",
    title: "Get and Set Device State (M2-111 v1.0)",
    file: "midi-m2-111.txt",
    source: "M2-111-UM",
    sides: "initiator, responder or both",
    note: TABLE_1,
    implicit_levels: false,
    dotted_top: true,
    tables: Tables::None,
};

pub(in crate::gates) const M2_112: Doc = Doc {
    prefix: "m2-112",
    label: "M2-112",
    title: "Property Exchange ExternalSync Resource (M2-112 v1.0)",
    file: "midi-m2-112.txt",
    source: "M2-112-UM",
    sides: "initiator, responder or both",
    note: TABLE_1,
    implicit_levels: false,
    dotted_top: true,
    tables: Tables::None,
};

pub(in crate::gates) const M2_117: Doc = Doc {
    prefix: "m2-117",
    label: "M2-117",
    title: "Property Exchange Controller Resources (M2-117 v1.0)",
    file: "midi-m2-117.txt",
    source: "M2-117-UM",
    sides: "initiator, responder or both",
    note: SAME_TABLES,
    implicit_levels: false,
    dotted_top: false,
    tables: Tables::None,
};

/// The header note of a document with no conformance-word table of its own.
const NO_TABLE: &str = "\
# This document has no conformance-word table; its keyword sentences are read
# with the three rows of M2-104's Table 2 (shall is required, should
# recommended, may permitted), and no negated forms. Sentence rows only.
";

pub(in crate::gates) const M2_113: Doc = Doc {
    prefix: "m2-113",
    label: "M2-113",
    title: "Default Control Change Mapping Profile (M2-113 v1.0)",
    file: "midi-m2-113.txt",
    source: "M2-113-UM",
    sides: "sender, receiver or both",
    note: NO_TABLE,
    implicit_levels: false,
    dotted_top: true,
    tables: Tables::None,
};

pub(in crate::gates) const M2_118: Doc = Doc {
    prefix: "m2-118",
    label: "M2-118",
    title: "General MIDI 2 Function Block Profile (M2-118 v1.0.0)",
    file: "midi-m2-118.txt",
    source: "M2-118-UM",
    sides: "sender, receiver or both",
    note: SAME_TABLES,
    implicit_levels: true,
    dotted_top: false,
    tables: Tables::None,
};

pub(in crate::gates) const M2_119: Doc = Doc {
    prefix: "m2-119",
    label: "M2-119",
    title: "General MIDI 2 Single Channel Profile (M2-119 v1.0.0)",
    file: "midi-m2-119.txt",
    source: "M2-119-UM",
    sides: "sender, receiver or both",
    note: SAME_TABLES,
    implicit_levels: true,
    dotted_top: false,
    tables: Tables::None,
};

pub(in crate::gates) const M2_120: Doc = Doc {
    prefix: "m2-120",
    label: "M2-120",
    title: "MIDI Polyphonic Expression Profile (M2-120 v2.0.3)",
    file: "midi-m2-120.txt",
    source: "M2-120-UM",
    sides: "sender, receiver or both",
    note: SAME_TABLES,
    implicit_levels: true,
    dotted_top: false,
    tables: Tables::None,
};

pub(in crate::gates) const M2_121: Doc = Doc {
    prefix: "m2-121",
    label: "M2-121",
    title: "Drawbar Organ Profile (M2-121 v1.0.2)",
    file: "midi-m2-121.txt",
    source: "M2-121-UM",
    sides: "sender, receiver or both",
    note: SAME_TABLES,
    implicit_levels: true,
    dotted_top: false,
    tables: Tables::None,
};

pub(in crate::gates) const M2_122: Doc = Doc {
    prefix: "m2-122",
    label: "M2-122",
    title: "Rotary Speaker Profile (M2-122 v1.0.2)",
    file: "midi-m2-122.txt",
    source: "M2-122-UM",
    sides: "sender, receiver or both",
    note: SAME_TABLES,
    implicit_levels: true,
    dotted_top: false,
    tables: Tables::None,
};

pub(in crate::gates) const M2_123: Doc = Doc {
    prefix: "m2-123",
    label: "M2-123",
    title: "Note On Orchestral Articulation Profile (M2-123 v1.0)",
    file: "midi-m2-123.txt",
    source: "M2-123-UM",
    sides: "sender, receiver or both",
    note: SAME_TABLES,
    implicit_levels: true,
    dotted_top: false,
    tables: Tables::None,
};

pub(in crate::gates) const M2_124: Doc = Doc {
    prefix: "m2-124",
    label: "M2-124",
    title: "Network MIDI 2.0 (UDP) (M2-124 v1.0.1)",
    file: "midi-m2-124.txt",
    source: "M2-124-UM_v1-0-1_",
    sides: "client, host or both",
    note: SAME_TABLES,
    implicit_levels: false,
    dotted_top: false,
    tables: Tables::None,
};

pub(in crate::gates) const M2_125: Doc = Doc {
    prefix: "m2-125",
    label: "M2-125",
    title: "Default Drum Note Map Profile (M2-125 v1.0)",
    file: "midi-m2-125.txt",
    source: "M2-125-UM",
    sides: "sender, receiver or both",
    note: SAME_TABLES,
    implicit_levels: true,
    dotted_top: false,
    tables: Tables::None,
};

/// The Standard MIDI Files 1.0 reading of [`Named`].
pub(super) const SMF_TEXT: Named = Named {
    headings: &[
        (
            "Sequences, Tracks, Chunks: File Block Structure",
            "structure",
        ),
        ("Conventions", "conventions"),
        ("Files", "files"),
        ("Chunks", "chunks"),
        ("Header Chunks", "header-chunk"),
        ("Formats 0, 1, and 2", "formats"),
        ("Track Chunks", "track-chunks"),
        ("Meta-Events", "meta-events"),
        ("Sequence Number", "sequence-number"),
        ("Text Event", "text-event"),
        ("Copyright Notice", "copyright-notice"),
        ("Sequence/Track Name", "track-name"),
        ("Instrument Name", "instrument-name"),
        ("Lyric", "lyric"),
        ("Marker", "marker"),
        ("Cue Point", "cue-point"),
        ("MIDI Channel Prefix", "channel-prefix"),
        ("End of Track", "end-of-track"),
        (
            "Set Tempo, in microseconds per MIDI quarter-note",
            "set-tempo",
        ),
        ("SMPTE Offset", "smpte-offset"),
        ("Time Signature", "time-signature"),
        ("Key Signature", "key-signature"),
        ("Sequencer-Specific Meta-Event", "sequencer-specific"),
        ("Program Fragments and Example MIDI Files", "examples"),
        ("Calculating Delta Times:", "delta-times"),
    ],
    end: "General MIDI System Level 1",
    footer: "Standard MIDI Files 1.0",
    words: &[
        ("must", "required"),
        ("shall", "required"),
        ("required", "required"),
        ("important", "required"),
        ("vital", "required"),
        ("should", "recommended"),
        ("polite", "recommended"),
        ("idea", "recommended"),
        ("ideally", "recommended"),
        ("may", "permitted"),
        ("optional", "permitted"),
        ("always", "required"),
        ("never", "required"),
        ("assumed", "required"),
    ],
    phrases: &[
        ("not required", "may"),
        ("not optional", "must"),
        ("a/ways", "always"),
        // A "should" inside a definition of what an event does not imply (the spec writes F0 as `FO`)
        // binds nothing; the sentence defines the F7 event form.
        ("imply that an FO should", "imply that an FO is to"),
    ],
    defs: &[
        "stored",
        "represented",
        "represents",
        "begins",
        "followed",
        "specifies",
        "expressed",
        "encoded",
        "occupy",
        "contains",
        "cancel",
        "expresses",
        "indicates",
        "designates",
        "represent",
        "correspond",
        "refer",
        "refers",
        "representation",
        "flats",
        "has",
        "number",
        "reserved",
        "omitted",
        "imply",
        "ascii",
    ],
};

/// The Standard MIDI Files 1.0 document (RP-001, February 1996).
pub(in crate::gates) const SMF: Doc = Doc {
    prefix: "smf",
    label: "SMF 1.0",
    title: "Standard MIDI Files 1.0 (RP-001, February 1996)",
    file: "midi-smf.txt",
    source: "complete_midi_96",
    sides: "writer, reader or both",
    note: "",
    implicit_levels: false,
    dotted_top: false,
    tables: Tables::Named(&SMF_TEXT),
};
