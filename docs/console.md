# The console's server

OpenSearch Dashboards is a browser application and a Node server. The
application is a set of built bundles that boot from what the server tells
them; the server serves those bundles, keeps the saved objects, and answers
the few hundred routes the pages call. VeloSearch provides that server. The
application is not touched: it is served, byte for byte, from a Dashboards
distribution the console is pointed at.

## Running it

```bash
VELOSEARCH_CONSOLE_PATH=/usr/share/opensearch-dashboards   # a 3.1.0 distribution
VELOSEARCH_ENGINE=http://127.0.0.1:9200                    # VeloSearch, or OpenSearch
./target/release/console
```

The console has no login of its own, and every request it makes to the engine
goes with the credentials in `VELOSEARCH_ENGINE` -- including the Dev Tools
proxy, which by default forwards any path the page asks for
(`VELOSEARCH_CONSOLE_PROXY_FILTER`, `.*` unless set, the same default the
Node server has). Against a secured engine that means whoever can reach the
console can do whatever those credentials can do. The Node server
behaves the same way without the security plugin's session handling; the
answer is the same as there -- keep the console off any network you would not
give those credentials to, and narrow the proxy filter.

The distribution is the one thing to fetch: the tarball or the Docker image
of OpenSearch Dashboards 3.1.0, extracted anywhere. The console reads its
bundles, its assets, its translations and its plugin manifests from it, and
nothing else of the Node server is run. The settings are in
[settings.md](settings.md) under "The console".

## What it looks like

The application is OpenSearch Dashboards', and out of the distribution it
calls itself OpenSearch Dashboards and draws itself in OpenSearch's blue.
Neither is in the bundles: the name, the three marks and the favicon come out
of the `branding` block in the metadata the server injects -- a contract the
front end already reads -- and the colours are a stylesheet. So the console
serves VeloSearch's.

| | |
|---|---|
| the name | `VeloSearch`, in the tab and wherever the header writes it |
| the wordmark | the header, one for a light page and one for a dark one |
| the mark | the collapsed navigation, the loading screen and the favicon |
| the colours | `#00C566` is the mark's green; `#00753C` is what text, a link and a filled button are, because the mark's green reads at 2.3 against white and that one at 5.8; `#004628` is the header; `#E1F4E9` is a tint. A dark page swaps the first two, where the bright green is the one that can be read |

### The theme, not a skin over it

A distribution's theme is six prebuilt stylesheets, and every colour in the
application comes out of them. So the console moves the theme rather than
laying a sheet of overrides on top of it: every stylesheet it serves is read,
each colour in it is looked at, and the ones that are the primary blue are
replaced on the way out. The result is kept, so a theme is transformed once
and not once per reader.

Which colours those are was measured, not guessed. Across the six stylesheets
every shade of the primary -- the blue, its hover, its focus ring, its tints
and the lighter versions the dark themes use -- has a hue between 197 and 210
and a saturation of 0.56 or more. Everything outside that window is left
exactly as it was: the blue-greys that body text and panel borders are drawn
in, the danger red, the warning yellow, and the categorical palette a chart
gives its series, where recolouring would make two series the same colour.

A replacement keeps the colour's relative luminance, which is the one quantity
WCAG contrast is computed from. Hue and saturation move; how bright the colour
is does not. Every contrast ratio the people who built the theme measured --
label on a filled button, focus ring against the panel behind it, disabled
text on its background -- comes out the same here. `#0268BC`, the primary,
lands on `#01773E`, two units off the brand's own `#00753C`.

What is left for the stylesheet is what a distribution paints outside the
theme: the bar across the top, and the page drawn before the application has
booted.

The marks are compiled into the binary rather than read from a directory --
a console is one binary pointed at a distribution, and an image a deployment
could forget is a header with a hole in it. They are served under
`/ui/velosearch/`, and the stylesheet with them.

`VELOSEARCH_CONSOLE_BRANDING=opensearch` leaves the distribution's own name,
marks and colours in place, which is what `tools/console_diff.py` wants when
it compares this server's metadata with the Node server's field by field.

## What it pins

Some of what the front end boots from is compiled into the Node server
rather than written down anywhere: the injected metadata, the bundle list,
the default settings, the capabilities, the saved-object index mapping, the
versions each type migrates to, the Dev Tools' description of the engine's
API, the sample data's mappings and saved objects. `tools/osd_pin.py` and
`tools/osd_sample_data.js` take them from a running Dashboards and write
them to `console/`. A console refuses to start against a distribution whose
version it has no pin for, rather than guess.

To make a pin, start the reference pair (`tools/dashboards_reference.sh`
starts Dashboards 3.1.0 and an OpenSearch behind it in Docker) and run:

```bash
python3 tools/osd_pin.py --url http://127.0.0.1:5613 --engine http://127.0.0.1:9221
```

Two things to know: the engine behind the reference has to be running, or
the probe that writes one object of each type finds nothing and records no
migration versions at all; and the index is read through its alias, since a
Dashboards that has had a suite run against it has migrated more than once.

## What it carries

The shell, the base path and the CSP; `uiSettings`; `/api/status` with its
metrics; the saved objects -- the store under `.kibana`, the index migration
that makes `.kibana_N` and moves the alias, the per-type document migrations
(`src/console/migrations/`), the whole API including export, import and
`_resolve_import_errors`, and the management routes; index patterns
(`_fields_for_wildcard`, `_fields_for_time_pattern`, `resolve_index`);
`_msearch`, the `opensearch` search strategy, value suggestions; short URLs;
the Dev Tools proxy; the sample data sets; the DQL and usage counters;
`/api/stats`; the Index Management plugin's index listing and its
`apiCaller`; compression by referrer allowlist; a JSON 404 for the rest.

## What it does not

The other plugins' server halves -- alerting, anomaly detection,
observability, notifications, security analytics, reports, ML -- and the
rest of Index Management's own routes (policies, rollups, transforms,
snapshots). Their pages load and say so. Telemetry, which the 3.1.0
reference does not serve either. Multiple data sources and workspaces.

## How it is measured

- `tools/dashboards_gate.py`: OpenSearch Dashboards' own `test/api_integration`
  (166 cases) against whichever server is named. The Node server's own
  score is recorded in `tools/dashboards_baseline.json` -- it fails 24 --
  and a failure counts against this console only when the Node server
  passes the case. **146 of 166, none ours alone.**
- `tools/console_diff.py`: the shell and `bootstrap.js` compared with a
  running Dashboards, field by field and character by character.
- `tools/dashboards_check.py`: six areas that suite never asks about.
- Every flow driven by hand in a browser -- Discover, Visualize, a
  dashboard, saved objects, Index Management, Dev Tools -- against a
  VeloSearch node.

Every request that changes something must carry the `osd-xsrf` header, as
the pages do; the suite does not, so the console it tests is started with
`VELOSEARCH_CONSOLE_XSRF=false`, which is the `--server.xsrf.disableProtection=true`
the suite starts the Node server with.

The suite needs the Dashboards repository bootstrapped
(cloned to `study/OpenSearch-Dashboards`, Node 20, `yarn osd bootstrap`) and the
server it tests started with `--server.xsrf.disableProtection=true`, which
is the difference between 76 and 140 for the Node server itself.
