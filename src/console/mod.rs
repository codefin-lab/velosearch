//! The server the console's front end talks to.
//!
//! OpenSearch Dashboards is two programs: a Node server and a React
//! application in the browser. This is the first one. The second is left
//! exactly as the OpenSearch project publishes it -- we serve their bundle and
//! answer the requests it makes, rather than forking a line of its JavaScript.
//!
//! What the browser needs before it can run at all is in [`shell`]: a page
//! carrying the metadata the application boots from, the script that starts
//! it, and the assets both of those name. That metadata is a contract between
//! the two halves of one program and is written down nowhere, so it is pinned
//! from a Dashboards that works rather than guessed -- see `tools/osd_pin.py`
//! and `console/osd-<version>.json`.

pub mod assets;
pub mod brand;
pub mod engine;
pub mod fields;
pub mod filter;
pub mod ism;
pub mod management;
pub mod metrics;
pub mod migrate;
pub mod migrations;
pub mod pinned;
pub mod sample_data;
pub mod saved;
pub mod search;
pub mod settings;
pub mod shell;
pub mod urls;
pub mod usage;

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

/// Now, as a saved object records the time it was written.
///
/// The same format the engine's own writes use, to the millisecond: a console
/// and the engine behind it disagreeing about what time looks like would show
/// up as a saved object that sorts oddly and nothing else.
pub fn now() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default();
    crate::store::format_millis(ms, "strict_date_optional_time").unwrap_or_default()
}

/// Where the console's front end and its pinned contract are.
pub struct Console {
    /// an OpenSearch Dashboards distribution: the built bundles, the assets
    /// and the plugin manifests
    pub home: PathBuf,
    /// what that distribution's server would have told the front end
    pub pinned: pinned::Pinned,
    /// the path every URL this serves is under, `""` for none
    pub base_path: String,
    /// settings an operator fixed when this server was started, which no
    /// reader may change
    pub overrides: BTreeMap<String, Value>,
    /// whether the status page answers without a sign-in -- this server's own
    /// choice, the way `status.allowAnonymous` is the Node server's
    pub anonymous_status: bool,
    /// the shape the console's index should have, taken out of the pin once
    /// so that everything that may have to put it back has it
    pub mapping: Value,
    /// the sample data sets the home page offers, as pinned by
    /// `tools/osd_sample_data.js`; none where the pin is not there
    pub sample_data: Vec<Value>,
    /// what this server calls itself, kept for as long as it runs
    ///
    /// The one it replaces keeps its across restarts, in a file beside its
    /// data. Nothing reads it but the status page, so a fresh one each start
    /// is the difference between a console that has been restarted looking
    /// like a different console and looking like the same one -- worth
    /// keeping, and 13.2 does not need it yet.
    pub(crate) uuid: String,
    /// where each plugin's built files are, by the id the browser asks for
    ///
    /// A URL names a plugin the way its manifest does -- `usageCollection` --
    /// and the directory it lives in is named another way -- `usage_collection`
    /// for the ones a distribution ships with, and the manifest's own name for
    /// the ones added to it. Guessing at the conversion works until a plugin is
    /// named in a way the guess does not cover, so the manifests are read
    /// instead: each one says which id it is, and it is standing in its own
    /// directory while it says so.
    pub plugin_dirs: std::collections::HashMap<String, PathBuf>,
}

impl Console {
    /// Read a distribution and the pin that goes with its version.
    ///
    /// Both have to be there and they have to agree: a pin from one version
    /// in front of another version's bundles serves a page that names files
    /// which are not there, and the browser's failure would say nothing about
    /// why.
    pub fn open(
        home: PathBuf,
        pins: &std::path::Path,
        base_path: String,
        overrides: BTreeMap<String, Value>,
    ) -> Result<Console, String> {
        let package = home.join("package.json");
        let raw = std::fs::read_to_string(&package)
            .map_err(|e| format!("no OpenSearch Dashboards at {}: {e}", home.display()))?;
        let found: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", package.display()))?;
        let version = found
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{} says no version", package.display()))?
            .to_string();
        let pinned = pinned::Pinned::read(&pins.join(format!("osd-{version}.json")), &version)?;
        let plugin_dirs = plugin_dirs(&home);
        let uuid = uuid_of();
        let mapping = pinned.saved_object_index.get("mappings").cloned().unwrap_or_default();
        let anonymous_status = std::env::var("VELOSEARCH_CONSOLE_ANONYMOUS_STATUS")
            .map(|v| v != "false")
            .unwrap_or(true);
        let sample_data: Vec<Value> = std::fs::read_to_string(pins.join("sample_data.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Ok(Console {
            sample_data,
            home,
            pinned,
            base_path,
            overrides,
            anonymous_status,
            mapping,
            uuid,
            plugin_dirs,
        })
    }
}

impl Console {
    /// What this server calls itself.
    pub fn uuid(&self) -> &str {
        &self.uuid
    }

    /// What a caller may do: what the plugins decided, and one entry per
    /// application the caller asked about.
    pub fn capabilities(&self, applications: &[String]) -> Value {
        let mut found = self.pinned.capabilities.clone();
        let links: serde_json::Map<String, Value> =
            applications.iter().map(|id| (id.clone(), Value::Bool(true))).collect();
        found["navLinks"] = Value::Object(links);
        found
    }
}

/// Settings an operator fixed when the server was started, which no reader
/// may change.
///
/// `key=value` pairs separated by commas. A value is JSON where it reads as
/// JSON and the text it is otherwise, so that `false` is a boolean and
/// `Asia/Bangkok` is a string without anybody having to quote it on a command
/// line that would eat the quotes.
pub fn overrides_from(listed: &str) -> BTreeMap<String, Value> {
    listed
        .split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| {
            let value = value.trim();
            let parsed =
                serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string()));
            (key.trim().to_string(), parsed)
        })
        .filter(|(key, _)| !key.is_empty())
        .collect()
}

/// A name for this server, made from the time it started and where it is.
fn uuid_of() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    // the clock on macOS moves in microseconds, so two names asked for within
    // one of them came out the same; a count keeps them apart
    static ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let asked = ASKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut seed =
        now as u64 ^ (std::process::id() as u64) << 32 ^ asked.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut hex = String::new();
    for _ in 0..32 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        hex.push(char::from_digit(((seed >> 33) % 16) as u32, 16).unwrap_or('0'));
    }
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// Every plugin in a distribution, by the id its manifest gives it.
fn plugin_dirs(home: &std::path::Path) -> std::collections::HashMap<String, PathBuf> {
    let mut out = std::collections::HashMap::new();
    for root in ["src/plugins", "plugins"] {
        let Ok(entries) = std::fs::read_dir(home.join(root)) else { continue };
        for entry in entries.flatten() {
            let manifest = entry.path().join("opensearch_dashboards.json");
            let Ok(raw) = std::fs::read_to_string(&manifest) else { continue };
            let Ok(found) = serde_json::from_str::<serde_json::Value>(&raw) else { continue };
            if let Some(id) = found.get("id").and_then(|v| v.as_str()) {
                out.insert(id.to_string(), entry.path());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_override_reads_its_value_as_what_it_looks_like() {
        let found =
            overrides_from("query:enhancements:enabled=false,dateFormat:tz=Asia/Bangkok,n=3");
        assert_eq!(found["query:enhancements:enabled"], json!(false), "a boolean");
        assert_eq!(found["dateFormat:tz"], json!("Asia/Bangkok"), "a string nobody quoted");
        assert_eq!(found["n"], json!(3), "a number");
    }

    #[test]
    fn nothing_is_overridden_by_nothing() {
        assert!(overrides_from("").is_empty());
        assert!(overrides_from("nonsense-with-no-equals").is_empty());
    }

    #[test]
    fn a_name_for_this_server_is_shaped_like_one() {
        let name = uuid_of();
        assert_eq!(name.len(), 36, "{name}");
        assert_eq!(name.match_indices('-').count(), 4, "{name}");
        assert_ne!(name, uuid_of(), "two starts are two servers");
    }
}
