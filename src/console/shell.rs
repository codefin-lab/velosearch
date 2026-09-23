//! The page the browser is given before it has run anything.
//!
//! Three things have to be right or the console does not start at all, and
//! none of them says so when it is wrong -- the application simply fails in
//! the browser with a message about the server.
//!
//!   - the metadata, in `<osd-injected-metadata>`: the version, where the
//!     server is, which plugins exist and what every setting defaults to
//!   - the boot script, which names every bundle in the order they load
//!   - the assets both of those name, at the paths they name them at

use serde_json::{Value, json};

use super::Console;

impl Console {
    /// The page for any application. Every application is served the same
    /// page: which one it is is in the URL, and the front end reads it.
    pub fn page(&self, user: Value) -> String {
        let csp = json!({"strictCsp": self.strict_csp()});
        let metadata = self.metadata_with(user);
        format!(
            "{head}<osd-csp data=\"{csp}\"></osd-csp>\
             <osd-injected-metadata data=\"{metadata}\"></osd-injected-metadata>{tail}",
            head = self.branded_head(&self.with_base_path(&self.pinned.shell_head)),
            tail = self.with_base_path(&self.pinned.shell_tail),
            csp = escape(&csp.to_string()),
            metadata = escape(&metadata.to_string()),
        )
    }

    /// What the front end boots from.
    ///
    /// Most of it is the pinned contract as it stands. What is not: the base
    /// path, which is this server's own, and the settings somebody has
    /// changed, which are read for the page rather than fetched by it -- a
    /// console that drew itself with the default theme and then redrew with
    /// the chosen one would flash white at every reader who did not want it.
    pub fn metadata_with(&self, user: Value) -> Value {
        let mut found = self.metadata();
        found["legacyMetadata"]["uiSettings"]["user"] = user;
        found
    }

    pub fn metadata(&self) -> Value {
        json!({
            "version": self.pinned.version,
            "buildNumber": self.pinned.build_number,
            "branch": self.pinned.branch,
            "basePath": self.base_path,
            "serverBasePath": self.base_path,
            "env": self.pinned.env,
            "anonymousStatusPage": self.anonymous_status,
            "i18n": {"translationsUrl": self.at("/translations/en.json")},
            "csp": self.pinned.csp.get("warnLegacyBrowsers")
                .map(|v| json!({"warnLegacyBrowsers": v}))
                .unwrap_or_else(|| json!({"warnLegacyBrowsers": true})),
            "vars": self.pinned.vars,
            "uiPlugins": self.pinned.ui_plugins,
            "legacyMetadata": {
                "uiSettings": {
                    "defaults": self.pinned.setting_defaults,
                    // what somebody has actually set, which the page carries
                    // rather than the front end fetching it. Put here by
                    // `metadata_with`; the build number alone is what a page
                    // served without reaching the engine can honestly say.
                    "user": {"buildNum": {"userValue": self.pinned.build_number}},
                }
            },
            "branding": self.branding(),
            "survey": self.pinned.survey,
        })
    }

    /// The script that loads every bundle, in the order they load.
    ///
    /// The order is not ours to choose: a bundle that runs before something
    /// it needs throws, and the front end shows the same failure it shows for
    /// everything else.
    pub fn bootstrap(&self) -> String {
        let paths: serde_json::Map<String, Value> = self
            .pinned
            .public_paths
            .iter()
            .map(|(name, path)| (name.clone(), json!(self.at(path))))
            .collect();
        let bundles: Vec<String> =
            self.pinned.bundles.iter().map(|b| format!("        '{}',", self.at(b))).collect();
        let theme_css = &self.pinned.theme_css;
        let kui_css = &self.pinned.kui_css;
        let shared = self.at(&format!("/{}/bundles/osd-ui-shared-deps/", self.pinned.build_number));
        let kui = self.at("/node_modules/@osd/ui-framework/dist/");
        let ui = self.at("/ui/");
        // this is the reference's own bootstrap, kept to the character where
        // the front end could tell: the loader it defines, the order it loads
        // in, the theme it chooses from the tag the startup script set
        format!(
            r#"var osdCsp = JSON.parse(document.querySelector('osd-csp').getAttribute('data'));
window.__osdStrictCsp__ = osdCsp.strictCsp;
window.__osdPublicPath__ = {paths};
window.__osdBundles__ = (function osdBundlesLoader() {{
  var modules = {{}};
  function has(prop) {{
    return Object.prototype.hasOwnProperty.call(modules, prop);
  }}
  function define(key, bundleRequire, bundleModuleKey) {{
    if (has(key)) {{
      throw new Error('__osdBundles__ already has a module defined for "' + key + '"');
    }}
    modules[key] = {{
      bundleRequire,
      bundleModuleKey
    }};
  }}
  function get(key) {{
    if (!has(key)) {{
      throw new Error('__osdBundles__ does not have a module defined for "' + key + '"');
    }}
    return modules[key].bundleRequire(modules[key].bundleModuleKey);
  }}
  return {{
    has: has,
    define: define,
    get: get
  }};
}})();

if (window.__osdStrictCsp__ && window.__osdCspNotEnforced__) {{
  var legacyBrowserError = document.getElementById('osd_legacy_browser_error');
  legacyBrowserError.style.display = 'flex';
}} else {{
  if (!window.__osdCspNotEnforced__ && window.console) {{
    window.console.log("^ A single error about an inline script not firing due to content security policy is expected!");
  }}

  var themeCssDistFilenames = {theme_css};
  var kuiCssDistFilenames = {kui_css};

  var isDarkMode = window.__osdThemeTag__.endsWith('dark');
  var themeMode = isDarkMode ? 'dark' : 'light';
  var themeVersion = window.__osdThemeTag__.replace(/(light|dark)$/, '');
  if (!themeCssDistFilenames[themeVersion]) {{
    themeVersion = 'v8'; // this default should only be needed by tests
  }}

  var loadingMessage = document.getElementById('osd_loading_message');
  loadingMessage.style.display = 'flex';

  var styleSheetPaths = [
    '{shared}osd-ui-shared-deps.css',
    '{shared}' + themeCssDistFilenames[themeVersion][themeMode],
    '{kui}' + kuiCssDistFilenames[themeVersion][themeMode],
    '{ui}legacy_' + themeMode + '_theme.css',
  ];

  window.onload = function () {{
    function failure() {{
      // make subsequent calls to failure() noop
      failure = function () {{}};

      var err = document.createElement('h1');
      err.style['color'] = 'white';
      err.style['font-family'] = 'monospace';
      err.style['text-align'] = 'center';
      err.style['background'] = '#F44336';
      err.style['padding'] = '25px';
      err.innerText = document.querySelector('[data-error-message]').dataset.errorMessage;

      document.body.innerHTML = err.outerHTML;
    }}

    var stylesheetTarget = document.querySelector('head meta[name="add-styles-here"]')
    function loadStyleSheet(url, cb) {{
      var dom = document.createElement('link');
      dom.rel = 'stylesheet';
      dom.type = 'text/css';
      dom.href = url;
      dom.addEventListener('error', failure);
      dom.addEventListener('load', cb);
      document.head.insertBefore(dom, stylesheetTarget);
    }}

    var scriptsTarget = document.querySelector('head meta[name="add-scripts-here"]')
    function loadScript(url, cb) {{
      var dom = document.createElement('script');
      dom.async = false;
      dom.src = url;
      dom.addEventListener('error', failure);
      dom.addEventListener('load', cb);
      document.head.insertBefore(dom, scriptsTarget);
    }}

    function load(urls, cb) {{
      var pending = urls.length;
      urls.forEach(function (url) {{
        var innerCb = function () {{
          pending = pending - 1;
          if (pending === 0 && typeof cb === 'function') {{
            cb();
          }}
        }}

        if (typeof url !== 'string') {{
          load(url, innerCb);
        }} else if (url.slice(-4) === '.css') {{
          loadStyleSheet(url, innerCb);
        }} else {{
          loadScript(url, innerCb);
        }}
      }});
    }}

    load([
{bundles}
    ], function () {{
      __osdBundles__.get('entry/core/public').__osdBootstrap__();

      load(styleSheetPaths);
    }});
  }}
}}
"#,
            paths = Value::Object(paths),
            theme_css = theme_css,
            kui_css = kui_css,
            bundles = bundles.join("\n"),
        )
    }

    /// The script that runs before the page is drawn, so that a reader who
    /// chose a dark theme is not shown a white page first.
    pub fn startup(&self) -> String {
        self.with_base_path(&self.pinned.startup)
    }

    /// Where the console's own images are, and what it calls itself.
    ///
    /// VeloSearch's, unless an operator asked for the distribution's own
    /// (`VELOSEARCH_CONSOLE_BRANDING=opensearch`). Either way the URLs are
    /// made absolute against this server's base path, since the front end
    /// puts them in the page as it finds them.
    fn branding(&self) -> Value {
        if super::brand::wanted() {
            return super::brand::block(&|path| self.at(path), &self.pinned.branding);
        }
        let mut found = self.pinned.branding.clone();
        if let Some(url) = found.get("assetFolderUrl").and_then(|v| v.as_str()) {
            let at = self.at(url);
            found["assetFolderUrl"] = json!(at);
        }
        found
    }

    /// The head of the page with this brand's title, icons and stylesheet in
    /// it.
    ///
    /// They go last, immediately before the head ends, because a browser
    /// takes the last icon and the last stylesheet it is given: the
    /// distribution's own are left where they are rather than cut out, so a
    /// head shaped differently in some later version still ends up branded
    /// rather than broken.
    fn branded_head(&self, head: &str) -> String {
        if !super::brand::wanted() {
            return head.to_string();
        }
        let titled = match (head.find("<title>"), head.find("</title>")) {
            (Some(open), Some(close)) if open < close => {
                format!("{}{}{}", &head[..open + 7], super::brand::TITLE, &head[close..])
            }
            _ => head.to_string(),
        };
        let ours = super::brand::head(&|path| self.at(path));
        match titled.rfind("</head>") {
            Some(at) => format!("{}{ours}{}", &titled[..at], &titled[at..]),
            // a head that does not end is a distribution shaped differently
            // than every one seen so far; the page is still served
            None => titled,
        }
    }

    /// Whether the policy this server sends allows an inline script.
    ///
    /// The page carries one on purpose: a browser that runs it says so, and a
    /// browser that refuses to is one enforcing the policy. That is how the
    /// front end finds out which kind it is running in.
    pub fn strict_csp(&self) -> bool {
        self.pinned.csp.get("strictCsp").and_then(|v| v.as_bool()).unwrap_or(false)
    }

    /// What this server sends as its content security policy.
    pub fn content_security_policy(&self) -> String {
        let script = match self.strict_csp() {
            true => "script-src 'self'",
            // the page's own inline script has to run for the front end to
            // know the policy is not being enforced
            false => "script-src 'unsafe-eval' 'self'",
        };
        format!("{script}; worker-src blob: 'self'; style-src 'unsafe-inline' 'self'")
    }

    /// A path under this server's base path.
    pub fn at(&self, path: &str) -> String {
        match self.base_path.is_empty() {
            true => path.to_string(),
            false => format!("{}{path}", self.base_path),
        }
    }

    /// Every absolute URL in a piece of the page, moved under the base path.
    ///
    /// The pin was taken from a server with no base path, so its URLs start at
    /// the root. A server that has one has to put it back, and there is
    /// nothing in the page that is an absolute URL and not ours.
    fn with_base_path(&self, text: &str) -> String {
        match self.base_path.is_empty() {
            true => text.to_string(),
            false => text.replace("\"/", &format!("\"{}/", self.base_path)),
        }
    }
}

/// A string as an HTML attribute may carry it.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::pinned::Pinned;

    fn console(base_path: &str) -> Console {
        let raw = std::fs::read_to_string("console/osd-3.1.0.json").expect("the pinned contract");
        let pinned: Pinned = serde_json::from_str(&raw).expect("readable");
        Console {
            home: "/nowhere".into(),
            pinned,
            base_path: base_path.to_string(),
            overrides: Default::default(),
            anonymous_status: true,
            mapping: Default::default(),
            sample_data: Default::default(),
            uuid: "test".into(),
            plugin_dirs: Default::default(),
        }
    }

    #[test]
    fn the_page_carries_both_elements_the_front_end_reads() {
        let page = console("").page(json!({}));
        assert!(page.contains("<osd-csp data="), "no csp element");
        assert!(page.contains("<osd-injected-metadata data="), "no metadata element");
        assert!(page.contains("bootstrap.js"), "nothing would start");
    }

    #[test]
    fn the_page_calls_itself_velosearch_rather_than_the_distribution_s_name() {
        let page = console("").page(json!({}));
        assert!(page.contains("<title>VeloSearch</title>"), "the tab still says something else");
        let meta = metadata_of(&page);
        assert_eq!(meta["branding"]["applicationTitle"], "VeloSearch");
        // the header reads this, and it is the one place the old name would
        // have survived the title being replaced
        assert!(
            !meta["branding"].to_string().contains("OpenSearch Dashboards"),
            "the branding still names the distribution"
        );
    }

    #[test]
    fn the_marks_and_the_stylesheet_are_in_the_page_under_its_base_path() {
        let page = console("/console").page(json!({}));
        for named in [
            "/console/ui/velosearch/brand.css",
            "/console/ui/velosearch/favicon.png",
            "/console/ui/velosearch/mark.png",
        ] {
            assert!(page.contains(named), "the page does not name {named}");
        }
        let meta = metadata_of(&page);
        assert_eq!(meta["branding"]["logo"]["defaultUrl"], "/console/ui/velosearch/logo.png");
        assert_eq!(meta["branding"]["logo"]["darkModeUrl"], "/console/ui/velosearch/logo-dark.png");
        // and they come after the distribution's own, which is what makes
        // them the ones a browser ends up with
        let ours = page.find("/console/ui/velosearch/favicon.png").expect("ours");
        let theirs = page.find("/console/ui/favicons/favicon-32x32.png").expect("the pinned one");
        assert!(theirs < ours, "the distribution's icon is served after this brand's");
        assert!(ours < page.find("</head>").expect("a head that ends"));
    }

    /// The metadata a page carries, read back the way the front end reads it.
    fn metadata_of(page: &str) -> Value {
        let raw = page
            .split("<osd-injected-metadata data=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("the metadata element");
        let decoded = raw
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&");
        serde_json::from_str(&decoded).expect("json a browser could read")
    }

    #[test]
    fn the_metadata_is_json_a_browser_can_read_back() {
        let page = console("").page(json!({}));
        let at = page.find("<osd-injected-metadata data=\"").expect("the element");
        let rest = &page[at + "<osd-injected-metadata data=\"".len()..];
        let raw = &rest[..rest.find('"').expect("the attribute ends")];
        let unescaped = raw
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&");
        let found: Value = serde_json::from_str(&unescaped).expect("the metadata parses");
        assert_eq!(found["version"], "3.1.0");
        assert_eq!(found["basePath"], "");
        assert!(found["uiPlugins"].as_array().is_some_and(|a| !a.is_empty()));
        assert!(
            found["legacyMetadata"]["uiSettings"]["defaults"]
                .as_object()
                .is_some_and(|o| o.len() > 50),
            "the front end asks for every setting it knows about"
        );
    }

    #[test]
    fn a_base_path_reaches_every_url_the_page_names() {
        let page = console("/dash").page(json!({}));
        assert!(page.contains("\"/dash/bootstrap.js\""), "the boot script");
        assert!(page.contains("\"/dash/ui/"), "the images");
        assert!(!page.contains("\"/ui/"), "and nothing was left at the root");
        let boot = console("/dash").bootstrap();
        assert!(boot.contains("/dash/8487/bundles/core/"), "the bundles");
    }

    #[test]
    fn the_boot_script_names_every_bundle_and_starts_the_application() {
        let console = console("");
        let boot = console.bootstrap();
        for bundle in &console.pinned.bundles {
            assert!(boot.contains(bundle.as_str()), "{bundle} is not loaded");
        }
        assert!(boot.contains("__osdBootstrap__()"), "nothing would start");
    }

    #[test]
    fn the_policy_lets_the_page_find_out_whether_it_is_enforced() {
        // the page carries an inline script on purpose: a browser that runs it
        // says so, and one that refuses is enforcing the policy
        let policy = console("").content_security_policy();
        let scripts = policy
            .split(';')
            .map(str::trim)
            .find(|d| d.starts_with("script-src"))
            .expect("a policy says where scripts may come from");
        assert!(
            !scripts.contains("unsafe-inline"),
            "an inline script must not run, or the page cannot tell: {scripts}"
        );
        // styles are a different question, and the front end writes them inline
        assert!(policy.contains("style-src 'unsafe-inline'"), "{policy}");
    }

    #[test]
    fn the_page_carries_the_settings_somebody_changed() {
        // fetched by the front end instead, a console would draw itself with
        // the default theme and then redraw with the chosen one
        let user = json!({"theme:darkMode": {"userValue": true}});
        let page = console("").page(user.clone());
        assert!(page.contains("theme:darkMode"), "the page does not carry it");
        let found = console("").metadata_with(user);
        assert_eq!(
            found["legacyMetadata"]["uiSettings"]["user"]["theme:darkMode"]["userValue"],
            true
        );
    }

    #[test]
    fn capabilities_answer_for_the_applications_that_were_asked_about() {
        let console = console("");
        let asked = vec!["home".to_string(), "discover".to_string()];
        let found = console.capabilities(&asked);
        assert_eq!(found["navLinks"]["home"], true);
        assert_eq!(found["navLinks"]["discover"], true);
        assert!(found["navLinks"].as_object().is_some_and(|o| o.len() == 2), "and no others");
        // and the rest of it is what the plugins decided, whoever asked
        assert!(found["catalogue"].is_object(), "the catalogue is missing");
        assert_eq!(console.capabilities(&[])["navLinks"], json!({}));
    }

    #[test]
    fn a_pin_for_another_version_is_refused() {
        let path = std::path::Path::new("console/osd-3.1.0.json");
        let e = match Pinned::read(path, "9.9.9") {
            Err(e) => e,
            Ok(_) => panic!("a pin for another version was accepted"),
        };
        assert!(e.contains("3.1.0") && e.contains("9.9.9"), "{e}");
    }
}
