#!/usr/bin/env python3
"""Our console's shell against the one it replaces, field by field.

The metadata the front end boots from is a contract between two halves of one
program that nobody wrote down. The only honest way to check a reimplementation
of it is to put the two side by side and look at every field, which is what
this does: the injected metadata, the boot script's bundle list and public
paths, and whether every file either page names is actually served.

    tools/dashboards_reference.sh
    tools/console_diff.py --ours http://127.0.0.1:5614 --reference http://127.0.0.1:5613

Some differences are correct. The base path is this server's own; the settings
a user has changed are live state; the reference is started with an override
its own suite needs. Those are named below rather than hidden, so that a
difference nobody expected still shows up.
"""
import argparse
import html
import json
import re
import sys
import urllib.request

# fields that are the server's own rather than the contract's
LIVE = {
    "basePath": "this server's own",
    "serverBasePath": "this server's own",
    # the name, the marks and the colours are this console's own: it is
    # VeloSearch's console rather than a copy of the reference's chrome.
    # `VELOSEARCH_CONSOLE_BRANDING=opensearch` puts the distribution's back,
    # which is how this field can still be compared when it is worth it.
    "branding": "this console's own; see docs/console.md",
}
failures = []


def get(url, path):
    with urllib.request.urlopen(url + path, timeout=30) as answer:
        return answer.read().decode()


def shell(url):
    page = get(url, "/app/home")
    boot = get(url, "/bootstrap.js")
    meta = json.loads(
        html.unescape(re.search(r'<osd-injected-metadata data="([^"]*)"', page).group(1))
    )
    paths = dict(
        re.findall(r'"([^"]+)":"([^"]+)"',
                   re.search(r"__osdPublicPath__ = (\{.*?\});", boot, re.S).group(1))
    )
    block = re.search(r"\[\s*((?:'[^']*',?\s*)+)\]\s*,\s*function", boot)
    # the rest of the bootstrap, with the two lists that are compared as
    # sets taken out, is compared as text: the loader, the theme choice and
    # the order of things are the front end's contract too
    rest = boot.replace(re.search(r"__osdPublicPath__ = (\{.*?\});", boot, re.S).group(1), "{}")
    rest = rest.replace(block.group(1), "")
    return meta, paths, re.findall(r"'([^']+)'", block.group(1)), rest


def compare(what, ours, theirs, path=""):
    """Every leaf of two structures, said once."""
    if type(ours) is not type(theirs):
        failures.append(f"{what}{path}: ours is {type(ours).__name__}, "
                        f"the reference's is {type(theirs).__name__}")
        return
    if isinstance(theirs, dict):
        for key in sorted(set(ours) | set(theirs)):
            here = f"{path}.{key}" if path else key
            if key in LIVE and not path:
                continue
            if key not in ours:
                failures.append(f"{what}{here}: missing, the reference has "
                                f"{json.dumps(theirs[key])[:70]}")
            elif key not in theirs:
                failures.append(f"{what}{here}: ours has {json.dumps(ours[key])[:70]}, "
                                "the reference has no such field")
            else:
                compare(what, ours[key], theirs[key], here)
        return
    if ours != theirs:
        failures.append(f"{what}{path}:\n      ours      {json.dumps(ours)[:100]}\n"
                        f"      reference {json.dumps(theirs)[:100]}")


def served(url, urls):
    """Which of the files a page names are not actually there."""
    missing = []
    for one in urls:
        try:
            urllib.request.urlopen(url + one, timeout=20).read(1)
        except Exception:
            missing.append(one)
    return missing


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ours", default="http://127.0.0.1:5614")
    ap.add_argument("--reference", default="http://127.0.0.1:5613")
    args = ap.parse_args()
    ours, theirs = args.ours.rstrip("/"), args.reference.rstrip("/")

    our_meta, our_paths, our_bundles, our_boot = shell(ours)
    ref_meta, ref_paths, ref_bundles, ref_boot = shell(theirs)
    if our_boot != ref_boot:
        import difflib
        lines = [l for l in difflib.unified_diff(ref_boot.splitlines(), our_boot.splitlines(),
                                                 "reference bootstrap.js", "our bootstrap.js", lineterm="", n=0)
                 if l.startswith(("+", "-")) and not l.startswith(("+++", "---"))]
        failures.append("bootstrap.js differs from the reference's text:\n      " + "\n      ".join(lines[:12]))

    # the settings somebody has changed are live state, and the overrides are
    # what each server was started with; neither is the contract
    for meta in (our_meta, ref_meta):
        meta.get("legacyMetadata", {}).get("uiSettings", {}).pop("user", None)

    # the plugin list is in load order, which differs between two starts of the
    # same Dashboards; compared by id rather than by position
    ours_by_id = {p["id"]: p for p in our_meta.get("uiPlugins", [])}
    theirs_by_id = {p["id"]: p for p in ref_meta.get("uiPlugins", [])}
    compare("metadata.uiPlugins.", ours_by_id, theirs_by_id)
    our_rest = {k: v for k, v in our_meta.items() if k != "uiPlugins"}
    ref_rest = {k: v for k, v in ref_meta.items() if k != "uiPlugins"}
    compare("metadata.", our_rest, ref_rest)
    compare("publicPath.", our_paths, ref_paths)
    if our_bundles != ref_bundles:
        only_ours = [b for b in our_bundles if b not in ref_bundles]
        only_ref = [b for b in ref_bundles if b not in our_bundles]
        if only_ours or only_ref:
            failures.append(f"the bundle list differs: {len(only_ours)} only ours, "
                            f"{len(only_ref)} only the reference's")
        else:
            # Two starts of the same Dashboards load their plugins in different
            # orders -- measured: two 3.1.0 containers disagree -- so the order
            # is not the contract. Nor does it decide anything: the boot script
            # defines every bundle before it resolves any, and the reference
            # itself loads `usageCollection` before the utils it requires. The
            # set is what has to match.
            print("  (the bundles load in a different order from the reference, which two "
                  "starts of the reference do as well)")

    missing = served(ours, our_bundles + [our_meta["i18n"]["translationsUrl"]])
    if missing:
        failures.append(f"{len(missing)} files our page names are not served: "
                        + ", ".join(missing[:3]))

    print(f"  {len(our_bundles)} bundles, {len(our_meta['uiPlugins'])} plugins, "
          f"{len(our_meta['legacyMetadata']['uiSettings']['defaults'])} setting defaults")
    if not failures:
        print("  the shell our server serves is the shell the reference serves")
        return 0
    print(f"  {len(failures)} differences:")
    for f in failures:
        print("   ", f)
    return 1


if __name__ == "__main__":
    sys.exit(main())
