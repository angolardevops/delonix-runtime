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

    <!-- translated-from: 05-architecture.md sha256:<hash> -->

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
}

SHELL_LANGS = {"bash", "sh", "shell", "console", "zsh"}
REPO_PATH = re.compile(
    r"^(?P<path>(?:crates|bins|scripts|proto|docs|examples|\.github)/[A-Za-z0-9_./-]*[A-Za-z0-9_/-]"
    r"|(?:Cargo\.toml|Cargo\.lock|rust-toolchain\.toml|AGENTS\.md|ARCHITECTURE\.md|CONTRIBUTING\.md|SECURITY\.md|README\.rst|Makefile|Delonixfile|deny\.toml))"
    r"(?P<rest>(?::[A-Za-z0-9_:<>]+)?)$"
)


def english_pages() -> list[Path]:
    pages = sorted(DEV.glob("*.md"), key=lambda p: (p.name != "README.md", p.name))
    return pages


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
        return f'<div class="code"{shell}><pre><code{cls}>{m.group(2)}</code></pre></div>'

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
        text = re.sub(r'<pre class="mermaid">.*?</pre>', " ", piece, flags=re.S)
        text = html.unescape(re.sub(r"<[^>]+>", " ", text))
        text = re.sub(r"\s+", " ", text).strip()
        if text or anchor:
            records.append({"p": title, "h": heading, "u": f"{url}#{anchor}" if anchor else url, "t": text[:1500]})
    return records


CSS = r"""
:root{--accent:#e8590c;--accent-soft:#fff0e6;--ink:#1a1a2e;--muted:#5a6472;--line:#e6e8ec;--bg:#fff;--side:#f7f8fa;
--code-bg:#0f172a;--code-ink:#e2e8f0;--inline:#f1f3f5;--warn:#fff7db;--warn-line:#e0b400;--shadow:0 12px 40px rgba(0,0,0,.18)}
@media (prefers-color-scheme: dark){:root{--ink:#e6e8ee;--muted:#9aa4b2;--line:#252a33;--bg:#0d1117;--side:#10151c;
--accent-soft:#2a1810;--code-bg:#161b22;--code-ink:#dbe2ea;--inline:#1b212b;--warn:#2a2410;--warn-line:#8a6d00;--shadow:0 12px 40px rgba(0,0,0,.6)}}
*{box-sizing:border-box}body{margin:0;font:16px/1.65 -apple-system,'Segoe UI',Roboto,Ubuntu,sans-serif;color:var(--ink);background:var(--bg)}
a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}
.layout{display:grid;grid-template-columns:290px minmax(0,1fr) 240px;max-width:1640px;margin:0 auto}
nav.side{background:var(--side);border-right:1px solid var(--line);padding:1.1rem 1rem 3rem;position:sticky;top:0;height:100vh;overflow-y:auto}
.brand{font-weight:700;font-size:1.05rem}.subtitle{color:var(--muted);font-size:.82rem;margin-bottom:.8rem}
.tools{display:flex;gap:.4rem;margin-bottom:1rem;flex-wrap:wrap}
.searchbtn{flex:1;display:flex;align-items:center;gap:.4rem;border:1px solid var(--line);background:var(--bg);color:var(--muted);
border-radius:8px;padding:.35rem .55rem;font:inherit;font-size:.85rem;cursor:pointer;min-width:0}
kbd{font:600 .72rem ui-monospace,monospace;border:1px solid var(--line);border-bottom-width:2px;border-radius:5px;padding:.05rem .3rem;background:var(--side);color:var(--muted)}
.searchbtn kbd{margin-left:auto}
select.lang{border:1px solid var(--line);background:var(--bg);color:var(--ink);border-radius:8px;padding:.3rem .4rem;font:inherit;font-size:.85rem}
nav.side a{display:block;padding:.3rem .55rem;border-radius:6px;color:var(--ink);font-size:.92rem}
nav.side a:hover{background:var(--accent-soft);text-decoration:none}nav.side a.on{background:var(--accent-soft);color:var(--accent);font-weight:600}
main{padding:2rem 3rem 5rem;min-width:0}
aside.toc{position:sticky;top:0;height:100vh;overflow-y:auto;padding:2rem 1rem;font-size:.84rem;border-left:1px solid var(--line)}
aside.toc h5{margin:0 0 .5rem;text-transform:uppercase;letter-spacing:.08em;font-size:.7rem;color:var(--muted)}
aside.toc ul{list-style:none;padding-left:.7rem;margin:0}aside.toc>div>ul{padding-left:0}aside.toc a{color:var(--muted)}aside.toc li{margin:.2rem 0}
h1{font-size:2rem;line-height:1.25;margin-top:0}h2{margin-top:2.4rem;padding-bottom:.3rem;border-bottom:1px solid var(--line)}h3{margin-top:1.8rem}
h2,h3{scroll-margin-top:1rem}
code{font:.87em ui-monospace,SFMono-Regular,Menlo,monospace;background:var(--inline);padding:.1em .35em;border-radius:5px}
a.src code{color:var(--accent);text-decoration:underline dotted}
.crate-src{font-size:.72rem;font-weight:600;vertical-align:middle;border:1px solid var(--accent);border-radius:99px;padding:.05rem .5rem;margin-left:.4rem}
.code{position:relative;margin:1rem 0}
.code pre{background:var(--code-bg);color:var(--code-ink);padding:2.1rem 1.1rem 1rem;border-radius:10px;overflow-x:auto;line-height:1.5;margin:0;color-scheme:dark;scrollbar-width:thin;scrollbar-color:#475569 transparent}
.code pre::-webkit-scrollbar{height:8px}.code pre::-webkit-scrollbar-thumb{background:#475569;border-radius:8px}.code pre::-webkit-scrollbar-track{background:transparent}
.code pre code{background:none;padding:0;color:inherit;font-size:.86em}.code pre code.hljs{background:none;padding:0}
.copy{position:absolute;top:.45rem;right:.45rem;border:1px solid #334155;background:#1e293b;color:#e2e8f0;border-radius:6px;
font:600 .72rem -apple-system,sans-serif;padding:.2rem .5rem;cursor:pointer;opacity:.75}.copy:hover,.copy:focus{opacity:1}
pre.mermaid{background:var(--side);color:var(--ink);border:1px solid var(--line);border-radius:10px;padding:1rem;text-align:center;overflow-x:auto}
.tablewrap{overflow-x:auto;margin:1rem 0}table{border-collapse:collapse;font-size:.9rem;width:100%}
th,td{border:1px solid var(--line);padding:.45rem .6rem;vertical-align:top;text-align:left}th{background:var(--side)}
blockquote{margin:1rem 0;padding:.6rem 1rem;border-left:4px solid var(--accent);background:var(--accent-soft)}
.notice{background:var(--warn);border:1px solid var(--warn-line);border-radius:8px;padding:.6rem .9rem;margin-bottom:1.4rem;font-size:.92rem}
.pager{display:flex;justify-content:space-between;gap:1rem;margin-top:3rem;border-top:1px solid var(--line);padding-top:1rem;font-size:.92rem}
.pager span{color:var(--muted);font-size:.78rem;display:block}.pager .next{text-align:right;margin-left:auto}
.foot{font-size:.8rem;color:var(--muted);margin-top:1.5rem}
#search{position:fixed;inset:0;background:rgba(0,0,0,.35);display:flex;align-items:flex-start;justify-content:center;padding:10vh 1rem;z-index:50}
#search[hidden]{display:none}
.sbox{width:min(680px,100%);background:var(--bg);border:1px solid var(--line);border-radius:12px;box-shadow:var(--shadow);overflow:hidden}
.sbox input{width:100%;border:0;border-bottom:1px solid var(--line);padding:1rem 1.1rem;font:inherit;font-size:1.05rem;background:var(--bg);color:var(--ink);outline:none}
.sres{max-height:60vh;overflow-y:auto;margin:0;padding:.3rem;list-style:none}
.sres a{display:block;padding:.55rem .7rem;border-radius:8px;color:var(--ink)}.sres a:hover,.sres a.sel{background:var(--accent-soft);text-decoration:none}
.sres .sp{font-size:.74rem;color:var(--muted)}.sres .sh{font-weight:600}.sres .st{font-size:.82rem;color:var(--muted);display:block;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.sres mark{background:none;color:var(--accent);font-weight:700}.sempty{padding:1rem;color:var(--muted)}
.shelp{display:flex;gap:1rem;padding:.5rem 1rem;border-top:1px solid var(--line);font-size:.75rem;color:var(--muted)}
@media (max-width:1100px){.layout{grid-template-columns:250px minmax(0,1fr)}aside.toc{display:none}}
@media (max-width:760px){.layout{display:block}nav.side{position:static;height:auto}main{padding:1.2rem 1rem}}
"""

JS = r"""
(function(){
  const ui = window.HANDBOOK_UI;
  // Copy buttons: shell blocks drop a leading "$ " prompt so the copied text runs as is.
  document.querySelectorAll('.code').forEach(function(box){
    const btn = document.createElement('button');
    btn.className = 'copy'; btn.type = 'button'; btn.textContent = ui.copy;
    btn.addEventListener('click', function(){
      let text = box.querySelector('code').innerText;
      if (box.hasAttribute('data-shell')) text = text.split('\n').map(function(l){ return l.replace(/^\$ /, ''); }).join('\n');
      navigator.clipboard.writeText(text.replace(/\n$/, '')).then(function(){
        btn.textContent = ui.copied; setTimeout(function(){ btn.textContent = ui.copy; }, 1400);
      });
    });
    box.appendChild(btn);
  });
  if (window.hljs) document.querySelectorAll('.code pre code:not(.nohighlight)').forEach(function(el){ hljs.highlightElement(el); });
  if (window.mermaid) mermaid.initialize({startOnLoad:true, securityLevel:'strict',
    theme: matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'default'});

  // Search (Ctrl+K / Cmd+K): a static index of this language, scored in the browser.
  const modal = document.getElementById('search'), input = modal.querySelector('input'), list = modal.querySelector('.sres');
  let selected = 0;
  function norm(s){ return s.toLowerCase().normalize('NFD').replace(/[̀-ͯ]/g, ''); }
  const index = (window.HANDBOOK_INDEX || []).map(function(r){ return Object.assign({nh: norm(r.h), np: norm(r.p), nt: norm(r.t)}, r); });
  function esc(s){ return s.replace(/[&<>"]/g, function(c){ return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]; }); }
  function mark(s, terms){ let out = esc(s); terms.forEach(function(t){ if (t.length > 1) out = out.replace(new RegExp('(' + t.replace(/[.*+?^${}()|[\]\\]/g, '\\$&') + ')', 'ig'), '<mark>$1</mark>'); }); return out; }
  function snippet(r, terms){
    const i = terms.length ? r.nt.indexOf(terms[0]) : -1;
    const start = Math.max(0, i - 50);
    return (start ? '…' : '') + r.t.slice(start, start + 160);
  }
  function run(){
    const terms = norm(input.value).split(/\s+/).filter(Boolean);
    if (!terms.length){ list.innerHTML = ''; return; }
    const hits = [];
    index.forEach(function(r){
      let score = 0;
      for (const t of terms){
        const s = (r.nh.includes(t) ? 10 : 0) + (r.np.includes(t) ? 3 : 0) + (r.nt.includes(t) ? 1 : 0);
        if (!s) return; score += s;
      }
      hits.push([score, r]);
    });
    hits.sort(function(a, b){ return b[0] - a[0]; });
    selected = 0;
    list.innerHTML = hits.length ? hits.slice(0, 30).map(function(h, i){
      const r = h[1];
      return '<li><a href="' + r.u + '"' + (i === 0 ? ' class="sel"' : '') + '><span class="sp">' + esc(r.p) + '</span><br>' +
        '<span class="sh">' + mark(r.h, terms) + '</span><span class="st">' + mark(snippet(r, terms), terms) + '</span></a></li>';
    }).join('') : '<li class="sempty">' + ui.no_results + '</li>';
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
    else if (e.key === 'Escape' && !modal.hidden) close();
    else if (e.key === '/' && modal.hidden && !/input|textarea|select/i.test(document.activeElement.tagName)){ e.preventDefault(); open(); }
  });
  document.querySelectorAll('select.lang').forEach(function(s){
    s.addEventListener('change', function(){ location.href = s.value; });
  });
})();
"""

HLJS = "https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0"
HL_LANGS = ("rust", "bash", "yaml", "json", "ini", "dockerfile", "python", "protobuf", "diff", "makefile", "xml")


def render_site(out: Path) -> None:
    english = english_pages()
    crates = crate_paths()
    for lang in LANGS:
        ui = UI[lang]
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
            body = md.convert(text)
            body = post_process(rewrite_links(body, src.parent))
            body = link_sources(body, crates).replace("{SOURCE}", html.escape(ui["source"]))
            url = out_name(page.name)
            index_records += sections(body, title, url)

            notice = ""
            if state == "missing":
                notice = f'<div class="notice">{html.escape(ui["not_translated"].format(lang=ui["name"]))}</div>'
            elif state == "stale":
                notice = (f'<div class="notice">{html.escape(ui["stale"])} '
                          f'<a href="../en/{url}">{html.escape(ui["english"])}</a></div>')

            nav = "\n".join(
                f'<a class="{"on" if d[0] == page else ""}" href="{out_name(d[0].name)}">{html.escape(d[4])}</a>' for d in docs
            )
            options = "".join(
                f'<option value="../{l}/{url}"{" selected" if l == lang else ""}>{UI[l]["short"]} · {html.escape(UI[l]["name"])}</option>'
                for l in LANGS
            )
            prev_link = (f'<a href="{out_name(docs[i-1][0].name)}"><span>← {ui["prev"]}</span>{html.escape(docs[i-1][4])}</a>'
                         if i else "")
            next_link = (f'<a class="next" href="{out_name(docs[i+1][0].name)}"><span>{ui["next"]} →</span>{html.escape(docs[i+1][4])}</a>'
                         if i + 1 < len(docs) else "")
            rel_src = src.relative_to(ROOT).as_posix()
            hl = "".join(f'<script src="{HLJS}/languages/{l}.min.js"></script>' for l in HL_LANGS)
            html_lang = {"en": "en", "pt-AO": "pt-AO", "fr-FR": "fr-FR"}[lang]
            doc = f"""<!doctype html>
<html lang="{html_lang}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{html.escape(title)} · Delonix Runtime — {html.escape(ui["title"])}</title>
<link rel="stylesheet" href="{HLJS}/styles/github-dark.min.css">
<style>{CSS}</style></head>
<body><div class="layout">
<nav class="side"><div class="brand">Delonix Runtime</div><div class="subtitle">{html.escape(ui["title"])}</div>
<div class="tools"><button class="searchbtn" type="button">🔍 {html.escape(ui["search"])} <kbd>Ctrl K</kbd></button>
<select class="lang" aria-label="Language">{options}</select></div>
{nav}</nav>
<main>{notice}{body}
<div class="pager">{prev_link}{next_link}</div>
<p class="foot"><a href="{REPO}/blob/main/{rel_src}" target="_blank" rel="noopener">{html.escape(ui["edit"])}</a> · <code>{rel_src}</code></p></main>
<aside class="toc"><h5>{html.escape(ui["on_page"])}</h5>{md.toc}</aside></div>
<div id="search" hidden><div class="sbox" role="dialog" aria-label="{html.escape(ui["search"])}">
<input type="search" placeholder="{html.escape(ui["search_ph"])}" autocomplete="off"><ul class="sres"></ul>
<div class="shelp"><span><kbd>↑</kbd> <kbd>↓</kbd></span><span><kbd>Enter</kbd></span><span><kbd>Esc</kbd></span></div></div></div>
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
    (out / "index.html").write_text(
        '<!doctype html><meta charset="utf-8"><title>Delonix Runtime — Contributor handbook</title>'
        '<meta http-equiv="refresh" content="0; url=en/index.html"><a href="en/index.html">Contributor handbook</a>\n'
    )


def status() -> int:
    behind = 0
    for page in english_pages():
        for lang in LANGS[1:]:
            _, state = translation_state(lang, page)
            if state != "current":
                behind += 1
                print(f"{lang:6} {state:8} {page.name}")
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
        print(f"dev_docs_site: wrote {OUT.relative_to(ROOT)}/")
        return 0
    with tempfile.TemporaryDirectory() as tmp:
        fresh = Path(tmp) / "handbook"
        render_site(fresh)
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
