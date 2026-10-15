//! Reading `sender.json` / `receiver.json`: the files the C# apps wrote with DataContractJsonSerializer.
//!
//! What that serializer wrote is plain JSON (numbers for enums, `null` for a string nobody set), and all of it
//! is read. What it additionally *accepted* from a hand-edited file is not reproduced: a number in quotes
//! (`"fps":"60"`), an integer written as `60.0` or `6e1`, `1` for `true`, a comma before the closing brace, a
//! missing closing brace. Such a file counts as unreadable here, and the app starts from the defaults.

use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

/// The settings in `text`, or `None` when it is not a JSON object or has a member of the wrong type.
pub(crate) fn from_json<T: DeserializeOwned>(text: &str) -> Option<T> {
    // Notepad's byte order mark is not part of the JSON.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    // A derived Deserialize would also take an array as "the fields in order". A settings file is an object.
    if !text.trim_start().starts_with('{') {
        return None;
    }
    serde_json::from_str(text).ok()
}

/// The settings in `path`, or `None` when the file is missing, unreadable or not what [`from_json`] reads (the
/// caller then starts from the defaults, as the C# apps did).
pub(crate) fn load<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    from_json(&String::from_utf8_lossy(&bytes))
}

/// `null` reads as the type's default. DataContractJsonSerializer wrote a string nobody had set as `null`, and
/// the C# `Normalize()` turned it back into a usable value; a strict reader would throw the whole file away.
pub(crate) fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
