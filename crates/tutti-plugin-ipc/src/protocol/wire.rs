//! Serde adapters for wire fields whose in-memory type is not serde.

/// A block's MIDI list on the wire: a sequence of `(offset, [u32; 4])`.
///
/// [`WireMidi`](tutti_plugin_types::WireMidi) is serde-free on purpose: its
/// offset is relative to one block, so it is not a persistable value. The wire
/// is the one place it is encoded, and only through this adapter, used as
/// `#[serde(with = "wire::midi")]`. The offset is a raw `u32` because the
/// types stop at the IPC boundary (CLAUDE.md, "Where the types stop"); it
/// becomes an `Offset` where the receiving side knows the block length.
///
/// Encoding walks the list in place and allocates nothing. Decoding builds the
/// inline [`MidiEventVec`](tutti_plugin_types::MidiEventVec) and rejects a list longer than
/// [`MIDI_STACK_CAPACITY`](tutti_plugin_types::MIDI_STACK_CAPACITY) instead of truncating it.
pub mod midi {
    use std::fmt;

    use serde::de::{self, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};
    use tutti_plugin_types::{MIDI_STACK_CAPACITY, MidiEventVec, WireMidi};
    use tutti_ump::Ump;

    /// Encodes each event as `(offset, packet words)`.
    pub fn serialize<S: Serializer>(events: &MidiEventVec, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(events.iter().map(|e| (e.offset, e.ump.0)))
    }

    /// Decodes a list of at most [`MIDI_STACK_CAPACITY`] events.
    ///
    /// # Errors
    ///
    /// A longer list is a decode error.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<MidiEventVec, D::Error> {
        struct Events;

        impl<'de> Visitor<'de> for Events {
            type Value = MidiEventVec;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "at most {MIDI_STACK_CAPACITY} MIDI events")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                if let Some(len) = seq.size_hint()
                    && len > MIDI_STACK_CAPACITY
                {
                    return Err(de::Error::invalid_length(len, &self));
                }
                let mut out = MidiEventVec::new();
                while let Some((offset, data)) = seq.next_element::<(u32, [u32; 4])>()? {
                    out.try_push(WireMidi {
                        offset,
                        ump: Ump(data),
                    })
                    .map_err(|_| de::Error::invalid_length(MIDI_STACK_CAPACITY + 1, &self))?;
                }
                Ok(out)
            }
        }

        d.deserialize_seq(Events)
    }
}

/// An editor size as a [`dpi::Size`], in the encoding `dpi`'s `serde` feature gives it.
///
/// A logical extent is an `f64`, so on decode one that is not finite or is
/// negative is refused, as `Normalized::new` refuses its own: a peer that sends
/// one is broken, and the frame fails rather than carrying a size made up for
/// it. A physical size is any pair of `u32`. Used as
/// `#[serde(with = "wire::size")]`, and [`size_result`] and [`size_opt_result`]
/// for the answers that carry one.
pub mod size {
    use dpi::Size;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    /// A [`Size`] that decodes only if it is a size.
    #[derive(Deserialize)]
    #[serde(try_from = "Size")]
    pub(super) struct Checked(pub(super) Size);

    impl TryFrom<Size> for Checked {
        type Error = String;

        fn try_from(size: Size) -> Result<Self, String> {
            match size {
                Size::Logical(l)
                    if ![l.width, l.height]
                        .iter()
                        .all(|e| e.is_finite() && *e >= 0.0) =>
                {
                    Err(format!(
                        "a logical size needs finite, non-negative extents, got {}x{}",
                        l.width, l.height
                    ))
                }
                _ => Ok(Self(size)),
            }
        }
    }

    /// Encodes the size as `dpi` does.
    pub fn serialize<S: Serializer>(value: &Size, s: S) -> Result<S::Ok, S::Error> {
        value.serialize(s)
    }

    /// Decodes a size, refusing a logical one that is not finite and non-negative.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Size, D::Error> {
        Ok(Checked::deserialize(d)?.0)
    }
}

/// A `Result<Size, EditorError>` carried as [`size`] does.
pub mod size_result {
    use dpi::Size;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tutti_plugin_types::EditorError;

    use super::size::Checked;

    /// Encodes the result as it is.
    pub fn serialize<S: Serializer>(
        value: &Result<Size, EditorError>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        value.serialize(s)
    }

    /// Decodes the result, refusing a bad size in `Ok`.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Result<Size, EditorError>, D::Error> {
        Ok(Result::<Checked, EditorError>::deserialize(d)?.map(|c| c.0))
    }
}

/// A `Result<Option<Size>, EditorError>` carried as [`size`] does.
pub mod size_opt_result {
    use dpi::Size;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tutti_plugin_types::EditorError;

    use super::size::Checked;

    /// Encodes the result as it is.
    pub fn serialize<S: Serializer>(
        value: &Result<Option<Size>, EditorError>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        value.serialize(s)
    }

    /// Decodes the result, refusing a bad size in `Ok(Some(_))`.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Result<Option<Size>, EditorError>, D::Error> {
        Ok(Result::<Option<Checked>, EditorError>::deserialize(d)?.map(|o| o.map(|c| c.0)))
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use tutti_plugin_types::{EditorError, MIDI_STACK_CAPACITY, MidiEventVec, WireMidi};
    use tutti_ump::Ump;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Holder {
        #[serde(with = "super::midi")]
        events: MidiEventVec,
    }

    fn ev(i: u32) -> WireMidi {
        WireMidi {
            offset: i,
            ump: Ump([0x2090_3C40, 0, 0, 0]),
        }
    }

    /// **A list past the cap is refused, not truncated.** The bytes of a list
    /// one event over the cap are built by hand (a length and that many
    /// events), since no `MidiEventVec` can hold them.
    #[test]
    fn a_list_past_the_cap_is_rejected() {
        let n = MIDI_STACK_CAPACITY as u64 + 1;
        // The length is a varint, as postcard writes a sequence's.
        let mut bytes = postcard::to_allocvec(&n).unwrap();
        for i in 0..n as u32 {
            bytes.extend(postcard::to_allocvec(&(i, [0x2090_3C40u32, 0, 0, 0])).unwrap());
        }
        assert!(postcard::from_bytes::<Holder>(&bytes).is_err());

        let full: MidiEventVec = (0..MIDI_STACK_CAPACITY as u32).map(ev).collect();
        let at_cap = Holder { events: full };
        let back: Holder = postcard::from_bytes(&postcard::to_allocvec(&at_cap).unwrap()).unwrap();
        assert_eq!(back, at_cap);
    }

    /// **The typed parameter and rate fields encode as bare numbers**: a `u32`
    /// id, an `f64` value (the width a `Normalized` holds, not an `f32`), an
    /// `f64` rate, an `Option<f64>` as a tag byte and the value. The expected bytes are
    /// written out from postcard's rules (an integer or a variant tag is a
    /// varint, seven bits a byte with the low group first; a float is its
    /// little-endian bytes), so a layout change fails here instead of as a
    /// desync between two binaries built apart.
    #[test]
    fn typed_parameter_and_rate_fields_keep_their_wire_widths() {
        use crate::{ControlReply, ControlRequest, News};
        use tutti_plugin_types::ParamEdit;
        use tutti_units::Normalized;
        use tutti_units::ParamId;
        use tutti_units::SampleRate;

        let text = ControlRequest::GetParameterText {
            param_id: ParamId::new(0x0102_0304),
            value: Normalized::of(0.25),
        };
        // variant 13, id 0x0102_0304 as a varint, 0.25f64
        assert_eq!(
            postcard::to_allocvec(&text).unwrap(),
            [13, 0x84, 0x86, 0x88, 0x08, 0, 0, 0, 0, 0, 0, 208, 63]
        );

        let rate = ControlRequest::SetSampleRate {
            rate: SampleRate::SR_48K,
        };
        // variant 4, 48000.0f64
        assert_eq!(
            postcard::to_allocvec(&rate).unwrap(),
            [4, 0, 0, 0, 0, 0, 112, 231, 64]
        );

        let set_reply = ControlReply::SampleRateSet {
            rate: SampleRate::SR_48K,
            refusal: None,
        };
        // variant 3, 48000.0f64, no error
        assert_eq!(
            postcard::to_allocvec(&set_reply).unwrap(),
            [3, 0, 0, 0, 0, 0, 112, 231, 64, 0]
        );

        let some = ControlReply::ParameterValue {
            value: Some(Normalized::of(0.25)),
        };
        // variant 1, Some, 0.25f64
        assert_eq!(
            postcard::to_allocvec(&some).unwrap(),
            [1, 1, 0, 0, 0, 0, 0, 0, 208, 63]
        );
        let none = ControlReply::ParameterValue { value: None };
        assert_eq!(postcard::to_allocvec(&none).unwrap(), [1, 0]);

        let changed = News::ParameterChanged {
            edit: ParamEdit::change(ParamId::new(7), Normalized::of(0.5)),
        };
        // variant 0, then the `ParamEdit`: its variant (`Change` is 1), id 7, 0.5f64
        // and the recording flag (true).
        assert_eq!(
            postcard::to_allocvec(&changed).unwrap(),
            [0, 1, 7, 0, 0, 0, 0, 0, 0, 224, 63, 1]
        );
        let back: News = postcard::from_bytes(&[0, 1, 7, 0, 0, 0, 0, 0, 0, 224, 63, 1]).unwrap();
        assert!(matches!(
            back,
            News::ParameterChanged { edit }
                if edit == ParamEdit::change(ParamId::new(7), Normalized::of(0.5))
        ));

        // A gesture's begin and end hold the id alone: no value is on the wire
        // to read, and none is made up.
        let begin = News::ParameterChanged {
            edit: ParamEdit::Begin(ParamId::new(7)),
        };
        assert_eq!(postcard::to_allocvec(&begin).unwrap(), [0, 0, 7]);
        let end = News::ParameterChanged {
            edit: ParamEdit::End(ParamId::new(7)),
        };
        assert_eq!(postcard::to_allocvec(&end).unwrap(), [0, 2, 7]);
    }

    /// **A rate that is no rate fails the message**: a `SampleRate` decodes only
    /// from a finite `f64` above zero, so zero, a negative, NaN and infinity
    /// never reach the server's clock or a plugin.
    #[test]
    fn a_rate_that_is_no_rate_does_not_decode() {
        use crate::ControlRequest;
        for bad in [
            0.0f64,
            -48_000.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            let mut bytes = vec![4u8];
            bytes.extend(bad.to_le_bytes());
            assert!(
                postcard::from_bytes::<ControlRequest>(&bytes).is_err(),
                "{bad} decoded"
            );
        }
        let mut ok = vec![4u8];
        ok.extend(48_000.0f64.to_le_bytes());
        assert!(matches!(
            postcard::from_bytes::<ControlRequest>(&ok),
            Ok(ControlRequest::SetSampleRate { .. })
        ));
    }

    /// **The editor messages are written in the widths the spec's rules give.** A
    /// variant tag is a varint over declaration order (`OpenEditor` is 18 of the
    /// control requests, `EditorOpened` 15 of the replies); a size is a unit tag then two varints (physical) or two `f64`
    /// (logical), a scale an `f64`, a parent an `Option<u64>`.
    #[test]
    fn the_editor_messages_are_written_in_the_specs_widths() {
        use crate::{ControlReply, ControlRequest, News};
        use dpi::{LogicalSize, PhysicalSize, Size};
        use tutti_plugin_types::ContentScale;

        let size = Size::Physical(PhysicalSize::new(640, 480));
        // Tag 0 (physical), then 640 = 0x280 and 480 = 0x1e0, seven bits a byte, low
        // group first.
        let size_bytes = [0x00, 0x80, 0x05, 0xe0, 0x03];
        // Tag 1 (logical), then 1.0 and 2.0 as little-endian `f64`.
        assert_eq!(
            postcard::to_allocvec(&Size::Logical(LogicalSize::new(1.0, 2.0))).unwrap(),
            [1, 0, 0, 0, 0, 0, 0, 0xf0, 0x3f, 0, 0, 0, 0, 0, 0, 0, 0x40]
        );
        let open = ControlRequest::OpenEditor {
            parent: None,
            scale: ContentScale::IDENTITY,
        };
        assert_eq!(
            postcard::to_allocvec(&open).unwrap(),
            [18, 0, 0, 0, 0, 0, 0, 0, 0xf0, 0x3f]
        );
        let embedded = ControlRequest::OpenEditor {
            parent: Some(300),
            scale: ContentScale::IDENTITY,
        };
        assert_eq!(
            postcard::to_allocvec(&embedded).unwrap(),
            [18, 1, 0xac, 0x02, 0, 0, 0, 0, 0, 0, 0xf0, 0x3f]
        );
        assert_eq!(
            postcard::to_allocvec(&ControlRequest::CloseEditor).unwrap(),
            [19]
        );
        let mut resize = vec![20];
        resize.extend(size_bytes);
        assert_eq!(
            postcard::to_allocvec(&ControlRequest::SetEditorSize { size }).unwrap(),
            resize
        );

        let mut opened = vec![15, 0, 1];
        opened.extend(size_bytes);
        assert_eq!(
            postcard::to_allocvec(&ControlReply::EditorOpened {
                result: Ok(Some(size))
            })
            .unwrap(),
            opened
        );
        assert_eq!(
            postcard::to_allocvec(&ControlReply::EditorOpened {
                result: Err(EditorError::NoPlugin)
            })
            .unwrap(),
            [15, 1, 4]
        );
        let mut sized = vec![16, 0];
        sized.extend(size_bytes);
        assert_eq!(
            postcard::to_allocvec(&ControlReply::EditorSized { result: Ok(size) }).unwrap(),
            sized
        );
        assert_eq!(
            postcard::to_allocvec(&ControlReply::EditorSized {
                result: Err(EditorError::FloatingEditor)
            })
            .unwrap(),
            [16, 1, 14]
        );
        assert!(matches!(
            postcard::from_bytes::<ControlReply>(&[16, 1, 14]),
            Ok(ControlReply::EditorSized {
                result: Err(EditorError::FloatingEditor)
            })
        ));
        let mut wants = vec![7];
        wants.extend(size_bytes);
        assert_eq!(
            postcard::to_allocvec(&News::EditorWantsSize { size }).unwrap(),
            wants
        );
        assert_eq!(postcard::to_allocvec(&News::EditorClosed).unwrap(), [8]);
        // The scale is a checked value on the wire: a NaN is refused.
        let mut nan = vec![18, 0];
        nan.extend(f64::NAN.to_le_bytes());
        assert!(postcard::from_bytes::<ControlRequest>(&nan).is_err());
    }

    /// **A logical size that is not finite and non-negative is refused on
    /// decode**, in each message that carries a size; a physical one and a good
    /// logical one decode.
    #[test]
    fn a_bad_logical_size_is_refused_on_decode() {
        use crate::{ControlReply, ControlRequest, News};
        use dpi::{LogicalSize, PhysicalSize, Size};

        let logical = |w: f64, h: f64| Size::Logical(LogicalSize::new(w, h));
        let bad = [
            logical(f64::NAN, 1.0),
            logical(1.0, f64::INFINITY),
            logical(-1.0, 1.0),
            logical(1.0, f64::NEG_INFINITY),
        ];
        let roundtrips = |size: Size| {
            let host = postcard::to_allocvec(&ControlRequest::SetEditorSize { size }).unwrap();
            let wants = postcard::to_allocvec(&News::EditorWantsSize { size }).unwrap();
            let sized =
                postcard::to_allocvec(&ControlReply::EditorSized { result: Ok(size) }).unwrap();
            let opened = postcard::to_allocvec(&ControlReply::EditorOpened {
                result: Ok(Some(size)),
            })
            .unwrap();
            [
                postcard::from_bytes::<ControlRequest>(&host).is_ok(),
                postcard::from_bytes::<News>(&wants).is_ok(),
                postcard::from_bytes::<ControlReply>(&sized).is_ok(),
                postcard::from_bytes::<ControlReply>(&opened).is_ok(),
            ]
        };
        for size in bad {
            assert_eq!(roundtrips(size), [false; 4], "{size:?}");
        }
        for size in [
            logical(0.0, 480.5),
            Size::Physical(PhysicalSize::new(0, u32::MAX)),
        ] {
            assert_eq!(roundtrips(size), [true; 4], "{size:?}");
        }
    }

    /// A wire value in `0..=1` decodes; one outside it, or NaN, fails the
    /// message, in each width and in the optional forms.
    #[test]
    fn a_normalized_outside_the_unit_range_is_refused() {
        use crate::{ControlReply, ControlRequest};
        use tutti_units::Normalized;
        use tutti_units::ParamId;
        let f64_tail = |message: &dyn Fn() -> Vec<u8>, raw: f64| {
            let mut bytes = message();
            let end = bytes.len();
            bytes[end - 8..].copy_from_slice(&raw.to_le_bytes());
            bytes
        };
        let get_text = || {
            postcard::to_allocvec(&ControlRequest::GetParameterText {
                param_id: ParamId::new(1),
                value: Normalized::of(0.5),
            })
            .unwrap()
        };
        let value_of = |raw: f64| {
            let mut bytes = postcard::to_allocvec(&ControlReply::ParameterValue {
                value: Some(Normalized::of(0.5)),
            })
            .unwrap();
            let end = bytes.len();
            bytes[end - 8..].copy_from_slice(&raw.to_le_bytes());
            postcard::from_bytes::<ControlReply>(&bytes)
        };
        let from_text = || {
            postcard::to_allocvec(&ControlReply::ParameterValueFromText {
                value: Some(Normalized::of(0.5)),
            })
            .unwrap()
        };

        assert!(matches!(
            postcard::from_bytes::<ControlRequest>(&f64_tail(&get_text, 0.25)).unwrap(),
            ControlRequest::GetParameterText { value, .. } if value == Normalized::of(0.25)
        ));
        assert!(matches!(
            postcard::from_bytes::<ControlReply>(&f64_tail(&from_text, 0.25)).unwrap(),
            ControlReply::ParameterValueFromText { value } if value == Some(Normalized::of(0.25))
        ));
        assert!(matches!(
            value_of(0.25).unwrap(),
            ControlReply::ParameterValue { value } if value == Some(Normalized::of(0.25))
        ));
        for raw in [7.5, -3.0, f64::NAN, f64::INFINITY] {
            assert!(value_of(raw).is_err(), "optional reply {raw}");
            assert!(
                postcard::from_bytes::<ControlRequest>(&f64_tail(&get_text, raw)).is_err(),
                "f64 {raw}"
            );
            assert!(
                postcard::from_bytes::<ControlReply>(&f64_tail(&from_text, raw)).is_err(),
                "optional f64 {raw}"
            );
        }
    }
}
