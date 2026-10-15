//! The system clipboard: invites go out through it and are noticed on it.

/// The clipboard's text, or "" when it holds none or another program has it open (often a chat client).
pub fn read_text() -> String {
    arboard::Clipboard::new().and_then(|mut c| c.get_text()).unwrap_or_default()
}

pub fn write_text(text: &str) -> Result<(), String> {
    arboard::Clipboard::new().and_then(|mut c| c.set_text(text.to_string())).map_err(|e| e.to_string())
}

/// The system's clipboard change counter, so the text is only read when something new was copied.
/// `None` where watching the clipboard is not done (macOS tells the user about every read).
pub fn sequence_number() -> Option<u64> {
    #[cfg(windows)]
    {
        // SAFETY: takes no arguments and only reads a counter.
        Some(u64::from(unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }))
    }
    #[cfg(not(windows))]
    {
        None
    }
}
