#!/usr/bin/env python3
"""Check book links, cross-links into rustdoc, and bundled manual coverage."""

from html.parser import HTMLParser
from pathlib import Path
import re
import sys
import tomllib
from urllib.parse import unquote, urlsplit


ROOT = Path(__file__).resolve().parents[1]
SITE = ROOT / "target/site"


class Page(HTMLParser):
    def __init__(self, path):
        super().__init__()
        self.ids = set()
        self.links = []
        self.feed(path.read_text())

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if "id" in attrs:
            self.ids.add(attrs["id"])
        if tag == "a" and "name" in attrs:
            self.ids.add(attrs["name"])
        for key in ("href", "src"):
            if value := attrs.get(key):
                self.links.append(value)


def check_manual():
    catalog = (ROOT / "src/manual/mod.rs").read_text()
    articles = set(re.findall(r'include_str!\("articles/([^"/]+\.md)"\)', catalog))
    summary = (ROOT / "docs/src/SUMMARY.md").read_text()
    errors = []
    if not articles:
        errors.append("Could not find the runtime manual catalog")
    for article in sorted(articles):
        wrapper = ROOT / "docs/src/manual" / article
        include = "{{#include ../../../src/manual/articles/" + article + "}}"
        if not wrapper.is_file():
            errors.append(f"Manual article missing from book: {article}")
            continue
        content = wrapper.read_text()
        if content.count(include) != 1 or "Agents have access to this manual too." not in content:
            errors.append(f"Manual needs its full source include and agent-access note: {article}")
        if f"(manual/{article})" not in summary:
            errors.append(f"Manual article missing from navigation: {article}")
    return errors


def check_links():
    config = tomllib.loads((ROOT / "docs/book.toml").read_text())
    prefix = config["output"]["html"]["site-url"]
    pages = {
        path: Page(path)
        for path in SITE.rglob("*.html")
        if not path.is_relative_to(SITE / "api")
    }
    targets = dict(pages)
    errors = []
    for required in ("index.html", "api/myco/index.html", "api/myco_model/index.html", "api/myco_agent/index.html"):
        if not (SITE / required).is_file():
            errors.append(f"Missing entry point: {required}")
    for path, page in pages.items():
        for link in page.links:
            url = urlsplit(link)
            if url.scheme or url.netloc:
                continue
            relative = unquote(url.path)
            if relative.startswith("/"):
                if not relative.startswith(prefix):
                    errors.append(f"{path.relative_to(SITE)}: link outside site prefix: {link}")
                    continue
                target = SITE / relative.removeprefix(prefix)
            else:
                target = path.parent / relative if relative else path
            target = target.resolve()
            if target.is_dir():
                target /= "index.html"
            if not target.is_relative_to(SITE) or not target.is_file():
                errors.append(f"{path.relative_to(SITE)}: missing local target: {link}")
            elif url.fragment and target.suffix == ".html":
                if target not in targets:
                    targets[target] = Page(target)
                if unquote(url.fragment) not in targets[target].ids:
                    errors.append(f"{path.relative_to(SITE)}: missing anchor: {link}")
    return errors, len(pages)


if __name__ == "__main__":
    errors = check_manual()
    link_errors, count = check_links()
    errors.extend(link_errors)
    if errors:
        print("\n".join(sorted(set(errors))), file=sys.stderr)
        sys.exit(1)
    print(f"Checked manual coverage and links from {count} book pages, including API cross-links.")
