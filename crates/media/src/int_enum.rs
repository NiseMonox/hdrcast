//! Enums that the settings files store as numbers.

/// Declares an enum with explicit numbers that serialises as its number, which is what the C# apps'
/// DataContractJsonSerializer wrote into `sender.json` / `receiver.json`.
///
/// A number that names no variant (a hand-edited file, or an encoder that was removed) reads as the `fallback`
/// instead of failing: the C# apps loaded such a file and repaired the one value, and losing every other
/// setting over it would be worse.
macro_rules! int_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident (fallback = $fallback:expr) {
            $( $(#[$variant_meta:meta])* $variant:ident = $value:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(i32)]
        $vis enum $name {
            $( $(#[$variant_meta])* $variant = $value ),+
        }

        impl $name {
            /// Every value, in the order of their numbers.
            pub const ALL: &'static [$name] = &[ $( $name::$variant ),+ ];

            /// The number the settings file stores.
            pub const fn number(self) -> i32 {
                self as i32
            }

            /// The value a stored number names, if any.
            pub fn from_number(number: i64) -> Option<$name> {
                match number {
                    $( $value => Some($name::$variant), )+
                    _ => None,
                }
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_i32(self.number())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let number = <i64 as ::serde::Deserialize>::deserialize(deserializer)?;
                Ok($name::from_number(number).unwrap_or_else(|| $fallback))
            }
        }
    };
}

#[cfg(test)]
mod tests {
    int_enum! {
        /// A test enum with a gap in its numbers.
        enum Sample (fallback = Sample::Two) {
            Zero = 0,
            Two = 2,
            Five = 5,
        }
    }

    #[test]
    fn the_number_is_what_is_written_and_read() {
        assert_eq!(serde_json::to_string(&Sample::Five).unwrap(), "5");
        assert_eq!(serde_json::from_str::<Sample>("0").unwrap(), Sample::Zero);
        assert_eq!(Sample::Two.number(), 2);
        assert_eq!(Sample::from_number(5), Some(Sample::Five));
        assert_eq!(Sample::ALL, [Sample::Zero, Sample::Two, Sample::Five]);
    }

    #[test]
    fn a_number_without_a_variant_reads_as_the_fallback() {
        for text in ["1", "3", "-1", "99", "4294967296"] {
            assert_eq!(
                serde_json::from_str::<Sample>(text).unwrap(),
                Sample::Two,
                "{text}"
            );
        }
        assert_eq!(Sample::from_number(1), None);
    }

    #[test]
    fn anything_but_an_integer_is_an_error() {
        for text in ["\"Two\"", "2.0", "null", "true", "[2]"] {
            assert!(serde_json::from_str::<Sample>(text).is_err(), "{text}");
        }
    }
}
