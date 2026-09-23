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

/// The stylesheet that re-colours the application.
///
/// Two layers, because a theme is not one thing. The first sets the
/// variables the shipped theme defines its own colours in, which is where
/// most of the application reads from. The second names the few surfaces
/// that are painted rather than derived -- the header, a filled button, a
/// selected tab, a link -- for a build whose variables are named differently.
/// Everything is additive and this sheet is last, so the worst a mismatch can
/// do is leave a surface the colour it already was.
pub fn stylesheet() -> String {
    format!(
        ":root {{\
           --velo-accent: {ACCENT};\
           --velo-primary: {PRIMARY};\
           --velo-primary-dark: {PRIMARY_DARK};\
           --velo-deep: {DEEP};\
           --velo-tint: {TINT};\
           --ouiColorPrimary: {PRIMARY};\
           --ouiColorPrimaryText: {PRIMARY};\
           --ouiColorAccent: {ACCENT};\
           --ouiColorAccentText: {PRIMARY};\
           --ouiColorLink: {PRIMARY};\
           --ouiLinkColor: {PRIMARY};\
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
           --ouiColorPrimary: {ACCENT};\
           --ouiColorPrimaryText: {ACCENT};\
           --ouiColorLink: {ACCENT};\
           --ouiLinkColor: {ACCENT};\
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

    #[test]
    fn the_stylesheet_carries_the_brand_rather_than_a_colour_somebody_typed() {
        let css = stylesheet();
        assert!(css.contains(ACCENT) && css.contains(PRIMARY) && css.contains(DEEP));
        // OpenSearch's own blue is what this replaces; none of it may be left
        assert!(!css.to_lowercase().contains("#0073e6"));
        assert!(!css.to_lowercase().contains("#003553"));
    }
}
