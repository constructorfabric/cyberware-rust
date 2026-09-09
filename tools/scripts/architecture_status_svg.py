#!/usr/bin/env python3
"""Render the live architecture diagram with per-gear status colours.

This script mirrors the data-access approach of ``back_roadmap_to_xls.py``: it
reads the "Status" of every gear from the GitHub Projects v2 board and then
paints the status icon (a small circle) next to each gear on the architecture
diagram.

Pipeline
--------
1. Load the mapping config (``docs/img/architecture_template.yaml``):
     * ``github``            - org / project number / status field name
     * ``status_map``        - GitHub Status value  -> SVG legend status
     * ``svg_status_styles`` - SVG legend status     -> circle style
     * ``gear_mappings``       - SVG label           -> GitHub issue ID
2. Fetch project statuses and repository issue URLs from GitHub.
3. Resolve each SVG gear label through its configured GitHub issue ID.
4. Copy the template SVG verbatim and, for every gear, locate the *nearest
   status circle to the left of the label* and restyle it to match the status.
5. Write the result to ``docs/img/architecture.drawio.svg``.

Usage
-----
    python tools/scripts/architecture_status_svg.py

GitHub token resolution matches ``back_roadmap_to_xls.py``:
    ~/.constructorfabric/gh_token.txt   or   $GITHUB_TOKEN
"""
from __future__ import annotations

import argparse
import base64
import html
import os
import re
import sys
import urllib.parse
import zlib
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

import requests
import yaml

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_CONFIG = REPO_ROOT / "docs" / "img" / "architecture_template.yaml"
DEFAULT_TEMPLATE = REPO_ROOT / "docs" / "img" / "architecture_template.drawio.svg"
DEFAULT_OUTPUT = REPO_ROOT / "docs" / "img" / "architecture.drawio.svg"

# Legend swatches live at the bottom of the diagram; ignore any circle whose
# centre is below this y so gear icons are never confused with the legend.
LEGEND_Y_THRESHOLD = 740.0

# A status circle sits just to the left of its gear label; horizontally it is
# offset ~30px to the right of the label block's text anchor (the bullet
# indent). We look for the circle column nearest that offset.
CIRCLE_X_OFFSET = 30.0
MAX_COLUMN_DX = 45.0   # how far a circle column may be from the expected offset
LINE_HEIGHT = 19.0     # approximate list line height (used only for y windows)


class UserFacingError(RuntimeError):
    pass


# ---------------------------------------------------------------------------
# GitHub access
# ---------------------------------------------------------------------------
def resolve_github_token() -> Optional[str]:
    token_path = Path("~/.constructorfabric/gh_token.txt").expanduser()
    if token_path.is_file():
        token = token_path.read_text(encoding="utf-8").strip()
        if token:
            return token
    return os.getenv("GITHUB_TOKEN")


_ITEMS_QUERY = """
query($org:String!, $number:Int!, $statusField:String!, $after:String) {
  organization(login:$org) {
    projectV2(number:$number) {
      title
      items(first:100, after:$after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          content { ... on Issue { title url } ... on PullRequest { title url } }
          status: fieldValueByName(name:$statusField) {
            ... on ProjectV2ItemFieldSingleSelectValue { name }
          }
        }
      }
    }
  }
}
"""

_REPO_ISSUES_QUERY = """
query($org:String!, $repo:String!, $after:String) {
  repository(owner:$org, name:$repo) {
    issues(first:100, after:$after, states:[OPEN, CLOSED]) {
      pageInfo { hasNextPage endCursor }
      nodes { title url }
    }
  }
}
"""


def fetch_gear_statuses(
    token: str, org: str, project_number: int, status_field: str
) -> Dict[str, Tuple[str, str]]:
    """Return {issue_title: (status_value, issue_url)} for board items with a status."""
    headers = {"Authorization": f"Bearer {token}", "Content-Type": "application/json"}
    statuses: Dict[str, Tuple[str, str]] = {}
    after: Optional[str] = None
    while True:
        resp = requests.post(
            "https://api.github.com/graphql",
            json={
                "query": _ITEMS_QUERY,
                "variables": {
                    "org": org,
                    "number": project_number,
                    "statusField": status_field,
                    "after": after,
                },
            },
            headers=headers,
            timeout=60,
        )
        resp.raise_for_status()
        payload = resp.json()
        if payload.get("errors"):
            raise UserFacingError(
                "GitHub GraphQL error while reading project "
                f"{org}/#{project_number}:\n"
                + "\n".join(f"- {e.get('message', e)}" for e in payload["errors"])
            )
        project = (((payload.get("data") or {}).get("organization") or {})
                   .get("projectV2"))
        if not project:
            raise UserFacingError(
                f"Could not read project {org}/#{project_number}. "
                "Check the org, the project number, and the token scopes "
                "(needs project + read:org, and repo for private issues)."
            )
        items = project.get("items", {})
        for node in items.get("nodes", []):
            content = node.get("content") or {}
            title = content.get("title")
            status = (node.get("status") or {}).get("name")
            url = content.get("url")
            if title and url:
                # Keep the first status seen for a title (stable across pages).
                statuses.setdefault(title, (status or "", url))
        page = items.get("pageInfo", {})
        if not page.get("hasNextPage"):
            break
        after = page.get("endCursor")
        if not after:
            break
    return statuses


def fetch_repo_issue_urls(token: str, org: str, repo: str) -> Dict[str, str]:
    headers = {"Authorization": f"Bearer {token}", "Content-Type": "application/json"}
    issue_urls: Dict[str, str] = {}
    after: Optional[str] = None
    while True:
        resp = requests.post(
            "https://api.github.com/graphql",
            json={
                "query": _REPO_ISSUES_QUERY,
                "variables": {"org": org, "repo": repo, "after": after},
            },
            headers=headers,
            timeout=60,
        )
        resp.raise_for_status()
        payload = resp.json()
        if payload.get("errors"):
            raise UserFacingError(
                f"GitHub GraphQL error while reading {org}/{repo}:\n"
                + "\n".join(f"- {e.get('message', e)}" for e in payload["errors"])
            )
        issues = (((payload.get("data") or {}).get("repository") or {}).get("issues") or {})
        for issue in issues.get("nodes", []):
            title = issue.get("title")
            url = issue.get("url")
            if title and url:
                issue_urls.setdefault(title, url)
        page = issues.get("pageInfo", {})
        if not page.get("hasNextPage"):
            break
        after = page.get("endCursor")
        if not after:
            break
    return issue_urls


# ---------------------------------------------------------------------------
# Name normalization / matching
# ---------------------------------------------------------------------------
def normalize_gear_name(name: str) -> str:
    """Drop a leading ``PREFIX - `` tag and parenthetical notes, then reduce to
    lowercase alphanumerics for tolerant comparison."""
    text = str(name or "")
    if " - " in text:
        text = text.split(" - ", 1)[1]
    text = re.sub(r"\([^)]*\)", "", text)
    return re.sub(r"[^a-z0-9]", "", text.lower())


def clean_label(fragment: str) -> str:
    """Normalize a gear label taken from HTML: strip tags, unescape entities
    (incl. ``&nbsp;`` -> space), collapse whitespace."""
    text = re.sub(r"<[^>]+>", " ", fragment)
    text = html.unescape(text).replace("\xa0", " ")
    return re.sub(r"\s+", " ", text).strip()


def github_prefix(title: str) -> str:
    """The unit tag in front of a GitHub issue title, e.g. ``CORE`` in
    ``CORE - Auth Resolver``. Empty string when there is no ``PREFIX - ``."""
    return title.split(" - ", 1)[0].strip() if " - " in title else ""


def build_svg_name_to_status(
    svg_names: List[str],
    gh_statuses: Dict[str, Tuple[str, str]],
    gh_issue_urls: Dict[str, str],
    mappings: Dict[str, Dict[str, Any]],
) -> Dict[str, Tuple[str, str, str]]:
    """Map each matched SVG label -> (GitHub issue title, status, URL)."""
    # Index GitHub titles by normalized key (keep the title, not just status).
    issues_by_id = {
        int(url.rsplit("/", 1)[1]): (title, url) for title, url in gh_issue_urls.items()
    }
    statuses_by_url = {url: status for status, url in gh_statuses.values()}
    result: Dict[str, Tuple[str, str, str]] = {}
    for label in svg_names:
        mapping = mappings.get(label) or {}
        issue_id = mapping.get("issue_id")
        if issue_id is None:
            continue
        issue = issues_by_id.get(int(issue_id))
        if issue:
            title, url = issue
            result[label] = (title, statuses_by_url.get(url, ""), url)
    return result


# ---------------------------------------------------------------------------
# SVG parsing
# ---------------------------------------------------------------------------
class GearBlock:
    """A bold 11px ``<ul>`` label block: an anchor plus its ordered gear names."""

    __slots__ = ("x", "y", "names")

    def __init__(self, x: float, y: float, names: List[str]) -> None:
        self.x = x
        self.y = y
        self.names = names


def parse_gear_blocks(svg: str) -> List[GearBlock]:
    """Extract every gear label block with its (x, y) text anchor.

    Gear labels are the bold 11px ``<ul><li>`` foreignObject blocks; each
    ``<li>`` is one gear name. We keep the block anchor (``margin-left`` /
    ``padding-top``) and defer the label->circle mapping to a geometric,
    alignment-agnostic step, because some blocks are top-aligned and others are
    vertically centered on the anchor."""
    blocks: List[GearBlock] = []
    for chunk in re.split(r"(?=<foreignObject)", svg):
        if not chunk.startswith("<foreignObject"):
            continue
        block = chunk.split("</foreignObject>", 1)[0]
        if "font-size: 11px" not in block or "font-weight: bold" not in block:
            continue
        if "<ul>" not in block and "<ul " not in block:
            continue
        m = re.search(r"margin-left:\s*([\d.]+)px", block)
        p = re.search(r"padding-top:\s*([\d.]+)px", block)
        if not m or not p:
            continue
        names: List[str] = []
        for li in re.findall(r"<li>(.*?)</li>", block, re.S):
            text = clean_label(li)
            if text:
                names.append(text)
        if names:
            blocks.append(GearBlock(float(m.group(1)), float(p.group(1)), names))
    return blocks


class StatusCircle:
    __slots__ = ("start", "end", "cx", "cy", "attrs")

    def __init__(self, start: int, end: int, cx: float, cy: float, attrs: str) -> None:
        self.start = start
        self.end = end
        self.cx = cx
        self.cy = cy
        self.attrs = attrs


_ELLIPSE_RE = re.compile(r"<ellipse\b([^>]*?)/>")


def parse_status_circles(svg: str) -> List[StatusCircle]:
    """All status icons: small (~5px radius) ellipses above the legend row.

    A tolerance is used because hand-edited templates can end up with slightly
    off radii (e.g. rx="5.5")."""
    circles: List[StatusCircle] = []
    for m in _ELLIPSE_RE.finditer(svg):
        attrs = m.group(1)
        rx = re.search(r'\brx="([\d.]+)"', attrs)
        ry = re.search(r'\bry="([\d.]+)"', attrs)
        cx = re.search(r'\bcx="([\d.]+)"', attrs)
        cy = re.search(r'\bcy="([\d.]+)"', attrs)
        if not (rx and ry and cx and cy):
            continue
        if abs(float(rx.group(1)) - 5.0) > 1.5 or abs(float(ry.group(1)) - 5.0) > 1.5:
            continue
        cy_val = float(cy.group(1))
        if cy_val >= LEGEND_Y_THRESHOLD:
            continue  # legend swatch, not a gear icon
        circles.append(
            StatusCircle(m.start(), m.end(), float(cx.group(1)), cy_val, attrs)
        )
    return circles


class Section:
    """A section box (Core, Toolkit, Gen AI, Serverless, BSS, OSS)."""

    __slots__ = ("name", "x", "y", "w", "h")

    def __init__(self, name: str, x: float, y: float, w: float, h: float) -> None:
        self.name = name
        self.x = x
        self.y = y
        self.w = w
        self.h = h

    def contains(self, x: float, y: float, tol: float = 16.0) -> bool:
        return (
            self.x - tol <= x <= self.x + self.w + tol
            and self.y - tol <= y <= self.y + self.h + tol
        )

    @property
    def area(self) -> float:
        return self.w * self.h


_RECT_RE = re.compile(r"<rect\b([^>]*?)/>")


def parse_sections(svg: str) -> List[Section]:
    """Parse the section boxes: light-grey rects titled by a bold 16px label."""
    rects: List[Tuple[float, float, float, float]] = []
    for m in _RECT_RE.finditer(svg):
        attrs = m.group(1)
        if 'fill="#f5f5f5"' not in attrs:
            continue

        def _num(key: str) -> Optional[float]:
            mm = re.search(rf'\b{key}="([\d.]+)"', attrs)
            return float(mm.group(1)) if mm else None

        x, y, w, h = _num("x"), _num("y"), _num("width"), _num("height")
        if None in (x, y, w, h):
            continue
        rects.append((x, y, w, h))  # type: ignore[arg-type]

    sections: List[Section] = []
    for chunk in re.split(r"(?=<foreignObject)", svg):
        if not chunk.startswith("<foreignObject"):
            continue
        block = chunk.split("</foreignObject>", 1)[0]
        if "font-size: 16px" not in block:
            continue
        m = re.search(r"margin-left:\s*([\d.]+)px", block)
        p = re.search(r"padding-top:\s*([\d.]+)px", block)
        if not m or not p:
            continue
        name = ""
        for frag in re.findall(r"<div[^>]*>(.*?)</div>", block, re.S):
            text = re.sub(r"<[^>]+>", " ", frag)
            text = re.sub(r"\s+", " ", text).strip()
            if text:
                name = text
        if not name:
            continue
        tx, ty = float(m.group(1)), float(p.group(1))
        for x, y, w, h in rects:
            if x - 2 <= tx <= x + w + 2 and y - 2 <= ty <= y + h + 2:
                sections.append(Section(name, x, y, w, h))
                break
    return sections


def section_for_point(
    x: float, y: float, sections: List[Section]
) -> Optional[str]:
    """Name of the smallest section box that contains the point (if any)."""
    best: Optional[Section] = None
    for sec in sections:
        if sec.contains(x, y) and (best is None or sec.area < best.area):
            best = sec
    return best.name if best else None


def assign_blocks_to_circles(
    blocks: List[GearBlock], circles: List[StatusCircle], used: set
) -> List[Tuple[str, int]]:
    """Map each gear name to the index of its status circle.

    For a block we pick the circle *column* (circles sharing an x) that sits at
    the expected offset to the right of the block anchor, then take the N
    circles in that column closest to the block anchor y (N = number of gear
    names), order them top-to-bottom and zip with the names. This is agnostic
    to whether the block is top-aligned or vertically centered, and it copes
    with a single column feeding two vertically separated blocks."""
    # Group circle indices by rounded x (a "column").
    columns: Dict[int, List[int]] = {}
    for idx, c in enumerate(circles):
        columns.setdefault(round(c.cx), []).append(idx)

    assignments: List[Tuple[str, int]] = []
    for block in blocks:
        n = len(block.names)
        expected_cx = block.x + CIRCLE_X_OFFSET
        window = LINE_HEIGHT * n + 40.0

        # Among all columns near the expected x, pick the one that actually has
        # free circles within the block's y-window. Two blocks can share an
        # x-position but live in different y-bands (and near-identical columns
        # a couple of pixels apart), so x-proximity alone is not enough.
        best_key: Optional[int] = None
        best_score: Tuple[int, float, float] = (0, 0.0, 0.0)
        for key, idxs in columns.items():
            dx = abs(key - expected_cx)
            if dx > MAX_COLUMN_DX:
                continue
            cand = [
                idx
                for idx in idxs
                if idx not in used and abs(circles[idx].cy - block.y) <= window
            ]
            if not cand:
                continue
            cand.sort(key=lambda i: abs(circles[i].cy - block.y))
            take = cand[:n]
            avg = sum(abs(circles[i].cy - block.y) for i in take) / len(take)
            # Prefer more coverage, then circles closer in y, then closer in x.
            score = (len(take), -avg, -dx)
            if score > best_score:
                best_score = score
                best_key = key
        if best_key is None:
            continue  # gear block has no status circle in the template

        candidates = [
            idx
            for idx in columns[best_key]
            if idx not in used and abs(circles[idx].cy - block.y) <= window
        ]
        candidates.sort(key=lambda i: abs(circles[i].cy - block.y))
        chosen = sorted(candidates[:n], key=lambda i: circles[i].cy)
        for name, idx in zip(block.names, chosen):
            used.add(idx)
            assignments.append((name, idx))
    return assignments


def restyle_circle(attrs: str, style: Dict[str, str]) -> str:
    """Rewrite fill/stroke/stroke-dasharray on an <ellipse> attribute string."""
    fill = style["fill"]
    stroke = style.get("stroke", "#000000")
    dasharray = style.get("dasharray")

    # Drop any existing fill / stroke / dasharray, keep everything else.
    attrs = re.sub(r'\s*fill="[^"]*"', "", attrs)
    attrs = re.sub(r'\s*stroke="[^"]*"', "", attrs)
    attrs = re.sub(r'\s*stroke-dasharray="[^"]*"', "", attrs)

    style_str = f' fill="{fill}" stroke="{stroke}"'
    if dasharray:
        style_str += f' stroke-dasharray="{dasharray}"'

    # Insert style right after the geometry attributes (before pointer-events if
    # present, else at the end) to keep the tag readable.
    if "pointer-events=" in attrs:
        attrs = re.sub(r'(\s*pointer-events=)', style_str + r"\1", attrs, count=1)
    else:
        attrs = attrs.rstrip() + style_str
    return f"<ellipse{attrs}/>"


_LABEL_GROUP_RE = re.compile(
    r'<g transform="translate\(-0\.5 -0\.5\)">\s*<switch>.*?</switch>\s*</g>',
    re.S,
)


def _xml_escape(text: str) -> str:
    return text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def render_aligned_labels(
    svg_text: str,
    assignments: List[Tuple[str, int]],
    circles: List["StatusCircle"],
    issue_links: Dict[str, str],
) -> str:
    """Replace the flowed <foreignObject> gear-label lists with real SVG
    <text> anchored to each status circle.

    The template draws gear names as HTML (``<ul><li>``) inside a
    ``<foreignObject>`` while the status icons are fixed-position ``<ellipse>``
    elements. Browsers lay the HTML out with their own line metrics, which do
    not match the circles' fixed 19px spacing, so labels and icons drift apart
    (and drift worse when the SVG is scaled). Rendering the labels as SVG text
    in the same coordinate space as the circles makes them scale together and
    stay aligned in every renderer."""
    # Drop the HTML label groups (bold 11px lists); keep section titles etc.
    def _strip(m: "re.Match[str]") -> str:
        block = m.group(0)
        if "font-size: 11px" in block and "font-weight: bold" in block and "<ul>" in block:
            return ""
        return block

    stripped = _LABEL_GROUP_RE.sub(_strip, svg_text)

    # Build one <text> per gear, vertically centered on its circle.
    parts: List[str] = []
    for name, idx in assignments:
        c = circles[idx]
        x = c.cx + 9.0          # just right of the circle
        y = c.cy + 3.6          # baseline for an 11px glyph centered on cy
        label = (
            f'<text x="{x:.2f}" y="{y:.2f}" fill="#000000" '
            f'font-family="Helvetica, Arial, sans-serif" font-size="11px" '
            f'pointer-events="all">{_xml_escape(name)}</text>'
        )
        link = issue_links.get(name)
        parts.append(
            f'<a xlink:href="{html.escape(link, quote=True)}" target="_blank">'
            f'{label}</a>' if link else label
        )
    labels_svg = "\n        " + "\n        ".join(parts) + "\n    "

    # Inject just before the closing </svg> (root user-space matches the
    # circles' coordinate space).
    idx = stripped.rfind("</svg>")
    if idx == -1:
        return stripped
    return stripped[:idx] + labels_svg + stripped[idx:]


# ---------------------------------------------------------------------------
# Embedded drawio source (mxfile) handling
#
# A `.drawio.svg` carries its editable source in <svg content="&lt;mxfile&gt;...">.
# The drawio editor renders the status circles from that mxfile, so we must
# recolour it too, not just the <ellipse> render layer.
# ---------------------------------------------------------------------------
_MX_ELLIPSE_CELL_RE = re.compile(
    r'<mxCell\b[^>]*?\bstyle="(?P<style>ellipse[^"]*)"[^>]*?>'
    r'\s*<mxGeometry\b[^>]*?\bx="(?P<x>[-\d.]+)"[^>]*?\by="(?P<y>[-\d.]+)"',
    re.S,
)
_MX_LABEL_CELL_RE = re.compile(
    r'<mxCell\b[^>]*?\bvalue="(?P<value>(?:&lt;ul&gt;|<ul>)[^"]*?)"[^>]*?>'
    r'\s*<mxGeometry\b[^>]*?\bx="(?P<x>[-\d.]+)"[^>]*?\by="(?P<y>[-\d.]+)"',
    re.S,
)
# In mxfile coordinates the legend circles sit near the bottom (center_y >= 760).
MX_LEGEND_Y_THRESHOLD = 760.0


def _mx_deflate_raw(text: str) -> bytes:
    compressor = zlib.compressobj(9, zlib.DEFLATED, -15)
    return compressor.compress(text.encode("utf-8")) + compressor.flush()


def _restyle_mxfile_xml(
    xml: str,
    name_to_svg_status: Dict[str, str],
    mx_styles: Dict[str, str],
) -> Tuple[str, int]:
    """Recolour status-circle mxCells in the decoded mxGraphModel XML."""
    # Gear label blocks (edgeLabel cells whose value is a <ul> of gear names).
    blocks: List[GearBlock] = []
    for m in _MX_LABEL_CELL_RE.finditer(xml):
        value = html.unescape(m.group("value"))
        names = []
        for li in re.findall(r"<li>(.*?)</li>", value, re.S):
            text = clean_label(li)
            if text:
                names.append(text)
        if names:
            blocks.append(GearBlock(float(m.group("x")), float(m.group("y")), names))

    # Status circles (10x10 ellipse cells), centre = (x+5, y+5), skip legend.
    circles: List[StatusCircle] = []
    for m in _MX_ELLIPSE_CELL_RE.finditer(xml):
        cx = float(m.group("x")) + 5.0
        cy = float(m.group("y")) + 5.0
        if cy >= MX_LEGEND_Y_THRESHOLD:
            continue
        circles.append(StatusCircle(m.start("style"), m.end("style"), cx, cy, m.group("style")))

    used: set = set()
    assignments = assign_blocks_to_circles(blocks, circles, used)

    edits: List[Tuple[StatusCircle, str]] = []
    for name, idx in assignments:
        svg_status = name_to_svg_status.get(name)
        new_style = mx_styles.get(svg_status) if svg_status else None
        if new_style:
            edits.append((circles[idx], new_style))

    for circle, new_style in sorted(edits, key=lambda e: e[0].start, reverse=True):
        xml = xml[: circle.start] + new_style + xml[circle.end :]
    return xml, len(edits)


def update_embedded_mxfile(
    svg_text: str,
    name_to_svg_status: Dict[str, str],
    mx_styles: Dict[str, str],
) -> Tuple[str, int]:
    """Decode the embedded mxfile, recolour circles, re-encode in place."""
    if not mx_styles:
        return svg_text, 0
    # The base64 diagram payload lives (HTML-escaped) inside the content attr.
    payload_re = re.compile(r"(&gt;)([A-Za-z0-9+/=]{100,})(&lt;/diagram&gt;)")
    m = payload_re.search(svg_text)
    if not m:
        return svg_text, 0
    try:
        raw = zlib.decompress(base64.b64decode(m.group(2)), -15).decode("utf-8")
        xml = urllib.parse.unquote(raw)
    except Exception:
        return svg_text, 0

    new_xml, count = _restyle_mxfile_xml(xml, name_to_svg_status, mx_styles)
    if count == 0:
        return svg_text, 0

    reencoded = base64.b64encode(
        _mx_deflate_raw(urllib.parse.quote(new_xml, safe="~()*!.'"))
    ).decode("ascii")
    updated = svg_text[: m.start(2)] + reencoded + svg_text[m.end(2):]
    return updated, count


# ---------------------------------------------------------------------------
# Orchestration
# ---------------------------------------------------------------------------
def _compile_exclusions(config: Dict[str, Any]) -> Dict[str, Any]:
    excl = config.get("exclusions") or {}
    return {
        "svg_gears": set(excl.get("svg_gears") or []),
        "github_titles": set(excl.get("github_titles") or []),
        "github_prefixes": set(excl.get("github_prefixes") or []),
        "github_patterns": [re.compile(p) for p in (excl.get("github_patterns") or [])],
    }


def _github_excluded(title: str, excl: Dict[str, Any]) -> bool:
    if title in excl["github_titles"]:
        return True
    if github_prefix(title) in excl["github_prefixes"]:
        return True
    return any(pat.search(title) for pat in excl["github_patterns"])


def render(
    config: Dict[str, Any],
    template_svg: str,
    gh_statuses: Dict[str, Tuple[str, str]],
    gh_issue_urls: Dict[str, str],
    verbose: bool = False,
) -> Tuple[str, Dict[str, int], Dict[str, List[str]], List[Dict[str, str]]]:
    status_map: Dict[str, str] = config.get("status_map") or {}
    styles: Dict[str, Dict[str, str]] = config.get("svg_status_styles") or {}
    mx_styles: Dict[str, str] = config.get("mx_status_styles") or {}
    gear_mappings: Dict[str, Dict[str, Any]] = config.get("gear_mappings") or {}
    section_map: Dict[str, str] = config.get("section_map") or {}
    excl = _compile_exclusions(config)

    blocks = parse_gear_blocks(template_svg)
    sections = parse_sections(template_svg)
    all_names = [name for block in blocks for name in block.names]
    svg_names = sorted(set(all_names))
    name_to_gh = build_svg_name_to_status(
        svg_names, gh_statuses, gh_issue_urls, gear_mappings
    )
    issue_links = {name: url for name, (_title, _status, url) in name_to_gh.items()}
    github = config.get("github") or {}
    issue_search_url = "https://github.com/{}/{}/issues?q=is%3Aissue+".format(
        github["org"], github["repo"]
    )
    for name in svg_names:
        issue_links.setdefault(name, issue_search_url + urllib.parse.quote(name))

    # SVG label -> resolved SVG legend status (used for both render layers).
    # Priority: explicit status_overrides win over the GitHub-derived status.
    name_to_svg_status: Dict[str, str] = {}
    for name, (_title, gh_status, _url) in name_to_gh.items():
        svg_status = status_map.get(gh_status)
        if svg_status:
            name_to_svg_status[name] = svg_status
    for name in svg_names:
        forced = (gear_mappings.get(name) or {}).get("status_override")
        if forced:
            name_to_svg_status[name] = forced

    # SVG label -> the section box it is drawn in.
    name_section: Dict[str, Optional[str]] = {}
    for block in blocks:
        sec = section_for_point(block.x, block.y, sections)
        for name in block.names:
            name_section.setdefault(name, sec)

    warnings: Dict[str, List[str]] = {
        "only_in_svg": [],
        "only_in_github": [],
        "section_mismatch": [],
        "missing_issue_link": [],
    }
    # Per-gear calculated status, in diagram reading order.
    report: List[Dict[str, str]] = []
    for block in blocks:
        sec = section_for_point(block.x, block.y, sections) or "-"
        for name in block.names:
            entry = name_to_gh.get(name)
            forced = (gear_mappings.get(name) or {}).get("status_override")
            if forced:
                gh_title = entry[0] if entry else "(config override)"
                gh_status = (entry[1] if entry else "-") + " [override]"
                svg_status = forced
            elif entry is None:
                gh_title, gh_status = "(no GitHub match)", "-"
                svg_status = "Not started (default)"
            else:
                gh_title, gh_status, _url = entry
                svg_status = status_map.get(gh_status) or f"?? (unmapped: {gh_status})"
            report.append({
                "section": sec,
                "name": name,
                "gh_title": gh_title,
                "gh_status": gh_status,
                "svg_status": svg_status,
            })

    # (1) SVG gears with no GitHub counterpart and no manual override.
    for name in svg_names:
        if (
            name not in name_to_gh
            and not (gear_mappings.get(name) or {}).get("status_override")
            and name not in excl["svg_gears"]
        ):
            warnings["only_in_svg"].append(name)

    for name in svg_names:
        if name not in issue_links:
            warnings["missing_issue_link"].append(name)

    # (2) GitHub gears (in a diagram section) not shown on the SVG diagram.
    matched_titles = {title for title, _status, _url in name_to_gh.values()}
    for title in sorted(gh_statuses):
        if title in matched_titles:
            continue
        prefix = github_prefix(title)
        if prefix not in section_map:
            continue  # not a section drawn on the diagram
        if _github_excluded(title, excl):
            continue
        warnings["only_in_github"].append(title)

    # (3) Section mismatch: box the gear is drawn in vs its GitHub unit tag.
    seen_mismatch: set = set()
    for name, (gh_title, _gh_status, _url) in name_to_gh.items():
        svg_section = name_section.get(name)
        gh_section = section_map.get(github_prefix(gh_title))
        if svg_section and gh_section and svg_section != gh_section and name not in seen_mismatch:
            seen_mismatch.add(name)
            warnings["section_mismatch"].append(
                f"{name!r}: drawn in {svg_section!r} but GitHub says "
                f"{gh_section!r} ({gh_title!r})"
            )

    # (A) Recolour the embedded drawio source (what the drawio editor renders).
    result, mx_styled = update_embedded_mxfile(
        template_svg, name_to_svg_status, mx_styles
    )

    # (B) Recolour the rendered <ellipse> layer (what browsers/GitHub render).
    #     Parse circles AFTER the mxfile edit so offsets are valid for `result`.
    circles = parse_status_circles(result)
    used: set = set()
    assignments = assign_blocks_to_circles(blocks, circles, used)

    stats = {
        "labels": len(all_names),
        "circles": len(circles),
        "matched": 0,
        "styled": 0,
        "mx_styled": mx_styled,
        "unmatched_status": 0,
        "no_circle": len(all_names) - len(assignments),
    }
    edits: List[Tuple[StatusCircle, Dict[str, str]]] = []
    for name, idx in assignments:
        svg_status = name_to_svg_status.get(name)
        if svg_status is None:
            stats["unmatched_status"] += 1
            continue  # leave the template default (Not started)
        stats["matched"] += 1
        style = styles.get(svg_status)
        if not style:
            if verbose:
                print(f"[warn] no SVG style for status {svg_status!r} "
                      f"(gear {name!r})", file=sys.stderr)
            continue
        edits.append((circles[idx], style))
        stats["styled"] += 1

    # Apply edits back-to-front so character offsets stay valid.
    for circle, style in sorted(edits, key=lambda e: e[0].start, reverse=True):
        new_tag = restyle_circle(circle.attrs, style)
        result = result[: circle.start] + new_tag + result[circle.end :]

    # Replace flowed HTML gear labels with SVG text anchored to the circles so
    # icons and names stay aligned at any scale / in any renderer.
    result = render_aligned_labels(result, assignments, circles, issue_links)

    return result, stats, warnings, report


def parse_args(argv: Optional[List[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Render docs/img/architecture.drawio.svg with gear "
        "status colours pulled from the GitHub project board."
    )
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--template", type=Path, default=DEFAULT_TEMPLATE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--org", default=None, help="Override github.org from config")
    parser.add_argument(
        "--project", type=int, default=None, help="Override github.project_number"
    )
    parser.add_argument("-v", "--verbose", action="store_true", default=False)
    return parser.parse_args(argv)


def main(argv: Optional[List[str]] = None) -> int:
    args = parse_args(argv)

    if not args.config.exists():
        raise UserFacingError(f"Config not found: {args.config}")
    if not args.template.exists():
        raise UserFacingError(f"Template SVG not found: {args.template}")

    config = yaml.safe_load(args.config.read_text(encoding="utf-8")) or {}
    gh_cfg = config.get("github") or {}
    org = args.org or gh_cfg.get("org")
    repo = gh_cfg.get("repo")
    project_number = args.project or gh_cfg.get("project_number")
    status_field = gh_cfg.get("status_field", "Status")
    if not org or not repo or not project_number:
        raise UserFacingError(
            "github.org, github.repo, and github.project_number are required."
        )

    token = resolve_github_token()
    if not token:
        raise UserFacingError(
            "GitHub token not found. Put it in ~/.constructorfabric/gh_token.txt "
            "or set $GITHUB_TOKEN (scopes: project, read:org, repo)."
        )

    gh_statuses = fetch_gear_statuses(token, org, int(project_number), status_field)
    gh_issue_urls = fetch_repo_issue_urls(token, org, repo)
    if args.verbose:
        print(f"[info] fetched {len(gh_statuses)} board items with a status",
              file=sys.stderr)
        print(f"[info] fetched {len(gh_issue_urls)} repository issues", file=sys.stderr)

    template_svg = args.template.read_text(encoding="utf-8")
    result, stats, warnings, report = render(
        config, template_svg, gh_statuses, gh_issue_urls, verbose=args.verbose
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(result, encoding="utf-8")

    _report_gear_statuses(report)
    _report_warnings(warnings)

    print(
        f"Wrote {args.output} "
        f"(gears={stats['labels']}, circles={stats['circles']}, "
        f"matched={stats['matched']}, svg_styled={stats['styled']}, "
        f"mxfile_styled={stats['mx_styled']}, "
        f"no_status={stats['unmatched_status']}, no_circle={stats['no_circle']})"
    )
    return 0


def _report_gear_statuses(report: List[Dict[str, str]]) -> None:
    """Print the calculated status for every gear, grouped by section."""
    if not report:
        return
    print(f"\nCalculated status for {len(report)} gear(s):")
    name_w = max(len(r["name"]) for r in report)
    ghs_w = max(len(r["gh_status"]) for r in report)
    current_section = None
    for r in sorted(report, key=lambda r: (r["section"], r["name"].lower())):
        if r["section"] != current_section:
            current_section = r["section"]
            print(f"\n  [{current_section}]")
        print(
            f"    {r['name']:<{name_w}}  {r['gh_status']:<{ghs_w}}  ->  "
            f"{r['svg_status']}"
        )


def _report_warnings(warnings: Dict[str, List[str]]) -> None:
    """Print mapping/consistency warnings to stderr."""
    only_svg = warnings["only_in_svg"]
    only_gh = warnings["only_in_github"]
    mismatch = warnings["section_mismatch"]
    missing_issue_links = warnings["missing_issue_link"]
    total = len(only_svg) + len(only_gh) + len(mismatch) + len(missing_issue_links)
    if total == 0:
        return

    print(f"\n{total} mapping warning(s):", file=sys.stderr)
    if only_svg:
        print(
            f"\n[warn] {len(only_svg)} SVG gear(s) with no GitHub board match "
            "(add to gear_mappings or exclusions.svg_gears):",
            file=sys.stderr,
        )
        for name in only_svg:
            print(f"    - {name}", file=sys.stderr)
    if only_gh:
        print(
            f"\n[warn] {len(only_gh)} GitHub gear(s) in a diagram section but not "
            "drawn on the SVG (add the gear to the template or "
            "exclusions.github_*):",
            file=sys.stderr,
        )
        for title in only_gh:
            print(f"    - {title}", file=sys.stderr)
    if missing_issue_links:
        print(
            f"\n[warn] {len(missing_issue_links)} SVG gear(s) without an "
            "issue link (set gear_mappings.<gear>.issue_id):",
            file=sys.stderr,
        )
        for name in missing_issue_links:
            print(f"    - {name}", file=sys.stderr)
    if mismatch:
        print(
            f"\n[warn] {len(mismatch)} gear section mismatch(es) "
            "(SVG box vs GitHub unit tag):",
            file=sys.stderr,
        )
        for line in mismatch:
            print(f"    - {line}", file=sys.stderr)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except UserFacingError as exc:
        print(exc, file=sys.stderr)
        raise SystemExit(1)
