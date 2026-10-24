//! `StrEnum` as a Rust type, as fastbrowse-core writes its own: core keeps its macro crate-private, so the closed
//! sets this crate declares use this copy until core exports it.

/// A `StrEnum`, written and read as its value and ordered as Python orders the strings.
macro_rules! str_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident = $value:literal ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $( $(#[$variant_meta])* $variant ),*
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),*];

            pub fn as_str(&self) -> &'static str {
                match self {
                    $($name::$variant => $value),*
                }
            }

            pub fn parse(value: &str) -> Option<$name> {
                match value {
                    $($value => Some($name::$variant),)*
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl PartialOrd for $name {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        impl Ord for $name {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                self.as_str().cmp(other.as_str())
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                $name::parse(&value).ok_or_else(|| {
                    let names: Vec<String> = $name::ALL.iter().map(|v| format!("'{}'", v.as_str())).collect();
                    let expected = match names.split_last() {
                        Some((last, [])) => last.clone(),
                        Some((last, rest)) => format!("{} or {}", rest.join(", "), last),
                        None => String::new(),
                    };
                    <D::Error as serde::de::Error>::custom(format!("Input should be {expected}"))
                })
            }
        }

        impl fastbrowse_core::schema::Schema for $name {
            fn ty() -> fastbrowse_core::schema::Ty {
                fastbrowse_core::schema::Ty::Enum(std::sync::Arc::new(fastbrowse_core::schema::EnumTy {
                    name: stringify!($name),
                    doc: None,
                    values: vec![$(serde_json::Value::from($value)),*],
                }))
            }
        }
    };
}
