//! The files the page names: the bundles, the images, the fonts, the
//! translations.
//!
//! Nothing here is generated. Every one of them is a file in the OpenSearch
//! Dashboards distribution, and the whole of Phase 13 rests on not touching
//! them: the browser runs the application the OpenSearch project published,
//! and this hands it over.

use std::path::{Path, PathBuf};

use super::Console;

/// The stylesheets already moved onto the brand's colours, by the file they
/// were read from. A distribution's files do not change while it is being
/// served, so what was made of one once is what it makes every time.
static KEPT: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<PathBuf, Vec<u8>>>> =
    std::sync::LazyLock::new(Default::default);

/// A file to hand back, and what it is.
pub struct Served {
    pub bytes: Vec<u8>,
    pub kind: &'static str,
    /// the encoding it is already in, where a pre-compressed copy was taken
    pub encoding: Option<&'static str>,
}

impl Console {
    /// The file a URL under `/{buildNum}/bundles/…` names.
    ///
    /// Three kinds of bundle live in three places in a distribution, and the
    /// URL says which without saying where:
    ///
    /// ```text
    ///   /bundles/core/…                 src/core/target/public/
    ///   /bundles/osd-ui-shared-deps/…   node_modules/@osd/ui-shared-deps/target/
    ///   /bundles/plugin/{id}/…          src/plugins/{id}/target/public/
    ///                                   or plugins/{id}/target/public/
    /// ```
    pub fn bundle(&self, rest: &str, accepts: &str) -> Option<Served> {
        let (which, file) = rest.split_once('/')?;
        let base = match which {
            "core" => self.home.join("src/core/target/public"),
            "osd-ui-shared-deps" => self.home.join("node_modules/@osd/ui-shared-deps/target"),
            "plugin" => {
                let (id, file) = file.split_once('/')?;
                let dir = self.plugin_dirs.get(id)?.join("target/public");
                return self.themed(&dir, file, accepts);
            }
            _ => return None,
        };
        self.themed(&base, file, accepts)
    }

    /// The file a URL under `/plugins/{id}/assets/…` names: what a plugin
    /// ships beside its bundles and shows on its own pages -- the home
    /// page's illustrations, the sample data's preview images.
    pub fn plugin_asset(&self, id: &str, rest: &str, accepts: &str) -> Option<Served> {
        let dir = self.plugin_dirs.get(id)?.join("public/assets");
        self.file(&dir, rest, accepts)
    }

    /// The file a URL under `/node_modules/@osd/ui-framework/dist/…` names:
    /// the one directory of a distribution's `node_modules` the browser is
    /// sent to, for the theme's stylesheet.
    pub fn ui_framework(&self, rest: &str, accepts: &str) -> Option<Served> {
        self.themed(&self.home.join("node_modules/@osd/ui-framework/dist"), rest, accepts)
    }

    /// One file, with a stylesheet's colours moved onto the brand's.
    ///
    /// A stylesheet is read uncompressed even where a compressed copy is
    /// sitting beside it, because the point is to change what is in it, and
    /// the result is kept: a theme is most of a megabyte and every reader
    /// asks for the same one.
    fn themed(&self, base: &Path, relative: &str, accepts: &str) -> Option<Served> {
        if !relative.ends_with(".css") || !super::brand::wanted() {
            return self.file(base, relative, accepts);
        }
        let mut path = PathBuf::from(base);
        path.push(relative);
        if let Some(kept) = KEPT.lock().ok().and_then(|kept| kept.get(&path).cloned()) {
            return Some(Served { bytes: kept, kind: "text/css; charset=utf-8", encoding: None });
        }
        let served = self.file(base, relative, "")?;
        let bytes = match std::str::from_utf8(&served.bytes) {
            Ok(css) => super::brand::recoloured(css).into_bytes(),
            // a stylesheet that is not text is a stylesheet this cannot read,
            // and handing it over unchanged is better than not at all
            Err(_) => served.bytes,
        };
        if let Ok(mut kept) = KEPT.lock() {
            kept.insert(path, bytes.clone());
        }
        Some(Served { bytes, ..served })
    }

    /// The file a URL under `/ui/…` names: the favicons, the logos, the fonts
    /// and the two legacy themes.
    pub fn ui_asset(&self, rest: &str, accepts: &str) -> Option<Served> {
        self.file(&self.home.join("src/core/server/core_app/assets"), rest, accepts)
    }

    /// The messages the front end shows, in the language asked for.
    ///
    /// A distribution ships English as the absence of a translation -- the
    /// strings are in the source -- so a locale nobody translated is answered
    /// the same way English is, with nothing to substitute.
    pub fn translations(&self, locale: &str) -> Served {
        // a locale is a tag, not a path: `..` in one named a file the server
        // was never asked to publish
        let named = if locale.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            locale
        } else {
            "en"
        };
        let file = self.home.join("src/core/server/i18n").join(format!("{named}.json"));
        let messages = std::fs::read_to_string(&file)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|found| found.get("messages").cloned())
            .unwrap_or_else(|| serde_json::json!({}));
        Served {
            bytes: serde_json::json!({
                "translations": {"locale": locale, "messages": messages},
                "warning": serde_json::Value::Null,
            })
            .to_string()
            .into_bytes(),
            kind: "application/json; charset=utf-8",
            encoding: None,
        }
    }

    /// One file under a directory, refusing anything that climbs out of it.
    ///
    /// A URL is not a path: `..` in one is a request for a file the server
    /// was never asked to publish, and the only safe answer is that there is
    /// no such file.
    fn file(&self, base: &Path, relative: &str, accepts: &str) -> Option<Served> {
        let mut path = PathBuf::from(base);
        for part in relative.split('/') {
            if part.is_empty() || part == "." || part == ".." {
                return None;
            }
            path.push(part);
        }
        let kind = kind_of(&path);
        // the distribution ships each bundle compressed as well; handing one
        // over saves compressing it again for every reader
        for (suffix, encoding) in [(".br", "br"), (".gz", "gzip")] {
            if accepts.contains(encoding.trim_end_matches("ip"))
                && let Ok(bytes) = std::fs::read(path.with_extension(format!(
                    "{}{suffix}",
                    path.extension().and_then(|e| e.to_str()).unwrap_or_default()
                )))
            {
                return Some(Served { bytes, kind, encoding: Some(encoding) });
            }
        }
        Some(Served { bytes: std::fs::read(&path).ok()?, kind, encoding: None })
    }
}

/// What a file is, by the name it has.
fn kind_of(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or_default() {
        "js" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "xml" => "application/xml",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
