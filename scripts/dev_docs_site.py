#!/usr/bin/env python3
"""The contributor handbook as a website: docs/dev/**/*.md -> docs/handbook/<lang>/*.html.

GitHub Pages serves `docs/` with `.nojekyll`, so the Markdown of `docs/dev/` is published
as plain text. This renders it as HTML, in the three handbook languages, with what an
unassisted newcomer needs to find their way:

* **Search** (Ctrl+K / ⌘K) over every section of the current language, from a static
  index built here — no server, nothing sent anywhere.
* **Syntax highlighting** of code and shell blocks, and a **copy** button on each one
  (a leading `$ ` prompt is not copied).
* **Links to the source**: an inline code span naming a file or directory that exists in
  the repository (`crates/adapters/delonix-sdn/src/infra.rs`, `scripts/e2e.sh`, …) links
  to it on GitHub, and every crate section links to the crate's directory.
* **Language switcher** (EN · PT-AO · FR-FR). A page not yet translated falls back to
  English and says so; a translation whose English source changed since it was made says
  that too — the reader is never shown a stale translation as if it were current.

The output is deterministic (no dates, no absolute paths), so CI can require that the
published site is the generated one:

    python3 scripts/dev_docs_site.py            # write docs/handbook/
    python3 scripts/dev_docs_site.py --check    # exit 1 when docs/handbook is stale

Translation freshness is recorded in each translated page's first line:

    <!-- translated-from: architecture.md sha256:<hash> -->

where the hash is of the English page with its generated regions emptied
(`dev_docs.strip_regions`). `--status` lists translations that are missing or behind.
"""

from __future__ import annotations

import argparse
import hashlib
import html
import json
import re
import shutil
import sys
import tempfile
from pathlib import Path

import markdown

sys.path.insert(0, str(Path(__file__).resolve().parent))
import dev_docs  # noqa: E402

ROOT = dev_docs.ROOT
DEV = dev_docs.DEV_DOCS
OUT = ROOT / "docs" / "handbook"
REPO = "https://github.com/angolardevops/delonix-runtime"
LANGS = dev_docs.LANGS
TRANSLATED_FROM = re.compile(r"^<!-- translated-from: (?P<file>\S+) sha256:(?P<sha>[0-9a-f]{64}) -->\n")

UI = {
    "en": {
        "name": "English", "short": "EN", "title": "Contributor handbook", "search": "Search",
        "search_ph": "Search the handbook…", "no_results": "No results", "on_page": "On this page",
        "copy": "Copy", "copied": "Copied", "source": "Source", "view_source": "Source on GitHub",
        "not_translated": "This page is not translated into {lang} yet — showing the English version.",
        "stale": "This translation may be behind the English page, which changed since it was translated.",
        "english": "Read the English page", "edit": "Edit this page", "prev": "Previous", "next": "Next",
        "hint": "to search",
    },
    "pt-AO": {
        "name": "Português (Angola)", "short": "PT-AO", "title": "Manual do contribuidor", "search": "Pesquisar",
        "search_ph": "Pesquisar no manual…", "no_results": "Sem resultados", "on_page": "Nesta página",
        "copy": "Copiar", "copied": "Copiado", "source": "Fonte", "view_source": "Código-fonte no GitHub",
        "not_translated": "Esta página ainda não está traduzida para {lang} — mostra-se a versão inglesa.",
        "stale": "Esta tradução pode estar atrás da página inglesa, que mudou depois de ser traduzida.",
        "english": "Ler a página em inglês", "edit": "Editar esta página", "prev": "Anterior", "next": "Seguinte",
        "hint": "para pesquisar",
    },
    "fr-FR": {
        "name": "Français", "short": "FR-FR", "title": "Guide du contributeur", "search": "Rechercher",
        "search_ph": "Rechercher dans le guide…", "no_results": "Aucun résultat", "on_page": "Sur cette page",
        "copy": "Copier", "copied": "Copié", "source": "Source", "view_source": "Source sur GitHub",
        "not_translated": "Cette page n'est pas encore traduite en {lang} — la version anglaise est affichée.",
        "stale": "Cette traduction peut être en retard sur la page anglaise, modifiée depuis la traduction.",
        "english": "Lire la page en anglais", "edit": "Modifier cette page", "prev": "Précédent", "next": "Suivant",
        "hint": "pour rechercher",
    },
    "zh-CN": {
        "name": "简体中文", "short": "ZH-CN", "title": "贡献者手册", "search": "搜索",
        "search_ph": "搜索手册…", "no_results": "没有结果", "on_page": "本页内容",
        "copy": "复制", "copied": "已复制", "source": "源码", "view_source": "在 GitHub 上查看源码",
        "not_translated": "本页尚未翻译为{lang}，当前显示英文版本。",
        "stale": "英文页面在翻译之后已有改动，本译文可能已经过时。",
        "english": "阅读英文页面", "edit": "编辑本页", "prev": "上一页", "next": "下一页",
        "hint": "搜索",
    },}

SHELL_LANGS = {"bash", "sh", "shell", "console", "zsh"}
REPO_PATH = re.compile(
    r"^(?P<path>(?:crates|bins|scripts|proto|docs|examples|\.github)/[A-Za-z0-9_./-]*[A-Za-z0-9_/-]"
    r"|(?:Cargo\.toml|Cargo\.lock|rust-toolchain\.toml|AGENTS\.md|ARCHITECTURE\.md|CONTRIBUTING\.md|SECURITY\.md|README\.rst|Makefile|Delonixfile|deny\.toml))"
    r"(?P<rest>(?::[A-Za-z0-9_:<>]+)?)$"
)


# The handbook's reading order, in ONE place: each topic builds on the ones before it.
# File names carry no number, so reordering never breaks a link or a translation.
PAGES = [
    ("overview", ["README.md"]),
    ("start", ["start-here.md", "iaas-and-cloud-native.md"]),
    ("foundations", ["linux-foundations.md", "cloud-native-primer.md", "rust-primer.md"]),
    ("setup", ["environment.md", "build-and-test.md"]),
    ("architecture", ["project-structure.md", "architecture.md", "crates.md", "system-design-interview.md"]),
    ("images", ["delonixfile-and-vmfile.md", "microvm-setup.md"]),
    ("contributing", ["coding-conventions.md", "contributing-workflow.md", "publishing-docs.md"]),
    ("reference", ["cloud-native-standards.md", "environment-variables.md", "glossary.md"]),
]


def english_pages() -> list[Path]:
    """The English pages in reading order. A page on disk that is not in PAGES fails:
    an unlisted page would be published without a place in the navigation."""
    ordered = [DEV / name for _, names in PAGES for name in names if (DEV / name).is_file()]
    unlisted = sorted(p.name for p in DEV.glob("*.md") if p not in ordered)
    if unlisted:
        sys.exit(f"dev_docs_site: pages not in PAGES (scripts/dev_docs_site.py): {', '.join(unlisted)}")
    return ordered


def out_name(md_name: str) -> str:
    return "index.html" if md_name == "README.md" else Path(md_name).with_suffix(".html").name


def source_hash(english: Path) -> str:
    return hashlib.sha256(dev_docs.strip_regions(english.read_text()).encode()).hexdigest()


def translation_state(lang: str, english: Path) -> tuple[Path, str]:
    """(file to render, state) where state is `source`, `current`, `stale` or `missing`."""
    if lang == "en":
        return english, "source"
    translated = dev_docs.lang_dir(lang) / english.name
    if not translated.is_file():
        return english, "missing"
    m = TRANSLATED_FROM.match(translated.read_text())
    if not m or m.group("sha") != source_hash(english):
        return translated, "stale"
    return translated, "current"


def title_of(text: str, fallback: str) -> str:
    for line in text.splitlines():
        if line.startswith("# "):
            return re.sub(r"`([^`]*)`", r"\1", line[2:].strip())
    return fallback


def crate_paths() -> dict[str, str]:
    return {c["name"]: c["path"] for c in dev_docs.workspace_crates()}


def link_sources(body: str, crates: dict[str, str]) -> str:
    """Inline code spans that name an existing repository path become links to it."""

    def code(m: re.Match) -> str:
        inner = m.group(1)
        raw = html.unescape(inner)
        pm = REPO_PATH.match(raw)
        if not pm:
            return m.group(0)
        path = pm.group("path").rstrip("/")
        target = ROOT / path
        if not target.exists():
            return m.group(0)
        kind = "tree" if target.is_dir() else "blob"
        return f'<a class="src" href="{REPO}/{kind}/main/{path}" target="_blank" rel="noopener"><code>{inner}</code></a>'

    # Only spans that are not already inside a link or a code block.
    parts = re.split(r"(<pre>.*?</pre>|<a [^>]*>.*?</a>)", body, flags=re.S)
    for i, part in enumerate(parts):
        if not part.startswith(("<pre>", "<a ")):
            parts[i] = re.sub(r"<code>([^<]+)</code>", code, part)
    body = "".join(parts)

    def crate_heading(m: re.Match) -> str:
        name = html.unescape(m.group(3))
        path = crates.get(name)
        if not path:
            return m.group(0)
        badge = f' <a class="crate-src" href="{REPO}/tree/main/{path}" target="_blank" rel="noopener">{{SOURCE}}</a>'
        return m.group(1) + m.group(2) + m.group(4) + badge + m.group(5)

    return re.sub(r"(<h[23] id=\"[^\"]*\">)(<code>([a-z0-9-]+)</code>)(.*?)(</h[23]>)", crate_heading, body)


def rewrite_links(body: str, page_dir: Path) -> str:
    def fix(m: re.Match) -> str:
        href = m.group(1)
        if re.match(r"^(https?:|mailto:|#)", href):
            return m.group(0)
        path, _, frag = href.partition("#")
        frag = f"#{frag}" if frag else ""
        target = (page_dir / path).resolve()
        if target.suffix == ".md" and target.parent in {DEV.resolve(), *(dev_docs.lang_dir(l).resolve() for l in LANGS)}:
            return f'href="{out_name(target.name)}{frag}"'
        try:
            rel = target.relative_to(ROOT).as_posix()
        except ValueError:
            return m.group(0)
        kind = "tree" if target.is_dir() else "blob"
        return f'href="{REPO}/{kind}/main/{rel}{frag}" target="_blank" rel="noopener"'

    return re.sub(r'href="([^"]+)"', fix, body)


FENCE = re.compile(r"^(?P<indent>[ \t]*)(?P<fence>```+|~~~+)[ \t]*(?P<lang>[A-Za-z0-9_+-]*)[^\n]*\n(?P<code>.*?)^(?P=indent)(?P=fence)[ \t]*$", re.M | re.S)


def stash_fences(text: str) -> tuple[str, dict[str, str]]:
    """Take every fenced block out before Markdown sees it, and leave a placeholder.

    Python-Markdown's `fenced_code` ignores a fence nested in a list item (GitHub
    renders it), which silently turned the code examples of whole pages into prose.
    Rendering the blocks here makes both behave the same wherever the fence sits.
    """
    blocks: dict[str, str] = {}

    def take(m: re.Match) -> str:
        indent = m.group("indent")
        code = "".join(line[len(indent):] if line.startswith(indent) else line
                       for line in m.group("code").splitlines(keepends=True))
        key = f"DLXFENCE{len(blocks)}X"
        lang = m.group("lang").lower()
        if lang == "mermaid":
            blocks[key] = f'<pre class="mermaid">{html.escape(code)}</pre>'
        else:
            cls = f' class="language-{"bash" if lang in SHELL_LANGS else lang}"' if lang else ' class="nohighlight"'
            shell = " data-shell" if lang in SHELL_LANGS else ""
            label = html.escape("shell" if lang in SHELL_LANGS else (lang or "text"))
            blocks[key] = (f'<div class="code"{shell}><div class="code-head"><span class="lang">{label}</span>'
                           f'<button class="copy" type="button">{ICON_COPY}<span>{{COPY}}</span></button></div>'
                           f'<pre><code{cls}>{html.escape(code)}</code></pre></div>')
        return f"{indent}{key}\n"

    return FENCE.sub(take, text), blocks


def restore_fences(body: str, blocks: dict[str, str]) -> str:
    for key, block in blocks.items():
        body = re.sub(rf"<p>{key}</p>|{key}", lambda _m, b=block: b, body, count=1)
    return body


HEADING_ID = re.compile(r'<h([1-3]) id="([^"]*)">')


def english_heading_ids(english: Path) -> list[str]:
    stashed, _ = stash_fences(english.read_text())
    return [m.group(2) for m in HEADING_ID.finditer(markdown.markdown(stashed, extensions=["tables", "toc", "sane_lists", "attr_list"]))]


def align_anchors(body: str, toc: str, english_ids: list[str]) -> tuple[str, str]:
    """Give a translated page the English heading ids.

    Anchors are slugs of the headings, so translating a heading would break every
    `page.md#anchor` link written in any language, and the language switcher could
    not keep the reader's position. Translations keep the English heading structure,
    so ids are matched by position; when the counts differ nothing is changed, and
    `--status` reports the page as structurally out of step."""
    ids = [m.group(2) for m in HEADING_ID.finditer(body)]
    if len(ids) != len(english_ids) or ids == english_ids:
        return body, toc
    counter = iter(english_ids)
    body = HEADING_ID.sub(lambda m: f'<h{m.group(1)} id="{next(counter)}">', body)
    mapping = dict(zip(ids, english_ids))
    toc = re.sub(r'href="#([^"]*)"', lambda m: f'href="#{mapping.get(m.group(1), m.group(1))}"', toc)
    return body, toc


def post_process(body: str) -> str:
    body = re.sub(
        r'<pre><code class="language-mermaid">(.*?)</code></pre>',
        lambda m: f'<pre class="mermaid">{m.group(1)}</pre>',
        body,
        flags=re.S,
    )

    def block(m: re.Match) -> str:
        lang = m.group(1) or ""
        cls = f' class="language-{"bash" if lang in SHELL_LANGS else lang}"' if lang else ' class="nohighlight"'
        shell = " data-shell" if lang in SHELL_LANGS else ""
        label = html.escape("shell" if lang in SHELL_LANGS else (lang or "text"))
        return (f'<div class="code"{shell}><div class="code-head"><span class="lang">{label}</span>'
                f'<button class="copy" type="button">{ICON_COPY}<span>{{COPY}}</span></button></div>'
                f'<pre><code{cls}>{m.group(2)}</code></pre></div>')

    body = re.sub(r'<pre><code(?: class="language-([A-Za-z0-9_+-]+)")?>(.*?)</code></pre>', block, body, flags=re.S)
    return body.replace("<table>", '<div class="tablewrap"><table>').replace("</table>", "</table></div>")


def sections(body: str, title: str, url: str) -> list[dict]:
    """Search records: one per h1/h2/h3 section, with its plain text."""
    records = []
    pieces = re.split(r'(<h[123] id="[^"]*">.*?</h[123]>)', body, flags=re.S)
    heading, anchor = title, ""
    for piece in pieces:
        hm = re.match(r'<h[123] id="([^"]*)">(.*?)</h[123]>', piece, flags=re.S)
        if hm:
            anchor = hm.group(1)
            heading = html.unescape(re.sub(r"<[^>]+>", "", hm.group(2))).replace("{SOURCE}", "").strip()
            continue
        text = re.sub(r'<pre class="mermaid">.*?</pre>|<div class="code-head">.*?</div>', " ", piece, flags=re.S)
        text = html.unescape(re.sub(r"<[^>]+>", " ", text))
        text = re.sub(r"\s+", " ", text).strip()
        if text or anchor:
            records.append({"p": title, "h": heading, "u": f"{url}#{anchor}" if anchor else url, "t": text[:1500]})
    return records


GROUP_NAMES = {
    "en": {"overview": "Overview", "start": "Get started", "foundations": "Foundations", "setup": "Set up & build",
           "architecture": "Architecture", "images": "Images & microVMs", "contributing": "Contributing", "reference": "Reference"},
    "pt-AO": {"overview": "Visão geral", "start": "Começar", "foundations": "Fundamentos", "setup": "Preparar e compilar",
              "architecture": "Arquitectura", "images": "Imagens e microVMs", "contributing": "Contribuir", "reference": "Referência"},
    "fr-FR": {"overview": "Vue d'ensemble", "start": "Démarrer", "foundations": "Fondamentaux", "setup": "Préparer et compiler",
              "architecture": "Architecture", "images": "Images et microVM", "contributing": "Contribuer", "reference": "Référence"},
    "zh-CN": {"overview": "概览", "start": "入门", "foundations": "基础", "setup": "环境与构建",
              "architecture": "架构", "images": "镜像与 microVM", "contributing": "贡献", "reference": "参考"},
}
CHROME = {
    "en": {"theme": "Toggle colour theme", "menu": "Menu", "github": "GitHub", "home": "Handbook home"},
    "pt-AO": {"theme": "Alternar tema de cor", "menu": "Menu", "github": "GitHub", "home": "Início do manual"},
    "fr-FR": {"theme": "Changer le thème de couleur", "menu": "Menu", "github": "GitHub", "home": "Accueil du guide"},
    "zh-CN": {"theme": "切换配色主题", "menu": "菜单", "github": "GitHub", "home": "手册首页"},
}
DIALOGUE = {"Interviewer": "interviewer", "Entrevistador": "interviewer", "Recruteur": "interviewer", "面试官": "interviewer",
            "Candidate": "candidate", "Candidato": "candidate", "Candidat": "candidate", "候选人": "candidate"}


def page_number(name: str) -> int:
    """Position in the reading order (README is the unnumbered overview)."""
    flat = [n for _, names in PAGES for n in names if (DEV / n).is_file()]
    return flat.index(name) if name in flat and name != "README.md" else -1


def group_of(name: str) -> str:
    return next(key for key, names in PAGES if name in names)


def soften_table_paths(body: str) -> str:
    """Let a long path in a table cell wrap after `/`, `::` or `.`, instead of forcing the
    table wider than the column or breaking a name in the middle."""
    def cell(m: re.Match) -> str:
        # A `|` inside code in a table cell is written `\|` so GitHub keeps the column;
        # the Markdown renderer here keeps the backslash visible, so it is dropped.
        m = re.match(r"(?s).*", re.sub(r"(<code>[^<]*?)\\\|", lambda c: c.group(1) + "|", m.group(0)))
        return re.sub(r"<code>([^<]{24,})</code>",
                      lambda c: "<code>" + re.sub(r"(/|::|\.)", r"\1<wbr>", c.group(1)) + "</code>", m.group(0))
    return re.sub(r"<td>.*?</td>", cell, body, flags=re.S)


def decorate(body: str) -> str:
    """Heading anchors, interview dialogue and the code-block header — the shape of the
    content, added after Markdown so the source stays plain."""
    def anchor(m: re.Match) -> str:
        level, hid, inner = m.group(1), m.group(2), m.group(3)
        if level == "1":
            return m.group(0)
        return f'<h{level} id="{hid}">{inner}<a class="hanchor" href="#{hid}" aria-label="#">#</a></h{level}>'

    body = re.sub(r'<h([23]) id="([^"]*)">(.*?)</h\1>', anchor, body, flags=re.S)

    speaker_p = re.compile(r'<p><strong>([^<]+?)\s*(?:&nbsp;|\u00a0)?:?</strong>\s*(?:&nbsp;|\u00a0)?:?\s*')

    def dialogue(m: re.Match) -> str:
        inner = m.group(1)
        starts = [s for s in speaker_p.finditer(inner) if DIALOGUE.get(html.unescape(s.group(1)).replace("\u00a0", " ").strip().rstrip("：:").strip())]
        if not starts or inner[: starts[0].start()].strip():
            return m.group(0)
        turns = []
        for k, s in enumerate(starts):
            end = starts[k + 1].start() if k + 1 < len(starts) else len(inner)
            name = html.unescape(s.group(1)).replace("\u00a0", " ").strip().rstrip("：:").strip()
            rest = inner[s.end():end]
            turns.append(f'<blockquote class="say say-{DIALOGUE[name]}"><p><span class="speaker">{html.escape(name)}</span>{rest}</blockquote>')
        return "".join(turns)

    body = re.sub(r"<blockquote>(.*?)</blockquote>", dialogue, body, flags=re.S)
    return body


CSS = r"""
:root{
  /* The NgolaCloud design system (delonix-web packages/ui/src/tokens.css):
     light theme by default, dark as the secondary theme. */
  --bg:#fcfaf8;--surface:#f5f3f1;--surface-2:#eeeae8;--card:#ffffff;--ink:#191513;--ink-2:#524b49;--muted:#675e59;
  --line:#e1ddda;--line-soft:#ece8e5;
  --accent:#cc2823;--accent-ink:#b70000;--accent-soft:#ffe2dc;--focus:#cc2823;
  --code-bg:#1a1614;--code-ink:#ede7e3;--code-line:#2e2825;--code-label:#a39993;
  --warn-bg:rgba(239,168,49,.16);--warn-line:#c9820f;--warn-ink:#6b4400;
  --topbar:rgba(255,255,255,.82);
  --shadow:0 4px 12px rgba(38,28,22,.08),0 1px 3px rgba(38,28,22,.06);
  --f-display:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,"PingFang SC","Noto Sans SC","Microsoft YaHei",sans-serif;
  --f-body:var(--f-display);
  --f-mono:"JetBrains Mono",ui-monospace,"SF Mono","Cascadia Mono",Menlo,Consolas,monospace;
  --r-sm:4px;--r:6px;--r-lg:8px;
  --header:60px;color-scheme:light;
}
@media (prefers-color-scheme: dark){:root:not([data-theme="light"]){
  --bg:#0b0b0c;--surface:#131315;--surface-2:#19191c;--card:#161618;--ink:#ededee;--ink-2:#b6b6ba;--muted:#87878c;
  --line:#262629;--line-soft:#1d1d20;
  --accent:#ee5a24;--accent-ink:#ff6c39;--accent-soft:rgba(238,90,36,.18);--focus:#ee5a24;
  --code-bg:#101012;--code-ink:#dedee2;--code-line:#26262a;--code-label:#87878c;
  --warn-bg:rgba(226,161,58,.12);--warn-line:#8a6a2a;--warn-ink:#e2c07a;
  --topbar:rgba(11,11,12,.82);
  --shadow:0 8px 24px rgba(0,0,0,.5),0 1px 4px rgba(0,0,0,.4);color-scheme:dark;
}}
:root[data-theme="dark"]{
  --bg:#0b0b0c;--surface:#131315;--surface-2:#19191c;--card:#161618;--ink:#ededee;--ink-2:#b6b6ba;--muted:#87878c;
  --line:#262629;--line-soft:#1d1d20;
  --accent:#ee5a24;--accent-ink:#ff6c39;--accent-soft:rgba(238,90,36,.18);--focus:#ee5a24;
  --code-bg:#101012;--code-ink:#dedee2;--code-line:#26262a;--code-label:#87878c;
  --warn-bg:rgba(226,161,58,.12);--warn-line:#8a6a2a;--warn-ink:#e2c07a;
  --topbar:rgba(11,11,12,.82);
  --shadow:0 8px 24px rgba(0,0,0,.5),0 1px 4px rgba(0,0,0,.4);color-scheme:dark;
}
*{box-sizing:border-box}
html{scroll-padding-top:calc(var(--header) + 16px)}
body{margin:0;background:var(--bg);color:var(--ink);font:16px/1.7 var(--f-body);-webkit-font-smoothing:antialiased;text-rendering:optimizeLegibility}
a{color:var(--accent-ink);text-decoration:none}
a:hover{text-decoration:underline;text-underline-offset:3px}
:focus-visible{outline:2px solid var(--focus);outline-offset:2px;border-radius:4px}
@media (prefers-reduced-motion: reduce){*{scroll-behavior:auto!important;transition:none!important}}

/* Header */
.top{position:sticky;top:0;z-index:30;height:var(--header);display:flex;align-items:center;gap:16px;
  padding-inline:20px;background:var(--topbar);backdrop-filter:blur(10px);border-bottom:1px solid var(--line)}
.brand{display:flex;align-items:center;gap:10px;color:var(--ink);font:700 16px/1 var(--f-display);white-space:nowrap}
.brand:hover{text-decoration:none}
.brand .mark{width:26px;height:26px;border-radius:var(--r);background:var(--accent);display:grid;place-items:center;color:#fff;font:800 13px/1 var(--f-display)}
.brand .sub{font:500 14px/1 var(--f-body);color:var(--muted)}
.brand .sep{width:1px;height:18px;background:var(--line)}
.searchbtn{flex:1;max-width:460px;margin-inline:auto;display:flex;align-items:center;gap:10px;height:38px;padding-inline:12px;
  border:1px solid var(--line);border-radius:10px;background:var(--surface);color:var(--muted);font:15px var(--f-body);cursor:pointer;min-width:0}
.searchbtn:hover{border-color:color-mix(in srgb,var(--accent) 45%,var(--line))}
.searchbtn svg{flex:none}
.searchbtn .label{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.searchbtn kbd{margin-left:auto}
kbd{font:600 11px/1 var(--f-mono);border:1px solid var(--line);border-bottom-width:2px;border-radius:5px;padding:3px 6px;background:var(--bg);color:var(--muted);white-space:nowrap}
.actions{display:flex;align-items:center;gap:6px}
select.lang{height:36px;border:1px solid var(--line);border-radius:9px;background:var(--bg);color:var(--ink);font:14px var(--f-body);padding-inline:8px}
.iconbtn{height:36px;width:36px;display:grid;place-items:center;border:1px solid var(--line);border-radius:9px;background:var(--bg);color:var(--ink-2);cursor:pointer}
.iconbtn:hover{color:var(--accent-ink);border-color:color-mix(in srgb,var(--accent) 45%,var(--line));text-decoration:none}
.menubtn{display:none}

/* Frame */
.frame{display:grid;grid-template-columns:280px minmax(0,1fr) 240px}
nav.side{position:sticky;top:var(--header);height:calc(100vh - var(--header));overflow-y:auto;padding:24px 16px 48px 20px;border-right:1px solid var(--line)}
nav.side,aside.toc,.sres{scrollbar-width:thin;scrollbar-color:var(--line) transparent}
nav.side .group{margin-bottom:18px}
nav.side .gtitle{font:700 11px/1 var(--f-display);letter-spacing:.09em;text-transform:uppercase;color:var(--muted);padding:0 10px 8px}
nav.side a{display:flex;gap:10px;align-items:baseline;padding:6px 10px;border-radius:8px;color:var(--ink-2);font-size:15px;line-height:1.35}
nav.side a:hover{background:var(--surface);text-decoration:none;color:var(--ink)}
nav.side a .n{flex:none;width:18px;font:500 12px/1.35 var(--f-mono);color:var(--muted);font-variant-numeric:tabular-nums}
nav.side a.on{background:var(--accent-soft);color:var(--accent-ink);font-weight:600}
nav.side a.on .n{color:var(--accent-ink)}
main{min-width:0;padding:40px clamp(20px,3.5vw,56px) 80px}
.content{max-width:1120px;margin-inline:auto}

aside.toc{position:sticky;top:var(--header);height:calc(100vh - var(--header));overflow-y:auto;padding:40px 20px 40px 8px;font-size:13.5px}
aside.toc .ttitle{font:700 11px/1 var(--f-display);letter-spacing:.09em;text-transform:uppercase;color:var(--muted);margin:0 0 12px}
aside.toc ul{list-style:none;margin:0;padding:0}
aside.toc ul ul{padding-left:12px}
aside.toc li{margin:0}
aside.toc a{display:block;padding:4px 0 4px 12px;border-left:2px solid var(--line);color:var(--muted);line-height:1.4}
aside.toc a:hover{color:var(--ink);text-decoration:none}
aside.toc a.active{color:var(--accent-ink);border-left-color:var(--accent);font-weight:600}

/* Type */
.kicker{font:700 12px/1 var(--f-display);letter-spacing:.09em;text-transform:uppercase;color:var(--accent-ink);margin:0 0 12px}
h1,h2,h3,h4{font-family:var(--f-display);color:var(--ink);text-wrap:balance;letter-spacing:-.01em}
h1{font-size:2.35rem;line-height:1.15;font-weight:800;margin:0 0 20px}
h2{font-size:1.55rem;line-height:1.25;font-weight:700;margin:56px 0 14px}
h3{font-size:1.18rem;line-height:1.3;font-weight:700;margin:36px 0 10px}
h4{font-size:1rem;margin:28px 0 8px}
h2 code,h3 code{font-size:.9em}
.hanchor{margin-left:8px;color:var(--muted);opacity:0;font-weight:500;text-decoration:none}
h2:hover .hanchor,h3:hover .hanchor,.hanchor:focus-visible{opacity:1}
p,ul,ol{margin:0 0 16px}
li{margin:4px 0}li>p{margin:0 0 8px}
strong{font-weight:650;color:var(--ink)}
hr{border:0;height:1px;background:var(--line);margin:40px 0}
code{font:.86em/1.5 var(--f-mono);background:var(--surface-2);border:1px solid var(--line);padding:.08em .38em;border-radius:6px;overflow-wrap:break-word}
a code{color:inherit}
a.src code{border-color:color-mix(in srgb,var(--accent) 30%,var(--line));background:var(--accent-soft)}
a.src:hover{text-decoration:none}a.src:hover code{border-color:var(--accent)}
.crate-src{margin-left:10px;vertical-align:middle;font:600 11px/1 var(--f-display);letter-spacing:.04em;text-transform:uppercase;
  border:1px solid var(--line);border-radius:999px;padding:5px 9px;color:var(--muted)}
.crate-src:hover{color:var(--accent-ink);border-color:var(--accent);text-decoration:none}

/* Blocks */
blockquote{margin:20px 0;padding:2px 0 2px 18px;border-left:3px solid var(--line);color:var(--ink-2)}
blockquote p:last-child{margin-bottom:0}
blockquote.say{border:1px solid var(--line-soft);padding:14px 18px;border-radius:var(--r-lg);background:var(--card);margin:18px 0}
blockquote.say .speaker{display:inline-block;margin-right:8px;font:700 11px/1 var(--f-display);letter-spacing:.08em;text-transform:uppercase;
  padding:5px 8px;border-radius:999px;vertical-align:1px}
blockquote.say-interviewer .speaker{background:var(--surface-2);color:var(--muted);border:1px solid var(--line)}
blockquote.say-candidate{background:var(--accent-soft)}
blockquote.say-candidate .speaker{background:var(--accent);color:#fff}
.notice{display:flex;gap:10px;align-items:baseline;background:var(--warn-bg);border:1px solid var(--warn-line);color:var(--warn-ink);
  border-radius:12px;padding:12px 16px;margin:0 0 28px;font-size:15px}
.notice a{color:inherit;font-weight:600;text-decoration:underline}
.code{margin:18px 0 22px;border-radius:var(--r-lg);background:var(--code-bg);border:1px solid var(--code-line);overflow:hidden}
.code-head{display:flex;align-items:center;justify-content:space-between;gap:8px;height:36px;padding:0 8px 0 14px;border-bottom:1px solid var(--code-line)}
.code-head .lang{font:600 11px/1 var(--f-mono);letter-spacing:.06em;text-transform:uppercase;color:var(--code-label)}
.copy{display:flex;align-items:center;gap:6px;height:26px;padding:0 9px;border:1px solid var(--code-line);border-radius:7px;background:transparent;
  color:var(--code-label);font:600 12px var(--f-body);cursor:pointer}
.copy:hover{color:#fff;border-color:#3a4555}
.copy.done{color:#7ee2a8;border-color:#2f5d45}
.code pre{margin:0;padding:14px 16px 22px;overflow-x:auto;color:var(--code-ink);font:13.5px/1.6 var(--f-mono);
  scrollbar-width:thin;scrollbar-color:#3a4555 transparent;color-scheme:dark}
.code pre::-webkit-scrollbar{height:8px}.code pre::-webkit-scrollbar-thumb{background:#3a4555;border-radius:8px}
.code pre code{background:none;border:0;padding:0;font:inherit;color:inherit;overflow-wrap:normal}
.code pre code.hljs{background:none;padding:0}
pre.mermaid{margin:22px 0;padding:18px;border:1px solid var(--line);border-radius:var(--r-lg);background:var(--card);text-align:center;overflow-x:auto;font-family:var(--f-body)}
.tablewrap{margin:18px 0 24px;overflow-x:auto;border:1px solid var(--line);border-radius:var(--r-lg);background:var(--card)}
table{border-collapse:collapse;width:100%;font-size:14.5px;line-height:1.5}
th,td{padding:10px 14px;text-align:left;vertical-align:top;border-bottom:1px solid var(--line)}
th{background:var(--surface);font:700 12.5px/1.3 var(--f-display);letter-spacing:.02em;color:var(--ink-2);white-space:nowrap}
tbody tr:last-child td{border-bottom:0}
tbody tr:hover td{background:color-mix(in srgb,var(--surface) 60%,transparent)}
td code{font-size:.82em;overflow-wrap:normal}
/* A table's first column names the thing (a variable, a crate, a flag): never break it. */
td:first-child code{white-space:nowrap}
th:first-child,td:first-child{white-space:nowrap}

/* Pager + footer */
.pager{display:grid;grid-template-columns:1fr 1fr;gap:14px;margin-top:64px}
.pager a{display:block;padding:14px 16px;border:1px solid var(--line);border-radius:12px;color:var(--ink);font:600 15px/1.35 var(--f-display)}
.pager a:hover{border-color:var(--accent);text-decoration:none}
.pager a span{display:block;font:500 12px/1 var(--f-body);color:var(--muted);margin-bottom:6px}
.pager .next{text-align:right;grid-column:2}
.foot{margin-top:28px;padding-top:18px;border-top:1px solid var(--line);display:flex;flex-wrap:wrap;gap:8px 16px;font-size:13.5px;color:var(--muted)}

/* Search */
#search{position:fixed;inset:0;z-index:60;background:rgba(10,13,18,.45);display:flex;align-items:flex-start;justify-content:center;padding:12vh 16px 16px}
#search[hidden]{display:none}
.sbox{width:min(680px,100%);background:var(--bg);border:1px solid var(--line);border-radius:16px;box-shadow:var(--shadow);overflow:hidden}
.sfield{display:flex;align-items:center;gap:10px;padding:0 16px;border-bottom:1px solid var(--line);color:var(--muted)}
.sfield input{flex:1;height:56px;border:0;outline:0;background:transparent;color:var(--ink);font:17px var(--f-body)}
.sres{list-style:none;margin:0;padding:8px;max-height:56vh;overflow-y:auto}
.sres a{display:block;padding:10px 12px;border-radius:10px;color:var(--ink)}
.sres a:hover,.sres a.sel{background:var(--accent-soft);text-decoration:none}
.sres .sp{display:block;font:600 11px/1 var(--f-display);letter-spacing:.06em;text-transform:uppercase;color:var(--muted);margin-bottom:5px}
.sres .sh{font:600 15px/1.35 var(--f-display)}
.sres .st{display:block;margin-top:3px;font-size:13.5px;color:var(--muted);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.sres mark{background:none;color:var(--accent-ink);font-weight:700}
.sempty{padding:18px;color:var(--muted)}
.shelp{display:flex;gap:16px;padding:10px 16px;border-top:1px solid var(--line);font-size:12.5px;color:var(--muted)}

/* Narrow screens */
@media (max-width:1280px){.frame{grid-template-columns:256px minmax(0,1fr)}aside.toc{display:none}}
@media (max-width:860px){
  .frame{display:block}.menubtn{display:grid}.brand .sub,.brand .sep{display:none}
  nav.side{position:fixed;top:var(--header);left:0;bottom:0;width:min(300px,86vw);height:auto;z-index:40;background:var(--bg);
    transform:translateX(-102%);transition:transform .2s ease;box-shadow:var(--shadow)}
  body.nav-open nav.side{transform:none}
  main{padding:28px 16px 64px}
  h1{font-size:1.9rem}h2{font-size:1.35rem}
  .searchbtn kbd,.searchbtn .label{display:none}.searchbtn{flex:none;width:38px;justify-content:center;margin-inline:auto 0;padding:0}
  .pager{grid-template-columns:1fr}.pager .next{grid-column:1}
  .top{gap:8px;padding-inline:12px}
  .brand{font-size:15px;min-width:0}
  select.lang{width:78px;padding-inline:4px;text-overflow:ellipsis}
}
@media (max-width:480px){
  .actions .ghbtn{display:none}
  .brand .word{display:none}
}
"""

JS = r"""
(function(){
  const ui = window.HANDBOOK_UI;
  const root = document.documentElement;

  // Theme: system by default; the toggle cycles light/dark and remembers the choice.
  function effectiveDark(){ const t = root.getAttribute('data-theme'); return t ? t === 'dark' : matchMedia('(prefers-color-scheme: dark)').matches; }
  document.querySelectorAll('.themebtn').forEach(function(b){
    b.addEventListener('click', function(){
      const next = effectiveDark() ? 'light' : 'dark';
      root.setAttribute('data-theme', next);
      try { localStorage.setItem('delonix-handbook-theme', next); } catch (e) {}
      renderMermaid();
    });
  });

  // Mobile navigation drawer.
  document.querySelectorAll('.menubtn').forEach(function(b){
    b.addEventListener('click', function(){ document.body.classList.toggle('nav-open'); });
  });
  document.querySelectorAll('nav.side a').forEach(function(a){ a.addEventListener('click', function(){ document.body.classList.remove('nav-open'); }); });

  // Copy buttons live in each code block's header; shell blocks drop a leading "$ ".
  document.querySelectorAll('.code').forEach(function(box){
    const btn = box.querySelector('.copy'); if (!btn) return;
    btn.addEventListener('click', function(){
      let text = box.querySelector('pre code').innerText;
      if (box.hasAttribute('data-shell')) text = text.split('\n').map(function(l){ return l.replace(/^\$ /, ''); }).join('\n');
      navigator.clipboard.writeText(text.replace(/\n$/, '')).then(function(){
        btn.lastChild.textContent = ui.copied; btn.classList.add('done');
        setTimeout(function(){ btn.lastChild.textContent = ui.copy; btn.classList.remove('done'); }, 1500);
      });
    });
  });
  if (window.hljs) document.querySelectorAll('.code pre code:not(.nohighlight)').forEach(function(el){ hljs.highlightElement(el); });

  // Mermaid follows the effective theme, and re-renders when it changes.
  const diagrams = Array.prototype.slice.call(document.querySelectorAll('pre.mermaid'));
  diagrams.forEach(function(d){ d.setAttribute('data-src', d.textContent); });
  function renderMermaid(){
    if (!window.mermaid || !diagrams.length) return;
    diagrams.forEach(function(d){ d.removeAttribute('data-processed'); d.textContent = d.getAttribute('data-src'); });
    mermaid.initialize({startOnLoad:false, securityLevel:'strict', theme: effectiveDark() ? 'dark' : 'neutral',
      fontFamily:getComputedStyle(document.body).fontFamily,
      themeVariables:{fontSize:'17px'},
      flowchart:{useMaxWidth:true, nodeSpacing:28, rankSpacing:44, padding:10},
      sequence:{useMaxWidth:true}});
    mermaid.run({nodes: diagrams});
  }
  renderMermaid();
  matchMedia('(prefers-color-scheme: dark)').addEventListener('change', function(){ if (!root.getAttribute('data-theme')) renderMermaid(); });

  // "On this page": highlight the section being read.
  const tocLinks = Array.prototype.slice.call(document.querySelectorAll('aside.toc a'));
  const byId = {}; tocLinks.forEach(function(a){ byId[decodeURIComponent(a.getAttribute('href').slice(1))] = a; });
  const heads = Array.prototype.slice.call(document.querySelectorAll('.content h2[id], .content h3[id]')).filter(function(h){ return byId[h.id]; });
  function spy(){
    let current = heads.length ? heads[0] : null;
    const line = 120;
    heads.forEach(function(h){ if (h.getBoundingClientRect().top <= line) current = h; });
    tocLinks.forEach(function(a){ a.classList.remove('active'); });
    if (current) byId[current.id].classList.add('active');
  }
  if (heads.length){ document.addEventListener('scroll', spy, {passive:true}); spy(); }

  // Search (Ctrl+K / Cmd+K / "/"): a static index of this language, scored in the browser.
  const modal = document.getElementById('search'), input = modal.querySelector('input'), list = modal.querySelector('.sres');
  let selected = 0;
  function norm(s){ return s.toLowerCase().normalize('NFD').replace(/[̀-ͯ]/g, ''); }
  const index = (window.HANDBOOK_INDEX || []).map(function(r){ return Object.assign({nh: norm(r.h), np: norm(r.p), nt: norm(r.t)}, r); });
  function esc(s){ return s.replace(/[&<>"]/g, function(c){ return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]; }); }
  function mark(s, terms){ let out = esc(s); terms.forEach(function(t){ if (t.length > 1) out = out.replace(new RegExp('(' + t.replace(/[.*+?^${}()|[\]\\]/g, '\\$&') + ')', 'ig'), '<mark>$1</mark>'); }); return out; }
  function snippet(r, terms){ const i = terms.length ? r.nt.indexOf(terms[0]) : -1; const start = Math.max(0, i - 50); return (start ? '…' : '') + r.t.slice(start, start + 170); }
  function run(){
    const terms = norm(input.value).split(/\s+/).filter(Boolean);
    if (!terms.length){ list.innerHTML = ''; return; }
    const hits = [];
    index.forEach(function(r){
      let score = 0;
      for (const t of terms){ const s = (r.nh.includes(t) ? 10 : 0) + (r.np.includes(t) ? 3 : 0) + (r.nt.includes(t) ? 1 : 0); if (!s) return; score += s; }
      hits.push([score, r]);
    });
    hits.sort(function(a, b){ return b[0] - a[0]; });
    selected = 0;
    list.innerHTML = hits.length ? hits.slice(0, 30).map(function(h, i){
      const r = h[1];
      return '<li><a href="' + esc(r.u) + '"' + (i === 0 ? ' class="sel"' : '') + '><span class="sp">' + esc(r.p) + '</span>' +
        '<span class="sh">' + mark(r.h, terms) + '</span><span class="st">' + mark(snippet(r, terms), terms) + '</span></a></li>';
    }).join('') : '<li class="sempty">' + esc(ui.no_results) + '</li>';
  }
  function open(){ modal.hidden = false; input.value = ''; list.innerHTML = ''; input.focus(); }
  function close(){ modal.hidden = true; }
  document.querySelectorAll('.searchbtn').forEach(function(b){ b.addEventListener('click', open); });
  modal.addEventListener('click', function(e){ if (e.target === modal) close(); });
  input.addEventListener('input', run);
  input.addEventListener('keydown', function(e){
    const links = list.querySelectorAll('a');
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp'){
      e.preventDefault(); if (!links.length) return;
      links[selected].classList.remove('sel');
      selected = (selected + (e.key === 'ArrowDown' ? 1 : links.length - 1)) % links.length;
      links[selected].classList.add('sel'); links[selected].scrollIntoView({block:'nearest'});
    } else if (e.key === 'Enter' && links.length){ e.preventDefault(); close(); location.href = links[selected].getAttribute('href'); }
  });
  document.addEventListener('keydown', function(e){
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k'){ e.preventDefault(); modal.hidden ? open() : close(); }
    else if (e.key === 'Escape'){ if (!modal.hidden) close(); document.body.classList.remove('nav-open'); }
    else if (e.key === '/' && modal.hidden && !/input|textarea|select/i.test(document.activeElement.tagName)){ e.preventDefault(); open(); }
  });
  document.querySelectorAll('select.lang').forEach(function(s){ s.addEventListener('change', function(){ location.href = s.value + location.hash; }); });
})();
"""

HLJS = "https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0"
HL_LANGS = ("rust", "bash", "yaml", "json", "ini", "dockerfile", "python", "protobuf", "diff", "makefile", "xml")
FONTS = "https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;500;600&display=swap"
ICON_SEARCH = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" aria-hidden="true"><circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/></svg>'
ICON_THEME = '<svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 3a9 9 0 0 0 0 18z" fill="currentColor"/></svg>'
ICON_GITHUB = '<svg width="17" height="17" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M12 .5a11.5 11.5 0 0 0-3.64 22.41c.58.1.79-.25.79-.56v-2c-3.2.7-3.87-1.37-3.87-1.37-.53-1.33-1.28-1.69-1.28-1.69-1.05-.72.08-.7.08-.7 1.16.08 1.77 1.19 1.77 1.19 1.03 1.77 2.7 1.26 3.36.96.1-.75.4-1.26.73-1.55-2.56-.29-5.25-1.28-5.25-5.68 0-1.26.45-2.28 1.19-3.09-.12-.29-.52-1.46.11-3.04 0 0 .97-.31 3.17 1.18a11 11 0 0 1 5.77 0c2.2-1.49 3.17-1.18 3.17-1.18.63 1.58.23 2.75.11 3.04.74.81 1.19 1.83 1.19 3.09 0 4.41-2.7 5.38-5.27 5.67.41.36.78 1.06.78 2.14v3.17c0 .31.21.67.8.56A11.5 11.5 0 0 0 12 .5z"/></svg>'
ICON_MENU = '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" aria-hidden="true"><path d="M4 7h16M4 12h16M4 17h16"/></svg>'
ICON_COPY = '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V6a2 2 0 0 1 2-2h9"/></svg>'


def render_site(out: Path) -> None:
    english = english_pages()
    crates = crate_paths()
    for lang in LANGS:
        ui = UI[lang]
        chrome = CHROME[lang]
        lang_out = out / lang
        lang_out.mkdir(parents=True, exist_ok=True)
        docs = []
        for page in english:
            src, state = translation_state(lang, page)
            text = TRANSLATED_FROM.sub("", src.read_text(), count=1)
            docs.append((page, src, state, text, title_of(text, page.stem)))
        index_records: list[dict] = []
        for i, (page, src, state, text, title) in enumerate(docs):
            md = markdown.Markdown(
                extensions=["tables", "fenced_code", "toc", "sane_lists", "attr_list"],
                extension_configs={"toc": {"toc_depth": "2-3"}},
            )
            stashed, blocks = stash_fences(text)
            body = md.convert(stashed)
            body = restore_fences(post_process(rewrite_links(body, src.parent)), blocks)
            toc = md.toc
            if src != page:
                body, toc = align_anchors(body, toc, english_heading_ids(page))
            body = link_sources(body, crates).replace("{SOURCE}", html.escape(ui["source"]))
            url = out_name(page.name)
            index_records += sections(body, title, url)
            body = soften_table_paths(decorate(body)).replace("{COPY}", html.escape(ui["copy"]))

            notice = ""
            if state == "missing":
                notice = f'<div class="notice" role="note">{html.escape(ui["not_translated"].format(lang=ui["name"]))}</div>'
            elif state == "stale":
                notice = (f'<div class="notice" role="note"><span>{html.escape(ui["stale"])} '
                          f'<a href="../en/{url}">{html.escape(ui["english"])}</a></span></div>')

            nav_parts = []
            for key, _names in PAGES:
                items = [d for d in docs if group_of(d[0].name) == key]
                if not items:
                    continue
                links = []
                for d in items:
                    number = page_number(d[0].name)
                    label = re.sub(r"^\d+\.\s*", "", d[4])
                    n = f'<span class="n">{number:02d}</span>' if number >= 0 else '<span class="n">·</span>'
                    current = ' class="on" aria-current="page"' if d[0] == page else ""
                    links.append(f'<a{current} href="{out_name(d[0].name)}">{n}<span>{html.escape(label)}</span></a>')
                nav_parts.append(f'<div class="group"><div class="gtitle">{html.escape(GROUP_NAMES[lang][key])}</div>{"".join(links)}</div>')
            nav = "".join(nav_parts)

            options = "".join(
                f'<option value="../{l}/{url}"{" selected" if l == lang else ""}>{UI[l]["short"]} · {html.escape(UI[l]["name"])}</option>'
                for l in LANGS
            )
            prev_link = (f'<a href="{out_name(docs[i-1][0].name)}"><span>← {html.escape(ui["prev"])}</span>{html.escape(docs[i-1][4])}</a>'
                         if i else "<span></span>")
            next_link = (f'<a class="next" href="{out_name(docs[i+1][0].name)}"><span>{html.escape(ui["next"])} →</span>{html.escape(docs[i+1][4])}</a>'
                         if i + 1 < len(docs) else "")
            rel_src = src.relative_to(ROOT).as_posix()
            hl = "".join(f'<script src="{HLJS}/languages/{l}.min.js"></script>' for l in HL_LANGS)
            kicker = html.escape(GROUP_NAMES[lang][group_of(page.name)])
            doc = f"""<!doctype html>
<html lang="{lang}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{html.escape(title)} · Delonix Runtime — {html.escape(ui["title"])}</title>
<script>try{{var t=localStorage.getItem('delonix-handbook-theme');if(t)document.documentElement.setAttribute('data-theme',t)}}catch(e){{}}</script>
<link rel="preconnect" href="https://fonts.googleapis.com"><link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" href="{FONTS}">
<link rel="stylesheet" href="{HLJS}/styles/github-dark.min.css">
<style>{CSS}</style></head>
<body>
<header class="top">
  <button class="iconbtn menubtn" type="button" aria-label="{html.escape(chrome["menu"])}">{ICON_MENU}</button>
  <a class="brand" href="index.html" aria-label="{html.escape(chrome["home"])}"><span class="mark">D</span><span class="word">Delonix Runtime</span><span class="sep"></span><span class="sub">{html.escape(ui["title"])}</span></a>
  <button class="searchbtn" type="button">{ICON_SEARCH}<span class="label">{html.escape(ui["search_ph"])}</span><kbd>Ctrl K</kbd></button>
  <div class="actions">
    <select class="lang" id="lang" aria-label="Language">{options}</select>
    <button class="iconbtn themebtn" type="button" aria-label="{html.escape(chrome["theme"])}" title="{html.escape(chrome["theme"])}">{ICON_THEME}</button>
    <a class="iconbtn ghbtn" href="{REPO}" target="_blank" rel="noopener" aria-label="{chrome["github"]}" title="{chrome["github"]}">{ICON_GITHUB}</a>
  </div>
</header>
<div class="frame">
<nav class="side" aria-label="{html.escape(ui["title"])}">{nav}</nav>
<main><div class="content">
<p class="kicker">{kicker}</p>
{notice}{body}
<nav class="pager">{prev_link}{next_link}</nav>
<footer class="foot"><a href="{REPO}/blob/main/{rel_src}" target="_blank" rel="noopener">{html.escape(ui["edit"])}</a><code>{rel_src}</code></footer>
</div></main>
<aside class="toc"><p class="ttitle">{html.escape(ui["on_page"])}</p>{toc}</aside>
</div>
<div id="search" hidden><div class="sbox" role="dialog" aria-modal="true" aria-label="{html.escape(ui["search"])}">
<div class="sfield">{ICON_SEARCH}<input id="search-input" type="search" placeholder="{html.escape(ui["search_ph"])}" autocomplete="off"><kbd>Esc</kbd></div>
<ul class="sres"></ul>
<div class="shelp"><span><kbd>↑</kbd> <kbd>↓</kbd></span><span><kbd>Enter</kbd></span></div></div></div>
<script>window.HANDBOOK_UI={json.dumps({k: ui[k] for k in ("copy", "copied", "no_results")}, ensure_ascii=False)};</script>
<script src="search-index.js"></script>
<script src="{HLJS}/highlight.min.js"></script>{hl}
<script src="https://cdn.jsdelivr.net/npm/mermaid@11.4.1/dist/mermaid.min.js"></script>
<script>{JS}</script>
</body></html>
"""
            (lang_out / url).write_text(doc)
        (lang_out / "search-index.js").write_text(
            "window.HANDBOOK_INDEX=" + json.dumps(index_records, ensure_ascii=False, separators=(",", ":")) + ";\n"
        )
    # The entry page picks the reader's language; without JavaScript it links all three.
    (out / "index.html").write_text(
        '<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">'
        '<title>Delonix Runtime — Contributor handbook</title>'
        '<script>var l=(navigator.languages||[navigator.language||""]).join(",").toLowerCase();'
        'location.replace(/(^|,)pt/.test(l)?"pt-AO/index.html":/(^|,)fr/.test(l)?"fr-FR/index.html":"en/index.html");</script>'
        '<body style="font:16px/1.6 system-ui,sans-serif;padding:24px">'
        '<p><a href="en/index.html">Contributor handbook (English)</a> · <a href="pt-AO/index.html">Manual do contribuidor (Português de Angola)</a> · '
        '<a href="fr-FR/index.html">Guide du contributeur (Français)</a></p></body></html>\n'
    )


def markdown_tables(text: str) -> list[tuple[int, int]]:
    """(rows, cells) for each pipe table in a Markdown page, outside fenced blocks and
    generated-region-agnostic (regions are regenerated before rendering)."""
    stashed, _ = stash_fences(text)
    tables, rows, cells = [], 0, 0
    for line in stashed.splitlines() + [""]:
        s = line.strip()
        if s.startswith("|") and s.endswith("|") and len(s) > 1:
            if re.fullmatch(r"\|?\s*:?-{3,}:?\s*(\|\s*:?-{3,}:?\s*)*\|?", s):
                continue  # the header separator is not a row
            rows += 1
            cells += len(re.split(r"(?<!\\)\|", s.strip("|")))
        elif rows:
            tables.append((rows, cells))
            rows, cells = 0, 0
    return tables


def html_tables(page_html: str) -> list[tuple[int, int]]:
    body = page_html.split("<main>", 1)[-1].split("</main>", 1)[0]
    out = []
    for table in re.findall(r"<table>.*?</table>", body, flags=re.S):
        out.append((len(re.findall(r"<tr>", table)), len(re.findall(r"<t[hd][ >]", table))))
    return out


def table_problems(out: Path) -> list[str]:
    """Every table the reader sees must have exactly the rows and cells its Markdown has:
    the site may restyle a table, never add, drop or merge what it says."""
    problems = []
    for lang in LANGS:
        for page in english_pages():
            src, _ = translation_state(lang, page)
            want = markdown_tables(TRANSLATED_FROM.sub("", src.read_text(), count=1))
            got = html_tables((out / lang / out_name(page.name)).read_text())
            if want != got:
                problems.append(f"{lang}/{out_name(page.name)}: Markdown tables {want} != HTML tables {got}")
    return problems


def status() -> int:
    behind = 0
    for page in english_pages():
        for lang in LANGS[1:]:
            _, state = translation_state(lang, page)
            if state != "current":
                behind += 1
                print(f"{lang:6} {state:8} {page.name}")
            elif len(english_heading_ids(dev_docs.lang_dir(lang) / page.name)) != len(english_heading_ids(page)):
                behind += 1
                print(f"{lang:6} headings {page.name} (heading structure differs from English: anchors cannot be aligned)")
    print(f"dev_docs_site: {behind} translation(s) missing or behind" if behind else "dev_docs_site: all translations current")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--check", action="store_true", help="fail when docs/handbook differs from the generated site")
    parser.add_argument("--status", action="store_true", help="list translations that are missing or behind English")
    args = parser.parse_args()
    if args.status:
        return status()
    if not args.check:
        if OUT.exists():
            shutil.rmtree(OUT)
        render_site(OUT)
        problems = table_problems(OUT)
        if problems:
            print("dev_docs_site: rendered tables differ from their Markdown:")
            for problem in problems:
                print(f"  {problem}")
            return 1
        print(f"dev_docs_site: wrote {OUT.relative_to(ROOT)}/")
        return 0
    with tempfile.TemporaryDirectory() as tmp:
        fresh = Path(tmp) / "handbook"
        render_site(fresh)
        problems = table_problems(fresh)
        if problems:
            print("dev_docs_site: rendered tables differ from their Markdown:")
            for problem in problems:
                print(f"  {problem}")
            return 1
        differ = []
        new = {p.relative_to(fresh) for p in fresh.rglob("*") if p.is_file()}
        old = {p.relative_to(OUT) for p in OUT.rglob("*") if p.is_file()} if OUT.exists() else set()
        for rel in sorted(new | old):
            a, b = fresh / rel, OUT / rel
            if not (a.is_file() and b.is_file() and a.read_bytes() == b.read_bytes()):
                differ.append(rel)
        if differ:
            print("dev_docs_site: docs/handbook is not the generated site — run `python3 scripts/dev_docs_site.py` and commit:")
            for rel in differ[:20]:
                print(f"  docs/handbook/{rel}")
            return 1
    print("dev_docs_site: docs/handbook matches docs/dev")
    return 0


if __name__ == "__main__":
    sys.exit(main())
