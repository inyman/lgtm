//! Colors follow the active Omarchy theme
//! (`~/.local/state/omarchy/current/theme/colors.toml`), falling back to
//! Catppuccin Mocha. `watch_omarchy_theme` recolors live on theme switches.
//! The accessors keep Catppuccin role names (base, mantle, surface0, …).

use gpui::{rgb, App, FontStyle, HighlightStyle, Hsla, Rgba};
use gpui_component::{Theme, ThemeMode};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;
use syntax::Token;

#[derive(Clone, Copy, PartialEq)]
struct Palette {
    dark: bool,
    base: u32,
    mantle: u32,
    crust: u32,
    surface0: u32,
    text: u32,
    subtext: u32,
    overlay0: u32,
    muted: u32,
    selection: u32,
    accent: u32,
    red: u32,
    green: u32,
    yellow: u32,
    orange: u32,
    blue: u32,
    magenta: u32,
    cyan: u32,
    bright_red: u32,
    bright_blue: u32,
}

const MOCHA: Palette = Palette {
    dark: true,
    base: 0x1e1e2e,
    mantle: 0x181825,
    crust: 0x11111b,
    surface0: 0x313244,
    text: 0xcdd6f4,
    subtext: 0xa6adc8,
    overlay0: 0x6c7086,
    muted: 0x585b70,
    selection: 0x45475a,
    accent: 0x89b4fa,
    red: 0xf38ba8,
    green: 0xa6e3a1,
    yellow: 0xf9e2af,
    orange: 0xfab387,
    blue: 0x89b4fa,
    magenta: 0xcba6f7,
    cyan: 0x89dceb,
    bright_red: 0xeba0ac,
    bright_blue: 0xb4befe,
};

static PALETTE: RwLock<Palette> = RwLock::new(MOCHA);

fn pal() -> Palette {
    *PALETTE.read().unwrap()
}

fn omarchy_theme_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/state/omarchy/current"))
}

/// Parse Omarchy's `colors.toml` (flat `key = "#rrggbb"` lines). Keys a
/// theme omits keep their Catppuccin fallback.
fn load_palette() -> Palette {
    let Some(text) = omarchy_theme_dir()
        .and_then(|dir| std::fs::read_to_string(dir.join("theme/colors.toml")).ok())
    else {
        return MOCHA;
    };
    let mut kv = HashMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        kv.insert(key.trim(), value.trim().trim_matches('"').to_string());
    }
    let color = |key: &str, fallback: u32| {
        kv.get(key)
            .and_then(|v| u32::from_str_radix(v.trim_start_matches('#'), 16).ok())
            .unwrap_or(fallback)
    };
    let m = MOCHA;
    let yellow = color("yellow", m.yellow);
    Palette {
        dark: kv.get("mode").is_none_or(|mode| mode != "light"),
        base: color("background", m.base),
        mantle: color("dark_background", m.mantle),
        crust: color("darker_background", m.crust),
        surface0: color("lighter_background", m.surface0),
        text: color("foreground", m.text),
        subtext: color("light_foreground", m.subtext),
        overlay0: color("dark_foreground", m.overlay0),
        muted: color("muted", m.muted),
        selection: color("selection", m.selection),
        accent: color("accent", m.accent),
        red: color("red", m.red),
        green: color("green", m.green),
        yellow,
        orange: color("orange", color("bright_yellow", yellow)),
        blue: color("blue", m.blue),
        magenta: color("magenta", m.magenta),
        cyan: color("cyan", m.cyan),
        bright_red: color("bright_red", m.bright_red),
        bright_blue: color("bright_blue", m.bright_blue),
    }
}

/// `rgb` with an alpha byte.
fn tint(color: u32, alpha: u8) -> Rgba {
    gpui::rgba((color << 8) | alpha as u32)
}

pub fn base() -> Rgba {
    rgb(pal().base)
}
pub fn mantle() -> Rgba {
    rgb(pal().mantle)
}
pub fn crust() -> Rgba {
    rgb(pal().crust)
}
pub fn surface0() -> Rgba {
    rgb(pal().surface0)
}
pub fn text() -> Rgba {
    rgb(pal().text)
}
pub fn subtext() -> Rgba {
    rgb(pal().subtext)
}
pub fn overlay0() -> Rgba {
    rgb(pal().overlay0)
}
pub fn green() -> Rgba {
    rgb(pal().green)
}
pub fn red() -> Rgba {
    rgb(pal().red)
}
/// The theme's accent (cursor, current hunk, focus rings).
pub fn blue() -> Rgba {
    rgb(pal().accent)
}
pub fn mauve() -> Rgba {
    rgb(pal().magenta)
}
pub fn peach() -> Rgba {
    rgb(pal().orange)
}

/// Load the active Omarchy theme into the palette and the UI toolkit.
/// Call after `gpui_component::init`.
pub fn init(cx: &mut App) {
    *PALETTE.write().unwrap() = load_palette();
    apply_ui_theme(cx);
}

/// Recolor live when the Omarchy theme changes (`omarchy-theme-set` writes
/// `current/theme.name` after swapping the theme files in).
pub fn watch_omarchy_theme(cx: &mut App) {
    use notify::Watcher;
    let Some(dir) = omarchy_theme_dir().filter(|dir| dir.is_dir()) else {
        return;
    };
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let Ok(mut watcher) = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok_and(|e| !matches!(e.kind, notify::EventKind::Access(_))) {
            tx.send(()).ok();
        }
    }) else {
        return;
    };
    if watcher
        .watch(&dir, notify::RecursiveMode::NonRecursive)
        .is_err()
    {
        return;
    }
    cx.spawn(async move |cx| {
        let _watcher = watcher;
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            if rx.try_iter().count() == 0 {
                continue;
            }
            let next = load_palette();
            if next == pal() {
                continue;
            }
            *PALETTE.write().unwrap() = next;
            if cx
                .update(|cx| {
                    apply_ui_theme(cx);
                    cx.refresh_windows();
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}

/// Override gpui-component's theme (default shadcn palette) with ours.
fn apply_ui_theme(cx: &mut App) {
    let mode = if pal().dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    Theme::change(mode, None, cx);

    let base: Hsla = base().into();
    let mantle: Hsla = mantle().into();
    let crust: Hsla = crust().into();
    let surface0: Hsla = surface0().into();
    let text: Hsla = text().into();
    let overlay0: Hsla = overlay0().into();
    let green: Hsla = green().into();
    let red: Hsla = red().into();
    let blue: Hsla = blue().into();
    let peach: Hsla = peach().into();

    let theme = Theme::global_mut(cx);
    theme.background = base;
    theme.foreground = text;
    theme.muted = surface0;
    theme.muted_foreground = overlay0;
    theme.border = surface0;
    theme.input = surface0;
    theme.ring = blue;
    theme.primary = blue;
    theme.primary_hover = blue.opacity(0.9);
    theme.primary_active = blue.opacity(0.8);
    theme.primary_foreground = crust;
    theme.secondary = surface0;
    theme.secondary_hover = surface0.opacity(0.8);
    theme.secondary_active = surface0.opacity(0.6);
    theme.secondary_foreground = text;
    theme.accent = surface0;
    theme.accent_foreground = text;
    theme.danger = red;
    theme.danger_hover = red.opacity(0.9);
    theme.danger_active = red.opacity(0.8);
    theme.danger_foreground = crust;
    theme.success = green;
    theme.success_hover = green.opacity(0.9);
    theme.success_active = green.opacity(0.8);
    theme.success_foreground = crust;
    theme.warning = peach;
    theme.warning_hover = peach.opacity(0.9);
    theme.warning_active = peach.opacity(0.8);
    theme.warning_foreground = crust;
    theme.info = blue;
    theme.info_hover = blue.opacity(0.9);
    theme.info_active = blue.opacity(0.8);
    theme.info_foreground = crust;
    theme.link = blue;
    theme.link_hover = blue.opacity(0.9);
    theme.link_active = blue.opacity(0.8);
    theme.popover = mantle;
    theme.popover_foreground = text;
    theme.title_bar = mantle;
    theme.title_bar_border = surface0;
    theme.sidebar = mantle;
    theme.sidebar_foreground = text;
    theme.sidebar_border = surface0;
    theme.caret = text;
    theme.selection = Hsla::from(rgb(pal().selection));
    theme.scrollbar = crust.opacity(0.6);
    theme.scrollbar_thumb = overlay0.opacity(0.5);
    theme.scrollbar_thumb_hover = overlay0;
    theme.window_border = surface0;
}

/// Syntax colors for tree-sitter tokens from the theme palette (on
/// Catppuccin this reproduces its usual scheme). Variable and Embedded map to
/// the plain text color (spans for them are not emitted; belt-and-braces).
pub fn token_style(token: Token) -> HighlightStyle {
    let p = pal();
    let (color, italic) = match token {
        Token::Keyword => (p.magenta, false),
        Token::Function => (p.blue, false),
        Token::Type => (p.yellow, false),
        Token::String => (p.green, false),
        Token::Number | Token::Constant => (p.orange, false),
        Token::Comment => (p.muted, true),
        Token::Property => (p.bright_blue, false),
        Token::Variable | Token::Embedded => (p.text, false),
        Token::Parameter => (p.bright_red, true),
        Token::Operator => (p.cyan, false),
        Token::Punctuation => (p.overlay0, false),
        Token::Attribute | Token::Label => (p.yellow, false),
        Token::Namespace => (p.orange, true),
    };
    HighlightStyle {
        color: Some(rgb(color).into()),
        font_style: italic.then_some(FontStyle::Italic),
        ..Default::default()
    }
}

/// Split view: background for the absent side of a one-sided row — darker
/// than any content row so it clearly reads as "nothing here".
pub fn void_cell_bg() -> Rgba {
    tint(pal().crust, 0x99)
}

/// Text selection in the diff pane — the theme's selection color, as also
/// given to gpui-component in `apply_ui_theme`.
pub fn selection_bg() -> Rgba {
    rgb(pal().selection)
}

/// Low-alpha tints: syntax/text must stay readable on top. Never opaque.
/// Light themes get a bit more alpha so the tint still shows on white.
fn tint_alpha(dark: u8, light: u8) -> u8 {
    if pal().dark {
        dark
    } else {
        light
    }
}
pub fn added_row_bg() -> Rgba {
    tint(pal().green, tint_alpha(0x20, 0x30))
}
pub fn removed_row_bg() -> Rgba {
    tint(pal().red, tint_alpha(0x20, 0x30))
}
pub fn added_word_bg() -> Rgba {
    tint(pal().green, tint_alpha(0x48, 0x60))
}
pub fn removed_word_bg() -> Rgba {
    tint(pal().red, tint_alpha(0x48, 0x60))
}
