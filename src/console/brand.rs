//! What the console looks like: the name it goes by, its marks, and its
//! colours.
//!
//! The console serves OpenSearch Dashboards' own application, byte for byte,
//! and that application calls itself OpenSearch Dashboards and draws itself
//! in OpenSearch's blue. Neither is a property of the code: the name, the
//! three images and the favicon come out of the `branding` block in the
//! metadata the server injects, which is a contract the front end already
//! reads, and the colours are a stylesheet. So this is where VeloSearch's own
//! are, and the application is still untouched.
//!
//! The marks are compiled in rather than read from a directory. A console is
//! one binary pointed at a distribution; an image it had to be given
//! separately would be an image a deployment could forget, and a header with
//! a broken image in it is worse than one with somebody else's logo.
//!
//! `VELOSEARCH_CONSOLE_BRANDING=opensearch` turns all of it off and leaves
//! the distribution's own, which is what `tools/console_diff.py` compares
//! against the Node server.

/// The brand's green, as the logo draws it. Bright enough to be the mark and
/// too bright to be text: 2.3 against white.
pub const ACCENT: &str = "#00C566";

/// The same green taken down until text in it can be read on white -- 5.8
/// against it, past what WCAG asks for body text. This is what a link, a
/// filled button and a selected tab are.
pub const PRIMARY: &str = "#00753C";

/// One step further down, for the state a pointer is over.
pub const PRIMARY_DARK: &str = "#005C2F";

/// The deep green of the wordmark: headers, and the surfaces a dark theme
/// draws behind them.
pub const DEEP: &str = "#004628";

/// The palest green in the logo, for a tint behind a selected row.
pub const TINT: &str = "#E1F4E9";

/// What the console calls itself.
pub const TITLE: &str = "VeloSearch";

/// The marks, at the paths the branding block names them at.
///
/// `logo` is the wordmark, which the expanded header shows; `mark` is the V
/// alone, for the collapsed navigation and the loading screen; `favicon` is
/// the mark again, at the size a tab draws.
const ASSETS: &[(&str, &[u8], &str)] = &[
    ("logo.png", include_bytes!("../../console/brand/logo.png"), "image/png"),
    ("logo-dark.png", include_bytes!("../../console/brand/logo-dark.png"), "image/png"),
    ("mark.png", include_bytes!("../../console/brand/mark.png"), "image/png"),
    ("favicon.png", include_bytes!("../../console/brand/favicon.png"), "image/png"),
];

/// The path everything here is served under, inside the console's own `/ui`.
pub const FOLDER: &str = "/ui/velosearch";

/// One of the brand's files, by the name a URL gives it.
pub fn asset(name: &str) -> Option<(&'static [u8], &'static str)> {
    ASSETS.iter().find(|(at, _, _)| *at == name).map(|(_, bytes, kind)| (*bytes, *kind))
}

/// Whether a console draws itself as VeloSearch.
///
/// On unless an operator says otherwise, because a console that is part of
/// this project and says somebody else's name is a console nobody can tell
/// apart from the thing it replaces.
pub fn wanted() -> bool {
    !matches!(
        std::env::var("VELOSEARCH_CONSOLE_BRANDING").as_deref(),
        Ok("opensearch") | Ok("off") | Ok("none")
    )
}

/// The stylesheet that paints what the theme does not derive.
///
/// [`recoloured`] moves the theme itself, which is where the application gets
/// almost all of its colour. What is left over is the handful of surfaces a
/// distribution paints outside the theme -- the bar across the top, the page
/// drawn before the application has booted -- and the few custom properties
/// the built theme really does declare (`--euiColor*`; there are no `--oui*`
/// ones in it, which is why none are set here). Everything is additive and
/// this sheet is last, so the worst a mismatch can do is leave a surface the
/// colour it already was.
pub fn stylesheet() -> String {
    format!(
        ":root {{\
           --velo-accent: {ACCENT};\
           --velo-primary: {PRIMARY};\
           --velo-primary-dark: {PRIMARY_DARK};\
           --velo-deep: {DEEP};\
           --velo-tint: {TINT};\
           --euiColorPrimary: {PRIMARY};\
           --euiColorPrimaryText: {PRIMARY};\
           --euiColorAccent: {ACCENT};\
           --euiColorAccentText: {PRIMARY};\
           --euiLinkColor: {PRIMARY};\
           --euiColorFocusBackground: {TINT};\
         }}\
         /* a dark page reads the bright green well and the dark one not at \
            all, so the two swap there */\
         .theme-dark, [data-theme=\"dark\"], .ouiTheme--dark, .euiTheme--dark {{\
           --euiColorPrimary: {ACCENT};\
           --euiColorPrimaryText: {ACCENT};\
           --euiLinkColor: {ACCENT};\
           --euiColorFocusBackground: {DEEP};\
         }}\
         /* the bar across the top, which the theme paints rather than \
            derives */\
         .headerGlobalNav .ouiHeader, .headerGlobalNav .euiHeader,\
         .ouiHeader--dark, .euiHeader--dark {{\
           background-color: {DEEP};\
           border-bottom-color: {PRIMARY_DARK};\
         }}\
         .ouiHeaderLogo__text, .euiHeaderLogo__text {{ color: {TINT}; }}\
         /* what a caller presses, and what tells them where they are */\
         .ouiButton--primary.ouiButton--fill, .euiButton--primary.euiButton--fill,\
         .ouiButtonIcon--primary.ouiButtonIcon--fill,\
         .euiButtonIcon--primary.euiButtonIcon--fill {{\
           background-color: {PRIMARY};\
           border-color: {PRIMARY};\
           color: #FFFFFF;\
         }}\
         .ouiButton--primary.ouiButton--fill:hover,\
         .euiButton--primary.euiButton--fill:hover {{\
           background-color: {PRIMARY_DARK};\
           border-color: {PRIMARY_DARK};\
         }}\
         .ouiTab-isSelected, .euiTab-isSelected {{\
           color: {PRIMARY};\
           box-shadow: inset 0 -2px 0 {ACCENT};\
         }}\
         .ouiLink, .euiLink {{ color: {PRIMARY}; }}\
         .ouiProgress__bar, .euiProgress__bar {{ background-color: {ACCENT}; }}\
         .ouiLoadingSpinner, .euiLoadingSpinner {{\
           border-color: {TINT} {TINT} {ACCENT} {ACCENT};\
         }}\
         /* the page that is drawn before the application has booted */\
         .osdWelcomeView {{ color: {DEEP}; }}\
         .osdProgress {{ background-color: {TINT}; }}\
         .osdProgress:before {{ background-color: {ACCENT}; }}\n"
    )
}

/// The hue the brand's greens are drawn at, in degrees. Every green in this
/// file sits on it, and so does every colour [`recoloured`] makes.
const BRAND_HUE: f64 = 151.0;

/// The band of hues a theme's primary blue lives in, in degrees.
///
/// Measured, not guessed. Across the six stylesheets a distribution ships,
/// every shade of the primary -- the blue itself, its hover, its focus ring,
/// its tints and the two dark themes' lighter versions -- falls between 197
/// and 210. A wider band starts catching colours that are not the primary at
/// all: the blue-greys that body text and panel borders are drawn in, and the
/// navy a shadow is made of.
const PRIMARY_HUES: std::ops::RangeInclusive<f64> = 195.0..=212.0;

/// How saturated a colour has to be before it counts as the primary rather
/// than a grey with a cool cast. `#2A3947` -- the colour of body text on the
/// dark themes -- is 0.26, and the palest real primary is 0.56.
const PRIMARY_SATURATION: f64 = 0.55;

/// How light, either side, before a colour is really black or really white.
const PRIMARY_LIGHTNESS: std::ops::RangeInclusive<f64> = 0.12..=0.80;

/// A theme's stylesheet with its blues moved onto the brand's green.
///
/// This is the theme, not a skin over it. Every colour the distribution's
/// stylesheet names is looked at, the ones that are the primary blue are
/// replaced, and the rest -- text, surfaces, borders, the danger red, the
/// warning yellow and the categorical palette a chart draws its series in --
/// are left exactly as they were.
///
/// A replacement keeps the colour's *relative luminance*, which is the one
/// property WCAG contrast is computed from. Hue and saturation move; how
/// bright the colour is does not. So every contrast ratio in the theme --
/// the text on a filled button, the focus ring against the panel behind it,
/// a disabled label against its background -- comes out the same as the
/// people who built the theme measured it. What is a legible pairing in
/// OpenSearch's blue is a legible pairing here.
pub fn recoloured(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let bytes = css.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#'
            && let Some((colour, width)) = hex_at(&bytes[i + 1..])
            && let Some(moved) = moved(colour)
        {
            out.push_str(&format!("#{:02x}{:02x}{:02x}", moved.0, moved.1, moved.2));
            i += 1 + width;
            continue;
        }
        if (bytes[i] == b'r' || bytes[i] == b'R')
            && let Some((colour, width)) = rgb_at(&bytes[i..])
            && let Some(moved) = moved(colour)
        {
            out.push_str(&format!("rgb({}, {}, {}", moved.0, moved.1, moved.2));
            i += width;
            continue;
        }
        let step = css[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&css[i..i + step]);
        i += step;
    }
    out
}

/// The colour a `#…` at the start of these bytes names, and how many bytes it
/// took. Both spellings a stylesheet uses; anything else is not a colour.
fn hex_at(rest: &[u8]) -> Option<((u8, u8, u8), usize)> {
    let digits = rest.iter().take_while(|b| b.is_ascii_hexdigit()).count();
    let read = |a: u8, b: u8| u8::from_str_radix(std::str::from_utf8(&[a, b]).ok()?, 16).ok();
    match digits {
        3 => Some(((read(rest[0], rest[0])?, read(rest[1], rest[1])?, read(rest[2], rest[2])?), 3)),
        6 => Some(((read(rest[0], rest[1])?, read(rest[2], rest[3])?, read(rest[4], rest[5])?), 6)),
        _ => None,
    }
}

/// The colour an `rgb(` or `rgba(` at the start of these bytes opens with,
/// and how many bytes reach the end of its third number. The alpha, and the
/// closing bracket, are left where they are: how see-through a colour is is
/// not part of which colour it is.
fn rgb_at(rest: &[u8]) -> Option<((u8, u8, u8), usize)> {
    let text = std::str::from_utf8(rest).ok()?;
    let lower = text.get(..5)?.to_ascii_lowercase();
    let mut at = if lower.starts_with("rgba(") {
        5
    } else if lower.starts_with("rgb(") {
        4
    } else {
        return None;
    };
    let mut channels = [0u8; 3];
    for (n, channel) in channels.iter_mut().enumerate() {
        while text.as_bytes().get(at) == Some(&b' ') {
            at += 1;
        }
        let start = at;
        while text.as_bytes().get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == start {
            return None;
        }
        *channel = text[start..at].parse().ok()?;
        if n < 2 {
            while text.as_bytes().get(at) == Some(&b' ') {
                at += 1;
            }
            if text.as_bytes().get(at) != Some(&b',') {
                return None;
            }
            at += 1;
        }
    }
    // a percentage or a fraction after the digits means this was not the
    // integer form, and the bytes counted would not be the whole number
    if matches!(text.as_bytes().get(at), Some(b'%') | Some(b'.')) {
        return None;
    }
    Some(((channels[0], channels[1], channels[2]), at))
}

/// Where a colour goes, or [`None`] if it is not the primary blue and so
/// stays where it is.
fn moved(colour: (u8, u8, u8)) -> Option<(u8, u8, u8)> {
    let (hue, saturation, lightness) = hsl(colour);
    if !PRIMARY_HUES.contains(&hue)
        || saturation < PRIMARY_SATURATION
        || !PRIMARY_LIGHTNESS.contains(&lightness)
    {
        return None;
    }
    // the same saturation on the brand's hue, taken up or down until it is
    // exactly as bright as what it replaces
    let wanted = luminance(colour);
    let (mut low, mut high) = (0.0f64, 1.0f64);
    for _ in 0..24 {
        let middle = (low + high) / 2.0;
        if luminance(rgb(BRAND_HUE, saturation, middle)) < wanted {
            low = middle;
        } else {
            high = middle;
        }
    }
    Some(rgb(BRAND_HUE, saturation, (low + high) / 2.0))
}

/// A colour's hue in degrees, saturation and lightness.
fn hsl(colour: (u8, u8, u8)) -> (f64, f64, f64) {
    let (r, g, b) = (colour.0 as f64 / 255.0, colour.1 as f64 / 255.0, colour.2 as f64 / 255.0);
    let (high, low) = (r.max(g).max(b), r.min(g).min(b));
    let lightness = (high + low) / 2.0;
    let span = high - low;
    if span <= f64::EPSILON {
        return (0.0, 0.0, lightness);
    }
    let saturation = span / (1.0 - (2.0 * lightness - 1.0).abs());
    let hue = if high == r {
        60.0 * (((g - b) / span) % 6.0)
    } else if high == g {
        60.0 * ((b - r) / span + 2.0)
    } else {
        60.0 * ((r - g) / span + 4.0)
    };
    (if hue < 0.0 { hue + 360.0 } else { hue }, saturation, lightness)
}

/// The colour a hue, saturation and lightness name.
fn rgb(hue: f64, saturation: f64, lightness: f64) -> (u8, u8, u8) {
    let span = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let second = span * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let floor = lightness - span / 2.0;
    let (r, g, b) = match (hue / 60.0) as u32 {
        0 => (span, second, 0.0),
        1 => (second, span, 0.0),
        2 => (0.0, span, second),
        3 => (0.0, second, span),
        4 => (second, 0.0, span),
        _ => (span, 0.0, second),
    };
    let byte = |v: f64| ((v + floor) * 255.0).round().clamp(0.0, 255.0) as u8;
    (byte(r), byte(g), byte(b))
}

/// A colour's relative luminance, as WCAG defines it.
fn luminance(colour: (u8, u8, u8)) -> f64 {
    let channel = |v: u8| {
        let v = v as f64 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * channel(colour.0) + 0.7152 * channel(colour.1) + 0.0722 * channel(colour.2)
}

/// The `branding` block the front end boots from: the distribution's own,
/// with this brand's name and marks put in it.
///
/// It starts from `pinned` rather than replacing it, because the block
/// carries more than a brand -- `useExpandedHeader` is a layout the
/// application draws, not a colour, and changing it here would be changing
/// the console's shape while claiming to change its logo. Every URL is made
/// absolute against `at`, the console's base path.
pub fn block(at: &dyn Fn(&str) -> String, pinned: &serde_json::Value) -> serde_json::Value {
    let url = |name: &str| at(&format!("{FOLDER}/{name}"));
    let mut found = pinned.clone();
    if !found.is_object() {
        found = serde_json::json!({});
    }
    found["assetFolderUrl"] = serde_json::json!(at(FOLDER));
    found["logo"] =
        serde_json::json!({"defaultUrl": url("logo.png"), "darkModeUrl": url("logo-dark.png")});
    found["mark"] =
        serde_json::json!({"defaultUrl": url("mark.png"), "darkModeUrl": url("mark.png")});
    found["loadingLogo"] =
        serde_json::json!({"defaultUrl": url("mark.png"), "darkModeUrl": url("mark.png")});
    found["faviconUrl"] = serde_json::json!(url("favicon.png"));
    found["applicationTitle"] = serde_json::json!(TITLE);
    found
}

/// What goes into the page's head: this brand's icons and colours, after
/// everything the distribution put there, so that they are what a browser
/// ends up with.
pub fn head(at: &dyn Fn(&str) -> String) -> String {
    format!(
        "<link rel=\"icon\" type=\"image/png\" href=\"{icon}\"/>\
         <link rel=\"shortcut icon\" href=\"{icon}\"/>\
         <link rel=\"apple-touch-icon\" href=\"{mark}\"/>\
         <meta name=\"theme-color\" content=\"{DEEP}\"/>\
         <link rel=\"stylesheet\" href=\"{css}\"/>",
        icon = at(&format!("{FOLDER}/favicon.png")),
        mark = at(&format!("{FOLDER}/mark.png")),
        css = at(&format!("{FOLDER}/brand.css")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contrast of two colours, as WCAG counts it.
    fn contrast(a: &str, b: &str) -> f64 {
        let luminance = |hex: &str| {
            let hex = hex.trim_start_matches('#');
            let channel = |i: usize| {
                let v = u8::from_str_radix(&hex[i..i + 2], 16).unwrap() as f64 / 255.0;
                if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * channel(0) + 0.7152 * channel(2) + 0.0722 * channel(4)
        };
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn text_in_the_brand_colour_can_be_read() {
        // 4.5 is what WCAG AA asks of body text; the accent is the mark's
        // colour and is never text on a light page, which is the whole
        // reason there are two greens
        assert!(contrast(PRIMARY, "#FFFFFF") >= 4.5, "{}", contrast(PRIMARY, "#FFFFFF"));
        assert!(contrast(PRIMARY_DARK, "#FFFFFF") >= 4.5);
        assert!(contrast(DEEP, "#FFFFFF") >= 4.5);
        // and on the dark theme's surface the bright one is what is read
        assert!(contrast(ACCENT, "#1D1E24") >= 4.5, "{}", contrast(ACCENT, "#1D1E24"));
        assert!(contrast(TINT, DEEP) >= 4.5);
    }

    #[test]
    fn every_mark_the_branding_names_is_a_file_that_is_there() {
        let at = |p: &str| p.to_string();
        let block = block(&at, &serde_json::json!({}));
        let mut named = vec![];
        for key in ["logo", "mark", "loadingLogo"] {
            for mode in ["defaultUrl", "darkModeUrl"] {
                named.push(block[key][mode].as_str().unwrap().to_string());
            }
        }
        named.push(block["faviconUrl"].as_str().unwrap().to_string());
        for url in named {
            let file = url.rsplit('/').next().unwrap();
            assert!(asset(file).is_some(), "the branding names {url}, which is not served");
        }
    }

    #[test]
    fn the_marks_are_images_rather_than_whatever_was_in_the_directory() {
        for (name, bytes, kind) in ASSETS {
            assert_eq!(*kind, "image/png", "{name}");
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{name} is not a png");
        }
    }

    #[test]
    fn the_urls_are_under_the_console_s_base_path() {
        let at = |p: &str| format!("/console{p}");
        let block = block(&at, &serde_json::json!({"useExpandedHeader": true}));
        // what is not a brand is left as the distribution had it
        assert_eq!(block["useExpandedHeader"], true);
        assert_eq!(block["assetFolderUrl"], "/console/ui/velosearch");
        assert_eq!(block["logo"]["defaultUrl"], "/console/ui/velosearch/logo.png");
        assert!(head(&at).contains("/console/ui/velosearch/brand.css"));
    }

    /// The blue a distribution's own theme draws its primary in, and the two
    /// shades either side of it that the six stylesheets actually contain.
    const THEME_BLUES: &[&str] = &[
        "#0268BC", "#006BB4", "#0097D1", "#1BA9F5", "#79AAD9", "#49BAF7", "#025AA3", "#014C8A",
        "#093B56", "#013652", "#002A46",
    ];

    /// Colours the theme draws that are not the primary, and must survive
    /// untouched: black, white, the body text and panel borders of both
    /// themes, the danger red, the warning yellow, the success green, and
    /// the categorical palette a chart gives its series.
    const NOT_THE_PRIMARY: &[&str] = &[
        "#000000", "#FFFFFF", "#FCFEFF", "#2A3947", "#DFE5EF", "#0A121A", "#BD271E", "#F5A700",
        "#017D73", "#54B399", "#6092C0", "#D36086", "#9170B8", "#CA8EAE", "#D6BF57", "#B9A888",
        "#DA8B45", "#AA6556", "#E7664C",
    ];

    #[test]
    fn the_theme_s_blue_comes_out_the_brand_s_green() {
        for blue in THEME_BLUES {
            let after = recoloured(blue);
            assert_ne!(&after, blue, "{blue} was left alone");
            let (hue, _, _) = hsl(read(&after));
            assert!(
                (hue - BRAND_HUE).abs() < 2.0,
                "{blue} -> {after} came out at {hue:.0} degrees, not the brand's"
            );
        }
    }

    #[test]
    fn what_is_not_the_primary_is_left_exactly_as_it_was() {
        for colour in NOT_THE_PRIMARY {
            assert_eq!(&recoloured(colour), colour, "{colour} was moved and should not have been");
        }
    }

    #[test]
    fn the_primary_lands_where_the_brand_already_was() {
        // the distribution's primary and the brand's green were arrived at
        // separately, and the move puts the first within two units of the
        // second -- which is the argument that this is the same theme in
        // another colour rather than a different theme
        assert_eq!(recoloured("#0268BC"), "#01773e");
        assert!((luminance(read("#01773e")) - luminance(read(PRIMARY))).abs() < 0.01);
    }

    #[test]
    fn a_colour_comes_out_as_bright_as_it_went_in() {
        // this is the whole promise: contrast is computed from relative
        // luminance, so a theme whose luminances are unchanged is a theme
        // whose every measured contrast ratio is unchanged
        for blue in THEME_BLUES {
            let before = luminance(read(blue));
            let after = luminance(read(&recoloured(blue)));
            let ratio = |l: f64| 1.05 / (l + 0.05);
            assert!(
                (ratio(before) - ratio(after)).abs() < 0.1,
                "{blue}: contrast against white went {:.2} -> {:.2}",
                ratio(before),
                ratio(after)
            );
        }
    }

    #[test]
    fn a_stylesheet_comes_out_a_stylesheet() {
        let css = ".euiButton{background:#0268BC;color:#FFF;border:1px solid rgba(2, 104, 188, .3)}\
                   .euiText{color:#2A3947}\
                   @media (min-width:768px){.x{box-shadow:0 2px 4px rgba(0,0,0,0.15)}}";
        let after = recoloured(css);
        // the shape is untouched: same braces, same declarations, same length
        // of everything that is not a colour
        assert_eq!(css.matches('{').count(), after.matches('{').count());
        assert_eq!(css.matches(';').count(), after.matches(';').count());
        assert!(after.contains(".euiText{color:#2A3947}"), "{after}");
        assert!(after.contains("rgba(0,0,0,0.15)"), "{after}");
        assert!(after.contains("#FFF"), "{after}");
        assert!(!after.to_lowercase().contains("#0268bc"), "{after}");
        assert!(!after.contains("rgba(2, 104, 188"), "{after}");
        assert!(after.contains(", .3)"), "how see-through it is is not a colour: {after}");
    }

    #[test]
    fn nothing_that_is_not_a_colour_is_touched() {
        for text in [
            "#0268BCDD", // eight digits is a colour with an alpha, not six
            "a#12",
            "rgb(2, 104, 188%)",
            "rgb(0.8%, 40%, 74%)",
            "url(#filter0268BC)",
            "--osd-id-0268BC",
        ] {
            assert_eq!(recoloured(text), text, "{text}");
        }
    }

    /// A `#rrggbb` as three numbers.
    fn read(hex: &str) -> (u8, u8, u8) {
        let hex = hex.trim_start_matches('#');
        let at = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap();
        (at(0), at(2), at(4))
    }

    #[test]
    fn the_stylesheet_carries_the_brand_rather_than_a_colour_somebody_typed() {
        let css = stylesheet();
        assert!(css.contains(ACCENT) && css.contains(PRIMARY) && css.contains(DEEP));
        // OpenSearch's own blue is what this replaces; none of it may be left
        assert!(!css.to_lowercase().contains("#0073e6"));
        assert!(!css.to_lowercase().contains("#003553"));
    }
}
