import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  getIconForDirectoryPath,
  getIconUrlByName,
  getIconUrlForFilePath,
} from "vscode-material-icons";
import { parseInline } from "marked";
import DOMPurify from "dompurify";
import hljs from "highlight.js/lib/core";
import rust from "highlight.js/lib/languages/rust";
import python from "highlight.js/lib/languages/python";
import typescript from "highlight.js/lib/languages/typescript";
import javascript from "highlight.js/lib/languages/javascript";

// Only the languages the chunker actually understands (see core/src/chunk.rs)
// are registered — pulling in hljs's full language bundle would be dead
// weight for everything else, which is still shown as plain text.
hljs.registerLanguage("rust", rust);
hljs.registerLanguage("python", python);
hljs.registerLanguage("typescript", typescript);
hljs.registerLanguage("javascript", javascript);

// Answer snippets are a single extracted sentence/table-row, not a full
// document — inline-only rendering (code spans, bold, links) is what
// actually shows up in practice, so block-level parsing isn't needed.
// Sanitized because the source is arbitrary indexed file content, not
// something we authored.
function renderInlineMarkdown(text: string): string {
  return DOMPurify.sanitize(parseInline(text) as string);
}

function hljsLanguageForPath(path: string): string | null {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  switch (ext) {
    case "rs":
      return "rust";
    case "py":
      return "python";
    case "ts":
    case "mts":
    case "cts":
    case "tsx":
      return "typescript";
    case "js":
    case "jsx":
    case "mjs":
    case "cjs":
      return "javascript";
    default:
      return null;
  }
}

// hljs escapes source text internally (it never interprets the input as
// HTML), so building innerHTML from its output is safe even though the
// underlying content is arbitrary indexed file text, not something we wrote.
function renderCodeBlock(path: string, code: string): HTMLElement {
  const pre = document.createElement("pre");
  pre.className = "code-block";

  const codeEl = document.createElement("code");
  const language = hljsLanguageForPath(path);
  const highlighted = language
    ? hljs.highlight(code, { language, ignoreIllegals: true }).value
    : hljs.highlightAuto(code).value;

  codeEl.className = language ? `hljs language-${language}` : "hljs";
  codeEl.innerHTML = highlighted;
  pre.appendChild(codeEl);
  return pre;
}

const ICONS_URL = "/material-icons";

function fileIconUrl(path: string): string {
  return getIconUrlForFilePath(path, ICONS_URL);
}

function folderIconUrl(path: string): string {
  return getIconUrlByName(getIconForDirectoryPath(path), ICONS_URL);
}

function iconImg(src: string): HTMLImageElement {
  const icon = document.createElement("img");
  icon.className = "row-icon";
  icon.src = src;
  icon.alt = "";
  return icon;
}

interface SearchResult {
  path: string;
  score: number;
  start_line: number;
  end_line: number;
  chunk_kind: string;
}

interface Citation {
  path: string;
  snippet: string;
  start_line: number;
  end_line: number;
  chunk_kind: string;
}

interface SearchResponse {
  results: SearchResult[];
  citations: Citation[];
}

// VSCode registers this URI scheme itself; no shell-out or `code` CLI
// dependency needed. Falls back to just opening the file if the line lookup
// or the open itself fails, rather than doing nothing.
async function openInEditor(path: string, line: number) {
  try {
    await openUrl(`vscode://file${path}:${line}:1`);
  } catch (err) {
    console.error("openInEditor failed", err);
  }
}

// A code-chunk result (function/impl/class/interface/...) already carries
// its exact start line from the index — no need to re-score the file
// line-by-line at click time the way plain "file"-kind results (unchunked
// languages, prose) still do.
async function openResultInEditor(result: SearchResult, query: string) {
  if (result.chunk_kind !== "file") {
    await openInEditor(result.path, result.start_line);
    return;
  }

  let line = 1;
  try {
    line = await invoke<number>("find_line", { path: result.path, query });
  } catch (err) {
    console.error("find_line failed", err);
  }
  await openInEditor(result.path, line);
}

const COLLAPSED_HEIGHT = 64;
const MAX_VISIBLE_ROWS = 8;
const MAX_WINDOW_HEIGHT = 560;
const WINDOW_WIDTH = 680;
const DEFAULT_PLACEHOLDER = "search your files — ⌘7 to add a folder";
const FOLDER_PLACEHOLDER = "type a folder path — tab to complete, enter to watch";

let searchInputEl: HTMLInputElement | null;
let resultsEl: HTMLElement | null;

let folderMode = false;
let folderSuggestions: string[] = [];
let folderSelectedIndex = 0;
let watchedFolders: string[] = [];

function basename(path: string): string {
  return path.replace(/\/$/, "").split("/").pop() ?? path;
}

function dirname(path: string): string {
  const parts = path.replace(/\/$/, "").split("/");
  parts.pop();
  return parts.join("/");
}

async function resize() {
  if (!resultsEl) return;
  // #results is `flex: 1`, so it stretches to fill whatever space the
  // window currently has — meaning its scrollHeight normally reflects the
  // *stretched* box, not the actual content, and each resize ends up
  // measuring the size the previous resize just set (a runaway ratchet).
  // Un-stretch it just long enough to measure true content height.
  resultsEl.style.flex = "none";
  resultsEl.style.height = "auto";
  const contentHeight = resultsEl.scrollHeight;
  resultsEl.style.flex = "";
  resultsEl.style.height = "";

  const height =
    contentHeight === 0
      ? COLLAPSED_HEIGHT
      : Math.min(COLLAPSED_HEIGHT + 12 + contentHeight, MAX_WINDOW_HEIGHT);
  console.log("resize(): contentHeight =", contentHeight, "-> window height =", height);
  try {
    await getCurrentWindow().setSize(new LogicalSize(WINDOW_WIDTH, height));
  } catch (err) {
    console.error("resize failed", err);
  }
}

function renderError(message: string) {
  if (!resultsEl) return;
  resultsEl.innerHTML = "";
  const li = document.createElement("li");
  li.className = "result result-error";
  li.textContent = message;
  resultsEl.appendChild(li);
  resize();
}

// Each citation renders as its own block (snippet + one source line) rather
// than joining every snippet into one paragraph — concatenating sentences
// pulled from unrelated files reads as an incoherent run-on, especially once
// more than one folder is watched.
function renderCitation(citation: Citation): HTMLElement {
  const li = document.createElement("li");
  li.className = "answer clickable";
  li.title = `open ${basename(citation.path)} at line ${citation.start_line}`;
  li.addEventListener("click", () => openInEditor(citation.path, citation.start_line));

  // A "file"-kind citation is prose (or an unchunked language's whole-file
  // blob) — rendered as inline markdown same as before. Anything else came
  // from the language-aware chunker (function/class/impl/interface) and is
  // real source code, so it gets a syntax-highlighted code block instead.
  if (citation.chunk_kind === "file") {
    const summary = document.createElement("p");
    summary.className = "answer-summary";
    summary.innerHTML = renderInlineMarkdown(citation.snippet);
    li.appendChild(summary);
  } else {
    li.appendChild(renderCodeBlock(citation.path, citation.snippet));
  }

  const source = document.createElement("span");
  source.className = "source-chip";
  source.title = citation.path;

  const icon = iconImg(fileIconUrl(citation.path));
  icon.classList.add("source-icon");

  const label = document.createElement("span");
  label.textContent = basename(citation.path);

  source.appendChild(icon);
  source.appendChild(label);
  li.appendChild(source);

  return li;
}

function renderResponse(response: SearchResponse, query: string) {
  if (!resultsEl) return;
  resultsEl.innerHTML = "";

  for (const citation of response.citations) {
    resultsEl.appendChild(renderCitation(citation));
  }

  for (const r of response.results) {
    const li = document.createElement("li");
    li.className = "result clickable";
    li.title = `open ${basename(r.path)}`;
    li.addEventListener("click", () => openResultInEditor(r, query));

    const name = document.createElement("span");
    name.className = "result-name";
    name.textContent = basename(r.path);

    const path = document.createElement("span");
    path.className = "result-path";
    path.textContent = dirname(r.path);

    li.appendChild(iconImg(fileIconUrl(r.path)));
    li.appendChild(name);
    li.appendChild(path);
    resultsEl.appendChild(li);
  }

  resize();
}

async function unwatchFolder(folder: string) {
  try {
    await invoke("stop_watch", { folder });
  } catch (err) {
    console.error("stop_watch failed", err);
  }
  await refreshWatchedFolders();
  renderFolderMode();
}

function renderFolderMode() {
  if (!resultsEl) return;
  const list = resultsEl;
  list.innerHTML = "";

  if (watchedFolders.length > 0) {
    const label = document.createElement("li");
    label.className = "section-label";
    label.textContent = "watching";
    list.appendChild(label);

    for (const folder of watchedFolders) {
      const li = document.createElement("li");
      li.className = "result watched-folder";

      const name = document.createElement("span");
      name.className = "result-name";
      name.textContent = basename(folder);

      const dir = document.createElement("span");
      dir.className = "result-path";
      dir.textContent = folder;

      const remove = document.createElement("button");
      remove.className = "unwatch-btn";
      // Lucide's "x" icon, inlined (no React here, so react-icons doesn't
      // apply — this is the same style icon set, just used as raw SVG).
      remove.innerHTML =
        '<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>';
      remove.title = `stop watching ${folder}`;
      remove.addEventListener("click", (e) => {
        e.stopPropagation();
        unwatchFolder(folder);
      });

      li.appendChild(iconImg(folderIconUrl(folder)));
      li.appendChild(name);
      li.appendChild(dir);
      li.appendChild(remove);
      list.appendChild(li);
    }
  }

  if (folderSuggestions.length > 0) {
    if (watchedFolders.length > 0) {
      const label = document.createElement("li");
      label.className = "section-label";
      label.textContent = "add a folder";
      list.appendChild(label);
    }

    folderSuggestions.forEach((path, i) => {
      const li = document.createElement("li");
      li.className = "result suggestion" + (i === folderSelectedIndex ? " selected" : "");

      const name = document.createElement("span");
      name.className = "result-name";
      name.textContent = basename(path);

      const dir = document.createElement("span");
      dir.className = "result-path";
      dir.textContent = dirname(path);

      li.appendChild(iconImg(folderIconUrl(path)));
      li.appendChild(name);
      li.appendChild(dir);
      list.appendChild(li);
    });
  }

  resize();
}

async function refreshWatchedFolders() {
  try {
    watchedFolders = await invoke<string[]>("list_watched");
  } catch (err) {
    console.error("list_watched failed", err);
  }
}

let folderDebounce: ReturnType<typeof setTimeout> | null = null;

function updateFolderSuggestions() {
  if (!searchInputEl) return;
  const partial = searchInputEl.value;

  if (folderDebounce) clearTimeout(folderDebounce);
  folderDebounce = setTimeout(async () => {
    try {
      folderSuggestions = await invoke<string[]>("list_dir_suggestions", { partial });
      folderSelectedIndex = 0;
      renderFolderMode();
    } catch (err) {
      console.error("list_dir_suggestions failed", err);
    }
  }, 80);
}

async function enterFolderMode() {
  if (!searchInputEl) return;
  folderMode = true;

  let home = "/";
  try {
    home = await invoke<string>("home_dir");
  } catch (err) {
    console.error("home_dir failed", err);
  }

  searchInputEl.value = `${home}/`;
  searchInputEl.placeholder = FOLDER_PLACEHOLDER;
  searchInputEl.focus();
  searchInputEl.setSelectionRange(searchInputEl.value.length, searchInputEl.value.length);

  await refreshWatchedFolders();
  renderFolderMode();
  updateFolderSuggestions();
}

function exitFolderMode() {
  folderMode = false;
  folderSuggestions = [];
  folderSelectedIndex = 0;
  if (searchInputEl) {
    searchInputEl.value = "";
    searchInputEl.placeholder = DEFAULT_PLACEHOLDER;
  }
  if (resultsEl) resultsEl.innerHTML = "";
  resize();
}

async function commitFolder() {
  if (!searchInputEl) return;
  const folder = searchInputEl.value.replace(/\/$/, "");
  if (!folder) return;
  const folderName = basename(folder);

  exitFolderMode();
  if (searchInputEl) searchInputEl.placeholder = `indexing ${folderName}...`;
  try {
    await invoke("start_watch", { folder });
  } catch (err) {
    console.error("start_watch failed", err);
    renderError(`failed to watch ${folder}: ${err}`);
  } finally {
    setTimeout(() => {
      if (searchInputEl) searchInputEl.placeholder = DEFAULT_PLACEHOLDER;
    }, 1500);
  }
}

let searchDebounce: ReturnType<typeof setTimeout> | null = null;
// Search is CPU-bound on the Rust side (a full BERT forward pass per
// query, competing with any in-progress indexing for the same cores).
// Debounce alone only stops timers that haven't fired *yet* — it does
// nothing once a request is already in flight. Without this gate, typing
// at a normal pace while indexing is busy lets searches queue up faster
// than they finish, and the backlog snowballs. So: at most one `search`
// invocation in flight at a time; anything typed while one's running just
// replaces the pending query rather than stacking a new request.
let searchInFlight = false;
let pendingQuery: string | null = null;

function search() {
  if (!searchInputEl) return;
  const query = searchInputEl.value.trim();

  if (searchDebounce) clearTimeout(searchDebounce);
  if (!query) {
    pendingQuery = null;
    if (resultsEl) resultsEl.innerHTML = "";
    resize();
    return;
  }

  searchDebounce = setTimeout(() => runSearch(query), 150);
}

async function runSearch(query: string) {
  if (searchInFlight) {
    pendingQuery = query;
    return;
  }

  searchInFlight = true;
  try {
    const response = await invoke<SearchResponse>("search", {
      query,
      topK: MAX_VISIBLE_ROWS,
    });
    renderResponse(response, query);
  } catch (err) {
    console.error("search failed", err);
    renderError(`search failed: ${err}`);
  } finally {
    searchInFlight = false;
    if (pendingQuery !== null) {
      const next = pendingQuery;
      pendingQuery = null;
      runSearch(next);
    }
  }
}

window.addEventListener("DOMContentLoaded", () => {
  searchInputEl = document.querySelector("#search-input");
  resultsEl = document.querySelector("#results");

  searchInputEl?.addEventListener("input", () => {
    if (folderMode) {
      updateFolderSuggestions();
    } else {
      search();
    }
  });

  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      if (folderMode) {
        exitFolderMode();
      } else {
        getCurrentWindow().close();
      }
      return;
    }

    if (e.key === "7" && e.metaKey) {
      e.preventDefault();
      enterFolderMode();
      return;
    }

    if (!folderMode) return;

    if (e.key === "Tab") {
      e.preventDefault();
      if (folderSuggestions.length > 0 && searchInputEl) {
        searchInputEl.value = folderSuggestions[folderSelectedIndex];
        updateFolderSuggestions();
      }
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      if (folderSuggestions.length > 0) {
        folderSelectedIndex = Math.min(folderSelectedIndex + 1, folderSuggestions.length - 1);
        renderFolderMode();
      }
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      if (folderSuggestions.length > 0) {
        folderSelectedIndex = Math.max(folderSelectedIndex - 1, 0);
        renderFolderMode();
      }
    } else if (e.key === "Enter") {
      e.preventDefault();
      commitFolder();
    }
  });

  resize();
});
