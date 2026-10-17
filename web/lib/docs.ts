import fs from "node:fs";
import path from "node:path";
import matter from "gray-matter";
import { marked } from "marked";

/* ── Navigation structure ──────────────────────────────────────────────────
   One entry per markdown file in content/docs/. The slug is the filename
   (without .md) and the URL is /docs/<slug>. The side rail, the docs overview
   page, and prev/next paging all read from this single structure — add a page
   by creating content/docs/<slug>.md and dropping a NavItem in the right
   group. */

export type NavItem = {
  slug: string;
  label: string;
  blurb: string;
};

export type NavGroup = {
  title: string;
  items: NavItem[];
};

export const NAV_GROUPS: NavGroup[] = [
  {
    title: "Getting Started",
    items: [
      { slug: "getting-started", label: "Getting started", blurb: "Install, first launch, the essential keys" },
      { slug: "configuration", label: "Configuration", blurb: "init.scm, options, and config paths" },
      { slug: "example-config", label: "Example config", blurb: "A real, annotated evil + SPC-leader init.scm" },
      { slug: "updates", label: "Updates", blurb: "How Macros updates and how to disable auto-updating" },
      { slug: "keybindings", label: "Keybindings", blurb: "The default keymap and how to rebind" },
      { slug: "getting-help", label: "Getting help", blurb: "Discover commands from inside the editor" },
      { slug: "tutorial", label: "Tutorial", blurb: "The interactive in-editor tutorial (C-h t)" },
    ],
  },
  {
    title: "Editing",
    items: [
      { slug: "modal-editing", label: "Modal editing (evil)", blurb: "Vim-style motions, operators, text objects" },
      { slug: "helix", label: "Modal editing (helix)", blurb: "Selection-first motions, verbs, multiple selections" },
      { slug: "editing-commands", label: "Editing commands", blurb: "Kill/yank, region, multiple cursors, occur" },
      { slug: "completion", label: "Completion", blurb: "The corfu-style popup and CAPFs" },
      { slug: "snippets", label: "Snippets", blurb: "YASnippet-style templates with tab-stops" },
    ],
  },
  {
    title: "Code",
    items: [
      { slug: "lsp", label: "Language servers (LSP)", blurb: "Definitions, references, rename, code actions" },
      { slug: "diagnostics", label: "Diagnostics", blurb: "Flycheck-style errors and navigation" },
      { slug: "projects", label: "Projects & search", blurb: "Projectile + ripgrep find-file, grep, and perspectives" },
      { slug: "shell-and-compile", label: "Shell & compile", blurb: "compile, grep, next-error, the shell" },
      { slug: "terminal", label: "Terminal", blurb: "The embedded VT/ANSI terminal" },
    ],
  },
  {
    title: "Tools",
    items: [
      { slug: "git", label: "Git (Magit)", blurb: "Stage, commit, branch, read diffs" },
      { slug: "ediff", label: "Ediff", blurb: "Diff files and merge hunks between buffers" },
      { slug: "dired", label: "Dired", blurb: "The directory editor / file manager" },
      { slug: "dired-sidebar", label: "Dired sidebar", blurb: "The bundled file-tree sidebar" },
      { slug: "grid", label: "Grid (CSV & spreadsheets)", blurb: "CSV, TSV, and .xlsx as a spreadsheet; flip to raw text with one key" },
      { slug: "grid-formulas", label: "Grid formulas", blurb: "Excel-compatible formulas, 150 functions, and functions in Scheme" },
      { slug: "org-mode", label: "Org mode", blurb: "Headings, TODOs, agenda, tables, babel" },
      { slug: "helm", label: "Helm", blurb: "The selection / narrowing framework" },
    ],
  },
  {
    title: "Customization",
    items: [
      { slug: "scripting", label: "Scripting with Steel", blurb: "Define commands, keymaps, and modes" },
      { slug: "anvil", label: "Anvil REPL", blurb: "A SLIME-style Steel REPL in an isolated, interruptible image — eval, macroexpand, doc, apropos" },
      { slug: "languages", label: "Languages & modes", blurb: "Tree-sitter highlighting and major modes" },
      { slug: "packages", label: "Packages", blurb: "Install Steel extensions from git with package-install" },
      { slug: "secrets", label: "Passwords & secrets", blurb: "One encrypted authinfo.gpg store for every package; secrets as option values" },
      { slug: "mcp-server", label: "MCP server", blurb: "Let an AI assistant eval Steel and read buffers in the live editor" },
      { slug: "titlebar-menus", label: "Title-bar menus", blurb: "Add dropdowns of quick actions to the title bar" },
      { slug: "themes-and-faces", label: "Themes & faces", blurb: "Load themes, set faces, line numbers" },
      { slug: "plotting", label: "Text charts", blurb: "Sparklines, bars, line plots, shaded function and normal-distribution plots, histograms, heatmaps" },
      { slug: "rainbow-delimiters", label: "Rainbow & tree-sitter", blurb: "Depth-colored brackets via tree-sitter" },
    ],
  },
];

export const DOCS_DIR = path.join(process.cwd(), "content", "docs");

export type TocEntry = { id: string; text: string; depth: number };

export type Doc = {
  slug: string;
  title: string;
  description: string;
  html: string;
  toc: TocEntry[];
};

/** All known doc slugs, in nav order. */
export function allDocSlugs(): string[] {
  return NAV_GROUPS.flatMap((g) => g.items.map((i) => i.slug));
}

/** Flattened nav items, in nav order — used for prev/next paging. */
export function flatNav(): NavItem[] {
  return NAV_GROUPS.flatMap((g) => g.items);
}

export function navItem(slug: string): NavItem | undefined {
  return flatNav().find((i) => i.slug === slug);
}

function slugifyText(s: string): string {
  return s
    .toLowerCase()
    .replace(/`/g, "")
    .replace(/[^a-z0-9\s-]/g, "")
    .trim()
    .replace(/\s+/g, "-");
}

/** A slug generator that dedupes repeats (heading-1, heading-2, …). Two
 *  generators fed the same heading sequence produce identical ids, so the TOC
 *  pass and the render pass stay in sync. */
function makeSlugger() {
  const seen = new Map<string, number>();
  return (text: string) => {
    const base = slugifyText(text) || "section";
    const n = seen.get(base) ?? 0;
    seen.set(base, n + 1);
    return n === 0 ? base : `${base}-${n}`;
  };
}

/** Strip inline markdown/html to plain text for headings + the TOC. */
function plainText(md: string): string {
  return md
    .replace(/`([^`]*)`/g, "$1")
    .replace(/\*\*?([^*]*)\*\*?/g, "$1")
    .replace(/\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/<[^>]+>/g, "")
    .trim();
}

export function getDoc(slug: string): Doc | null {
  const file = path.join(DOCS_DIR, `${slug}.md`);
  if (!fs.existsSync(file)) return null;

  const raw = fs.readFileSync(file, "utf8");
  const { data, content } = matter(raw);
  const nav = navItem(slug);

  // Build the TOC (h2 + h3) and the matching heading id sequence.
  const toc: TocEntry[] = [];
  const tocSlugger = makeSlugger();
  for (const tok of marked.lexer(content)) {
    if (tok.type === "heading" && (tok.depth === 2 || tok.depth === 3)) {
      const text = plainText(tok.text);
      toc.push({ id: tocSlugger(text), text, depth: tok.depth });
    }
  }

  // Render, assigning the same ids to headings so anchors line up.
  const renderSlugger = makeSlugger();
  const renderer = new marked.Renderer();
  renderer.heading = function ({ text, tokens, depth }) {
    const inner = this.parser.parseInline(tokens);
    if (depth === 2 || depth === 3) {
      // Slug from the SAME source and the SAME heading subset (h2/h3 only) the
      // TOC pass uses, so the dedup counters, anchor ids and TOC hrefs line up.
      const id = renderSlugger(plainText(text));
      return `<h${depth} id="${id}" class="section-anchor">${inner}` +
        `<a href="#${id}" class="section-anchor-hash" aria-hidden="true" ` +
        `style="margin-left:8px;color:var(--accent);text-decoration:none;">#</a></h${depth}>`;
    }
    return `<h${depth}>${inner}</h${depth}>`;
  };

  const html = marked.parse(content, { renderer, gfm: true, async: false }) as string;

  return {
    slug,
    title: (data.title as string) || nav?.label || slug,
    description: (data.description as string) || nav?.blurb || "",
    html,
    toc,
  };
}
