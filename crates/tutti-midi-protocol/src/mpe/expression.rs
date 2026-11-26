//! The two expression controls MPE can carry as a bipolar Registered
//! Controller: [`MpeExpression`] (M2-120 sections 4.8 and 4.9, Table 9).

use crate::ControllerSupport;
use tutti_ump::{CCNumber, ControlValue, Rpn, U7, Voice2};

/// One of the two controls an MPE Profile receiver may take as a bipolar
/// Registered Controller in place of its Control Change form (M2-120 Table 9):
/// Channel Pressure, or the Third Dimension of Control (Control Change 74).
///
/// Every value a method of this type takes or returns is a Control Change form
/// value (Channel Pressure's, or Control Change 74's) unless it says bipolar.
///
/// ```
/// use tutti_midi_protocol::{ControllerSupport, MpeExpression};
/// use tutti_ump::{ControlValue, Voice2};
///
/// let full = MpeExpression::Pressure
///     .form(ControllerSupport::Bipolar, ControlValue::MAX)
///     .unwrap();
/// assert_eq!(
///     MpeExpression::to_control_change(full),
///     Voice2::ChannelPressure(ControlValue::new(0xFFFF_FFFE))
/// );
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MpeExpression {
    /// Channel Pressure, or the Registered Controller `0x20/0x20`.
    Pressure,
    /// Control Change 74, or the Registered Controller `0x20/0x21`.
    ThirdDimension,
}

impl MpeExpression {
    /// The Registered Controller that carries the control bipolar (Table 9).
    #[must_use]
    const fn bipolar_rpn(self) -> Rpn {
        let index = match self {
            Self::Pressure => 0x20,
            Self::ThirdDimension => 0x21,
        };
        Rpn::new(U7::of(0x20), U7::of(index))
    }

    /// The control a Registered Controller carries bipolar, if it is one.
    #[must_use]
    fn of_rpn(rpn: Rpn) -> Option<Self> {
        [Self::Pressure, Self::ThirdDimension]
            .into_iter()
            .find(|e| e.bipolar_rpn() == rpn)
    }

    /// The message that carries `value` of the control in the form a receiver
    /// that supports `support` of it takes (M2-120 section 3.7, Table 8), or
    /// `None` when it does not use the control. A receiver of the bipolar form
    /// is sent that form and never the Control Change one (section 4.9).
    #[must_use]
    pub fn form(self, support: ControllerSupport, value: ControlValue) -> Option<Voice2> {
        match support {
            ControllerSupport::Unsupported => None,
            ControllerSupport::ControlChange => Some(self.control_change(value)),
            ControllerSupport::Bipolar => Some(self.bipolar(value)),
        }
    }

    /// The message of the Control Change form: Channel Pressure, or Control
    /// Change 74.
    const fn control_change(self, value: ControlValue) -> Voice2 {
        match self {
            Self::Pressure => Voice2::ChannelPressure(value),
            Self::ThirdDimension => Voice2::ControlChange {
                controller: CCNumber::BRIGHTNESS,
                value,
            },
        }
    }

    /// The message of the bipolar form, carrying what `value` is of the
    /// Control Change form: unsigned, centred at `0x8000_0000` (section 4.9).
    #[must_use]
    fn bipolar(self, value: ControlValue) -> Voice2 {
        let data = match self {
            // A property that is not bipolar is sent as the upper half only.
            Self::Pressure => ControlValue::CENTER.get() + (value.get() >> 1),
            // Control Change 74 is centred at `0x8000_0000` already.
            Self::ThirdDimension => value.get(),
        };
        Voice2::registered(self.bipolar_rpn(), ControlValue::new(data))
    }

    /// The control and its Control Change form value that `voice` carries, in
    /// either form, or `None` for any other message. A bipolar Pressure below
    /// the centre is `0`: Pressure has no lower half.
    #[must_use]
    fn read(voice: Voice2) -> Option<(Self, ControlValue)> {
        match voice {
            Voice2::ChannelPressure(value) => Some((Self::Pressure, value)),
            Voice2::ControlChange {
                controller: CCNumber::BRIGHTNESS,
                value,
            } => Some((Self::ThirdDimension, value)),
            Voice2::Registered { bank, index, value } => {
                let control = Self::of_rpn(Rpn::new(bank, index))?;
                let data = value.get();
                let value = match control {
                    Self::Pressure => {
                        let upper = u64::from(data.saturating_sub(ControlValue::CENTER.get()));
                        u32::try_from(upper * 2).unwrap_or(u32::MAX)
                    }
                    Self::ThirdDimension => data,
                };
                Some((control, ControlValue::new(value)))
            }
            _ => None,
        }
    }

    /// `voice` as the Control Change form of the control it carries bipolar,
    /// or as it is when it carries none.
    #[must_use]
    pub fn to_control_change(voice: Voice2) -> Voice2 {
        match voice {
            Voice2::Registered { .. } => {
                Self::read(voice).map_or(voice, |(control, value)| control.control_change(value))
            }
            _ => voice,
        }
    }
}

#[cfg(test)]
mod tests;
