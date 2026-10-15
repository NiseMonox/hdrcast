//! The one window: no system title bar, the system's backdrop material behind the sidebar where there is one.

use tauri::{AppHandle, Manager, Theme, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

pub const LABEL: &str = "main";

/// Creates the window. The second value says whether a backdrop material (Mica / vibrancy) is behind it, in which
/// case the page leaves its sidebar translucent.
pub fn create(app: &AppHandle) -> tauri::Result<(WebviewWindow, bool)> {
    let builder = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
        .title("HdrCast")
        .inner_size(1040.0, 720.0)
        .min_inner_size(880.0, 600.0)
        .center()
        // shown by the page once it has drawn itself, so there is no white flash
        .visible(false);
    // A separate profile (HDRCAST_DATA_ROOT) keeps its web view's data to itself too, so two profiles can run
    // side by side.
    let builder = match std::env::var_os(hdrcast_core::common::app_paths::ROOT_OVERRIDE_ENV) {
        Some(root) if !root.is_empty() => builder.data_directory(hdrcast_core::common::app_paths::config_dir().join("webview")),
        _ => builder,
    };

    #[cfg(windows)]
    let (builder, backdrop) = {
        use tauri::window::{Effect, EffectsBuilder};
        // Mica exists from Windows 11 on. Without it a transparent window would show the desktop through the
        // sidebar, so older systems get an opaque one.
        let mica = windows_version::OsVersion::current().build >= 22000;
        let builder = builder.decorations(false).shadow(true);
        // Debug builds only: HDRCAST_DEVTOOLS_PORT=9333 lets a script drive the page over the DevTools protocol.
        #[cfg(debug_assertions)]
        let builder = match std::env::var("HDRCAST_DEVTOOLS_PORT") {
            Ok(port) if port.parse::<u16>().is_ok() => builder.additional_browser_args(&format!(
                "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --remote-debugging-port={port}"
            )),
            _ => builder,
        };
        if mica {
            (builder.transparent(true).effects(EffectsBuilder::new().effect(Effect::Mica).build()), true)
        } else {
            (builder, false)
        }
    };

    #[cfg(target_os = "macos")]
    let (builder, backdrop) = {
        use tauri::window::{Effect, EffectState, EffectsBuilder};
        use tauri::TitleBarStyle;
        (
            builder
                .title_bar_style(TitleBarStyle::Overlay)
                .hidden_title(true)
                .transparent(true)
                .effects(EffectsBuilder::new().effect(Effect::Sidebar).state(EffectState::FollowsWindowActiveState).build()),
            true,
        )
    };

    #[cfg(not(any(windows, target_os = "macos")))]
    let backdrop = false;

    Ok((builder.build()?, backdrop))
}

/// Follows the appearance chosen in Settings ("auto" = the system's), so the backdrop material and the page agree.
pub fn apply_theme(app: &AppHandle, theme: &str) {
    let Some(window) = app.get_webview_window(LABEL) else { return };
    let theme = match theme {
        "light" => Some(Theme::Light),
        "dark" => Some(Theme::Dark),
        _ => None,
    };
    let _ = window.set_theme(theme);
}
