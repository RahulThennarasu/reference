import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize } from "@tauri-apps/api/dpi";

interface SearchResult {
  path: string;
  score: number;
}

interface Citation {
  path: string;
  snippet: string;
}

interface Answer {
  summary: string;
  citations: Citation[];
}

interface SearchResponse {
  results: SearchResult[];
  answer: Answer | null;
}

const COLLAPSED_HEIGHT = 64;
const MAX_VISIBLE_ROWS = 8;
const MAX_WINDOW_HEIGHT = 560;
const WINDOW_WIDTH = 680;
const DEFAULT_PLACEHOLDER = "Search your files — ⌘7 to add a folder";
const FOLDER_PLACEHOLDER = "Type a folder path — Tab to complete, Enter to watch";

let searchInputEl: HTMLInputElement | null;
let resultsEl: HTMLElement | null;

let folderMode = false;
let folderSuggestions: string[] = [];
let folderSelectedIndex = 0;

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

function renderAnswer(answer: Answer): HTMLElement {
  const li = document.createElement("li");
  li.className = "answer";

  const summary = document.createElement("p");
  summary.className = "answer-summary";
  summary.textContent = answer.summary;
  li.appendChild(summary);

  const sources = document.createElement("div");
  sources.className = "answer-sources";
  for (const citation of answer.citations) {
    const chip = document.createElement("span");
    chip.className = "source-chip";
    chip.textContent = basename(citation.path);
    chip.title = citation.path;
    sources.appendChild(chip);
  }
  li.appendChild(sources);

  return li;
}

function renderResponse(response: SearchResponse) {
  if (!resultsEl) return;
  resultsEl.innerHTML = "";

  if (response.answer) {
    resultsEl.appendChild(renderAnswer(response.answer));
  }

  for (const r of response.results) {
    const li = document.createElement("li");
    li.className = "result";

    const name = document.createElement("span");
    name.className = "result-name";
    name.textContent = basename(r.path);

    const path = document.createElement("span");
    path.className = "result-path";
    path.textContent = dirname(r.path);

    li.appendChild(name);
    li.appendChild(path);
    resultsEl.appendChild(li);
  }

  resize();
}

function renderFolderSuggestions() {
  if (!resultsEl) return;
  const list = resultsEl;
  list.innerHTML = "";

  folderSuggestions.forEach((path, i) => {
    const li = document.createElement("li");
    li.className = "result suggestion" + (i === folderSelectedIndex ? " selected" : "");

    const name = document.createElement("span");
    name.className = "result-name";
    name.textContent = basename(path);

    const dir = document.createElement("span");
    dir.className = "result-path";
    dir.textContent = dirname(path);

    li.appendChild(name);
    li.appendChild(dir);
    list.appendChild(li);
  });

  resize();
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
      renderFolderSuggestions();
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
  if (searchInputEl) searchInputEl.placeholder = `Indexing ${folderName}...`;
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
    renderResponse(response);
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
        renderFolderSuggestions();
      }
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      if (folderSuggestions.length > 0) {
        folderSelectedIndex = Math.max(folderSelectedIndex - 1, 0);
        renderFolderSuggestions();
      }
    } else if (e.key === "Enter") {
      e.preventDefault();
      commitFolder();
    }
  });

  resize();
});
