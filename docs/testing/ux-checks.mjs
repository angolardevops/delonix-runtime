// Arnês de verificação UX/acessibilidade da documentação (§14 do brief).
//
// Uso:
//   1) gerar e servir:  cd docs && python3 gen.py <bin>/delonix && python3 -m http.server 8100
//   2) correr:          BASE=http://localhost:8100 node docs/testing/ux-checks.mjs
//      (com chromium em cache: PW_CHROME=/caminho/chrome)
//
// Depende de `playwright` ou `playwright-core` (+ um chromium). Sai != 0 se
// algum teste falhar. Não substitui revisão manual — complementa-a.

let chromium;
try { ({ chromium } = await import('playwright')); }
catch { ({ chromium } = await import('playwright-core')); }

const BASE = process.env.BASE || 'http://localhost:8100';
const exe = process.env.PW_CHROME || undefined;
const WIDTHS = [390, 768, 1440, 1920];
const PAGES = ['index.html', 'kinds.html', 'comandos/container.html', 'labs.html', 'cheatsheet.html'];

let pass = 0, fail = 0;
const ok = (c, m) => { if (c) { pass++; } else { fail++; console.log('  ✗ FAIL', m); } };

const browser = await chromium.launch({ executablePath: exe });

// 1) Sem scroll horizontal da página (páginas × larguras).
for (const w of WIDTHS) {
  const ctx = await browser.newContext({ viewport: { width: w, height: 900 } });
  const p = await ctx.newPage();
  for (const pg of PAGES) {
    await p.goto(`${BASE}/${pg}`, { waitUntil: 'networkidle' });
    const o = await p.evaluate(() => ({ sw: document.documentElement.scrollWidth, cw: document.documentElement.clientWidth }));
    ok(o.sw <= o.cw + 1, `overflow ${w}px ${pg} (${o.sw}>${o.cw})`);
  }
  await ctx.close();
}

// 2) Tema: alternância persiste após recarregar.
{
  const ctx = await browser.newContext();
  const p = await ctx.newPage();
  await p.goto(`${BASE}/index.html`, { waitUntil: 'networkidle' });
  await p.click('.theme-toggle');
  const t1 = await p.evaluate(() => document.documentElement.getAttribute('data-theme'));
  await p.reload({ waitUntil: 'networkidle' });
  const t2 = await p.evaluate(() => document.documentElement.getAttribute('data-theme'));
  ok(t1 && t1 === t2, `tema não persiste (${t1}->${t2})`);
  await ctx.close();
}

// 3) Pesquisa Ctrl+K: abre, indexa, Esc fecha e repõe o foco.
{
  const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const p = await ctx.newPage();
  await p.goto(`${BASE}/index.html`, { waitUntil: 'networkidle' });
  await p.keyboard.press('Control+k');
  await p.waitForTimeout(150);
  ok(await p.evaluate(() => document.getElementById('search-overlay').classList.contains('open')), 'Ctrl+K não abre a pesquisa');
  await p.fill('#search-input', 'delonix init');
  await p.waitForTimeout(250);
  const n = await p.evaluate(() => document.querySelectorAll('#search-results li').length);
  ok(n > 0, 'pesquisa sem resultados para "delonix init"');
  await p.keyboard.press('ArrowDown');
  ok(await p.evaluate(() => !!document.querySelector('#search-results li.active')), 'teclado não navega resultados');
  await p.keyboard.press('Escape');
  await p.waitForTimeout(100);
  ok(!(await p.evaluate(() => document.getElementById('search-overlay').classList.contains('open'))), 'Esc não fecha a pesquisa');
  await ctx.close();
}

// 4) Cópia de código: existe cromado com botão e o clique confirma.
{
  const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, permissions: ['clipboard-read', 'clipboard-write'] });
  const p = await ctx.newPage();
  await p.goto(`${BASE}/index.html`, { waitUntil: 'networkidle' });
  const btn = await p.$('.codeblock .code-head .copy-btn');
  ok(!!btn, 'sem botão de cópia no cromado');
  if (btn) { await btn.click(); await p.waitForTimeout(100); ok(await p.evaluate(() => !!document.querySelector('.copy-btn.ok')), 'cópia não confirma (.ok)'); }
  await ctx.close();
}

// 5) Zoom 200%: o zoom real do browser reduz o viewport efectivo a metade e
// dispara as media-queries (layout reflui). Emula-se com viewport/2 +
// deviceScaleFactor 2 — assim testa-se o REFLUXO, não um escalonamento CSS.
{
  const ctx = await browser.newContext({ viewport: { width: 640, height: 450 }, deviceScaleFactor: 2 });
  const p = await ctx.newPage();
  await p.goto(`${BASE}/kinds.html`, { waitUntil: 'networkidle' });
  await p.waitForTimeout(150);
  const o = await p.evaluate(() => ({ sw: document.documentElement.scrollWidth, cw: document.documentElement.clientWidth }));
  ok(o.sw <= o.cw + 2, `overflow a 200% / 640px (${o.sw}>${o.cw})`);
  ok(await p.$('.nav-toggle'), 'a 200% o menu (hambúrguer) não está disponível');
  await ctx.close();
}

// 6) Página profunda aberta directamente + âncora.
{
  const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const p = await ctx.newPage();
  const r = await p.goto(`${BASE}/comandos/container.html`, { waitUntil: 'networkidle' });
  ok(r && r.status() < 400, 'página profunda não carrega');
  ok(await p.$('nav.side'), 'página profunda sem sidebar');
  ok(await p.$('.breadcrumb'), 'página profunda sem breadcrumb');
  await p.goto(`${BASE}/kinds.html#secret`, { waitUntil: 'networkidle' });
  await p.waitForTimeout(200);
  ok(await p.evaluate(() => !!document.getElementById('secret')), 'âncora #secret inexistente');
  await ctx.close();
}

await browser.close();
console.log(`\n${fail ? '✗' : '✓'} UX checks: ${pass} passaram, ${fail} falharam`);
process.exit(fail ? 1 : 0);
