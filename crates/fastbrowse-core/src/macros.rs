//! `StrEnum` and `Literal` as Rust types: written and read as their values, ordered as Python orders the strings.

/// A `StrEnum`. `sorted()` on one compares the values as strings, so `Ord` does too, and a `BTreeSet` of one
/// iterates the way `sorted(control.operations)` does. `doc` is the class docstring pydantic puts in the schema, for
/// the few classes that have one.
macro_rules! str_enum {
    (
        $(#[$meta:meta])*
        $(doc: $schema_doc:literal;)?
        pub enum $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident = $value:literal ),* $(,)?
        }
    ) => {
        str_enum_base! {
            $(#[$meta])*
            pub enum $name { $( $(#[$variant_meta])* $variant = $value ),* }
        }

        impl $crate::schema::Schema for $name {
            fn ty() -> $crate::schema::Ty {
                #[allow(unused_mut, unused_assignments)]
                let mut doc: Option<&'static str> = None;
                $(doc = Some($schema_doc);)?
                $crate::schema::Ty::Enum(std::sync::Arc::new($crate::schema::EnumTy {
                    name: stringify!($name),
                    doc,
                    values: vec![$(serde_json::Value::from($value)),*],
                }))
            }
        }
    };
}

/// A `Literal[...]` of strings: the Rust shape of a `StrEnum`, written inline in a schema instead of under `$defs`.
macro_rules! literal_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident = $value:literal ),* $(,)?
        }
    ) => {
        str_enum_base! {
            $(#[$meta])*
            pub enum $name { $( $(#[$variant_meta])* $variant = $value ),* }
        }

        impl $crate::schema::Schema for $name {
            fn ty() -> $crate::schema::Ty {
                $crate::schema::Ty::Literal(vec![$(serde_json::Value::from($value)),*])
            }
        }
    };
}

macro_rules! str_enum_base {
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
    };
}

/// A `Literal["x"] = "x"` field: always written, and read only as its one value.
macro_rules! literal_tag {
    ($(#[$meta:meta])* $name:ident = $value:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
        pub struct $name;

        impl $name {
            pub const VALUE: &'static str = $value;
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str($value)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                if value == $value {
                    Ok($name)
                } else {
                    Err(<D::Error as serde::de::Error>::custom(concat!("Input should be '", $value, "'")))
                }
            }
        }

        impl $crate::schema::Schema for $name {
            fn ty() -> $crate::schema::Ty {
                $crate::schema::Ty::literal($value)
            }
        }
    };
}
