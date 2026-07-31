// MCP server: the agent-facing successor to the removed `reference-cli`.
// Exposes one read-only `search` tool over the same index the Tauri app
// watches and writes (`~/.reference/index`) — no tool to add/remove watched
// folders, by design, since folder opt-in is a deliberate human action taken
// through the app's picker, never something an agent should be able to do
// on its own (see CLAUDE.md's "opt-in, never automatic" principle).
//
// Unlike the CLI, this process stays alive for the duration of an agent
// session instead of reloading the embedding model on every invocation, so
// repeated searches mid-task are fast after the first one.
use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo,
};
use rmcp::{tool, tool_handler, tool_router, transport::stdio, ErrorData as McpError, ServerHandler, ServiceExt};
use serde::{Deserialize, Serialize};
use serde_json::json;

use reference_core::embedding::Embedder;
use reference_core::paths;
use reference_core::store::{RankingWeights, Store};
use reference_core::synthesize;

// Mirrors the Tauri app's `SYNTHESIS_CANDIDATE_POOL` (app/src-tauri/src/lib.rs):
// `synthesize()` needs a wider candidate pool to pick citations from than
// whatever small `top_k` the caller asked for, otherwise a low `top_k` (the
// MCP default is 5) starves it down to exactly the results already shown,
// same failure mode the app avoids by expanding its own search pool first.
const SYNTHESIS_CANDIDATE_POOL: usize = 50;

// Whole-file chunks (docs, or any language with no code-aware chunker — see
// docs/code-aware-chunking.md) have no per-chunk size cap the way a parsed
// function does, and an MCP caller's `content`/`snippet` field goes straight
// into an agent's context window, unlike the app's UI where a human just
// scrolls. A real observed case: a 5-hit `search` scoped to this repo's own
// docs returned ~30KB of raw text in one call, most of it whole markdown
// files repeated near-verbatim across hits. Truncating here (not in
// `core::synthesize`, which the app also uses and has no such budget)
// keeps every tool call's response bounded by default.
const DEFAULT_MAX_CONTENT_CHARS: usize = 1500;

/// Truncates `content` to at most `max_chars` *characters* (not bytes) so a
/// multi-byte UTF-8 boundary is never split, with a trailing marker noting
/// how much was cut — an agent that actually needs the rest already has
/// `path`/`start_line`/`end_line` to read the file directly.
fn truncate_content(content: &str, max_chars: usize) -> String {
    let total_chars = content.chars().count();
    if total_chars <= max_chars {
        return content.to_string();
    }
    let truncated: String = content.chars().take(max_chars).collect();
    let omitted = total_chars - max_chars;
    format!("{truncated}\n… [truncated, {omitted} more chars — read the file directly for the rest]")
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchParams {
    /// Natural language query describing behavior or intent, not a grep
    /// pattern (e.g. "where do we retry failed api calls").
    query: String,
    /// How many results to return. Defaults to 5.
    top_k: Option<usize>,
    /// Scope the search to one watched folder (absolute path), e.g. the
    /// current project's root. Omit to search everything this machine has
    /// opted into watching. Set this whenever you know which project the
    /// query is about — otherwise an unrelated watched folder that merely
    /// shares some vocabulary with the query can outrank the file that's
    /// actually relevant.
    folder: Option<String>,
    /// Exclude one watched folder (absolute path) from the search — the
    /// inverse of `folder`. Set this to the current project's root when
    /// looking for how something was solved in a *different* watched
    /// project: the current project's own code isn't prior art for itself,
    /// so searching it too would just return the thing already being worked
    /// on instead of past solutions elsewhere. Mutually exclusive with
    /// `folder` in practice, though not enforced.
    exclude_folder: Option<String>,
    /// Caps each result's `content` field to at most this many characters
    /// (truncated with a marker noting how much was cut, not silently).
    /// Defaults to 1500 — a whole-file chunk (a doc, or any language with no
    /// code-aware chunker) has no per-chunk size limit otherwise and can
    /// blow up a single tool call's response size. Raise this if you
    /// specifically need a large chunk's full text; `path`/`start_line`/
    /// `end_line` are always enough to read the rest directly regardless.
    max_content_chars: Option<usize>,
}

#[derive(Serialize)]
struct JsonHit {
    path: String,
    start_line: i32,
    end_line: i32,
    chunk_kind: String,
    score: f32,
    // `hybrid_search` already has this in hand; an MCP caller is an agent
    // that almost always needs the actual text next, not just a pointer to
    // it, unlike the app's UI where a human just clicks through. Returning
    // it here saves a follow-up Read call on every single search.
    content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ExplainParams {
    /// Natural language query, identifier, or short phrase describing what
    /// to explain — unlike `search`, phrasing doesn't matter here. `search`
    /// only synthesizes citations when the query reads as a grammatical
    /// question (`synthesize::is_question`), so a query like "parse_config"
    /// or "rate limiting" gets raw results but zero citations even when an
    /// agent genuinely wants an explanation, not just a location. This tool
    /// always synthesizes, regardless of phrasing.
    query: String,
    /// How many citations to return. Defaults to 3 (`synthesize`'s own
    /// `MAX_CITED_FILES`, not `search`'s default 5 — citations are curated,
    /// not a ranked list, so a larger number just admits lower-relevance
    /// noise past `synthesize`'s own relevance cutoff).
    top_k: Option<usize>,
    /// Scope to one watched folder (absolute path). Same reasoning as
    /// `search`'s `folder` param.
    folder: Option<String>,
    /// Exclude one watched folder (absolute path). Same reasoning as
    /// `search`'s `exclude_folder` param.
    exclude_folder: Option<String>,
    /// Caps each citation's `snippet` field to at most this many characters.
    /// Same reasoning and default (1500) as `search`'s `max_content_chars` —
    /// a whole-chunk citation (a large function, or a whole prose file) has
    /// no size limit otherwise.
    max_content_chars: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FindSimilarParams {
    /// Absolute path of the file containing the chunk to compare against —
    /// usually the `path` of a hit already returned by `search`.
    path: String,
    /// The chunk's `start_line`, also usually taken straight from a prior
    /// `search` hit. Identifies which chunk in the file to compare against,
    /// since a file is chunked into multiple rows (see
    /// docs/code-aware-chunking.md), not indexed as one unit.
    start_line: i32,
    /// How many similar chunks to return. Defaults to 5.
    top_k: Option<usize>,
    /// Scope candidates to one watched folder (absolute path). Omit to
    /// search everything this machine has opted into watching.
    folder: Option<String>,
    /// Caps each result's `content` field to at most this many characters.
    /// Same reasoning and default (1500) as `search`'s `max_content_chars`.
    max_content_chars: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CheckDocDriftParams {
    /// Absolute path of the doc file containing the chunk to check —
    /// usually the `path` of a hit already returned by `search`.
    path: String,
    /// The chunk's `start_line`, also usually taken straight from a prior
    /// `search` hit. Markdown files fall back to one whole-file chunk (no
    /// code-aware chunker for prose), so this is almost always `1` for a
    /// doc file.
    start_line: i32,
    /// How many of the closest matching code chunks to return. Defaults to 5.
    top_k: Option<usize>,
    /// Score below which the top match is considered evidence the doc has
    /// drifted from the code it describes. Defaults to 0.35 — the same
    /// dot-product scale `find_similar` and `search`'s semantic component
    /// use, not a probability.
    stale_threshold: Option<f32>,
    /// Scope candidates to one watched folder (absolute path). Omit to
    /// search everything this machine has opted into watching.
    folder: Option<String>,
    /// Caps each result's `content` field to at most this many characters.
    /// Same reasoning and default (1500) as `search`'s `max_content_chars`.
    max_content_chars: Option<usize>,
}

#[derive(Serialize)]
struct JsonCitation {
    path: String,
    snippet: String,
    start_line: usize,
    end_line: usize,
    chunk_kind: String,
}

#[derive(Clone)]
struct ReferenceServer {
    embedder: std::sync::Arc<Embedder>,
    store: std::sync::Arc<Store>,
    tool_router: ToolRouter<ReferenceServer>,
}

#[tool_router]
impl ReferenceServer {
    async fn new() -> Result<Self> {
        let db_uri = paths::default_db_uri();
        std::fs::create_dir_all(paths::default_app_data_dir())?;

        // Reads whatever model the app currently has configured, not just
        // the default — this server opens the exact same on-disk index the
        // app writes to, so query embeddings have to come from the same
        // model as the stored vectors (see `load_configured_model`'s doc
        // comment in core/src/embedding.rs).
        let model = reference_core::embedding::load_configured_model(&paths::default_settings_path());
        tracing::info!("loading embedding model ({})...", model.display_name());
        let embedder = Embedder::load(model).await?;
        let store = Store::open(&db_uri).await?;

        Ok(Self {
            embedder: std::sync::Arc::new(embedder),
            store: std::sync::Arc::new(store),
            tool_router: Self::tool_router(),
        })
    }

    #[tool(
        description = "Semantic search over the locally indexed files/code this machine has opted into watching. Prefer this over grep whenever the question describes behavior or intent rather than naming a literal string/identifier to search for."
    )]
    async fn search(
        &self,
        Parameters(SearchParams { query, top_k, folder, exclude_folder, max_content_chars }): Parameters<
            SearchParams,
        >,
    ) -> Result<CallToolResult, McpError> {
        let k = top_k.unwrap_or(5);
        let max_chars = max_content_chars.unwrap_or(DEFAULT_MAX_CONTENT_CHARS);

        let embedding = self
            .embedder
            .embed(&query)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        // Fixed defaults, not user-adjustable: ranking-weight tuning (gap #5,
        // docs/feature-gaps.md) is an app-only knob, same reasoning as
        // folder scoping/exact-match lookup staying app-only elsewhere —
        // this is a stdio tool call from an agent, not a session with
        // persisted per-user settings to read.
        let hits = self
            .store
            .hybrid_search(
                &query,
                &embedding,
                k.max(SYNTHESIS_CANDIDATE_POOL),
                folder.as_deref(),
                exclude_folder.as_deref(),
                &RankingWeights::default(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let citations = if synthesize::is_question(&query) && !hits.is_empty() {
            synthesize::synthesize(&self.embedder, &query, &hits)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        } else {
            Vec::new()
        };

        let results: Vec<JsonHit> = hits
            .iter()
            .take(k)
            .map(|h| JsonHit {
                path: h.path.clone(),
                start_line: h.start_line,
                end_line: h.end_line,
                chunk_kind: h.chunk_kind.clone(),
                score: h.score,
                content: truncate_content(&h.content, max_chars),
            })
            .collect();
        let citations: Vec<JsonCitation> = citations
            .into_iter()
            .map(|c| JsonCitation {
                path: c.path,
                snippet: truncate_content(&c.snippet, max_chars),
                start_line: c.start_line,
                end_line: c.end_line,
                chunk_kind: c.chunk_kind,
            })
            .collect();

        let body = json!({ "results": results, "citations": citations });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Explain something in the indexed codebase by returning synthesized citations, unconditionally, regardless of how the query is phrased. Use this instead of `search` when you want an explanation of what/how/why for a specific identifier or short phrase (e.g. \"parse_config\", \"rate limiting\") that isn't grammatically a question — `search` only synthesizes citations for question-shaped queries and would return raw results with no citations for a query like that."
    )]
    async fn explain(
        &self,
        Parameters(ExplainParams { query, top_k, folder, exclude_folder, max_content_chars }): Parameters<
            ExplainParams,
        >,
    ) -> Result<CallToolResult, McpError> {
        let k = top_k.unwrap_or(3);
        let max_chars = max_content_chars.unwrap_or(DEFAULT_MAX_CONTENT_CHARS);

        let embedding = self
            .embedder
            .embed(&query)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let hits = self
            .store
            .hybrid_search(
                &query,
                &embedding,
                SYNTHESIS_CANDIDATE_POOL,
                folder.as_deref(),
                exclude_folder.as_deref(),
                &RankingWeights::default(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Unlike `search`, no `is_question` gate — that heuristic exists to
        // keep plain lookups from being needlessly narrated, but a caller
        // reaching for `explain` specifically has already decided they want
        // an explanation, whatever the query looks like.
        let citations = if hits.is_empty() {
            Vec::new()
        } else {
            synthesize::synthesize(&self.embedder, &query, &hits)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        };

        let citations: Vec<JsonCitation> = citations
            .into_iter()
            .take(k)
            .map(|c| JsonCitation {
                path: c.path,
                snippet: truncate_content(&c.snippet, max_chars),
                start_line: c.start_line,
                end_line: c.end_line,
                chunk_kind: c.chunk_kind,
            })
            .collect();

        let body = json!({ "citations": citations });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Find chunks whose embedding is closest to an already-indexed chunk (identified by path + start_line, e.g. from a prior search hit). Use this to catch duplicated or near-duplicate logic elsewhere in the index before writing new code, not to search by text."
    )]
    async fn find_similar(
        &self,
        Parameters(FindSimilarParams { path, start_line, top_k, folder, max_content_chars }): Parameters<
            FindSimilarParams,
        >,
    ) -> Result<CallToolResult, McpError> {
        let k = top_k.unwrap_or(5);
        let max_chars = max_content_chars.unwrap_or(DEFAULT_MAX_CONTENT_CHARS);

        let hits = self
            .store
            .find_similar(&path, start_line, k, folder.as_deref())
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let results: Vec<JsonHit> = hits
            .iter()
            .map(|h| JsonHit {
                path: h.path.clone(),
                start_line: h.start_line,
                end_line: h.end_line,
                chunk_kind: h.chunk_kind.clone(),
                score: h.score,
                content: truncate_content(&h.content, max_chars),
            })
            .collect();

        let body = json!({ "results": results });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Check whether a doc chunk (identified by path + start_line, e.g. from a prior search hit) still matches the code it describes. Returns the closest matching code chunks in the index and whether the top match scores below a staleness threshold. Use this to catch documentation that no longer reflects the current implementation, not to find code by text."
    )]
    async fn check_doc_drift(
        &self,
        Parameters(CheckDocDriftParams {
            path,
            start_line,
            top_k,
            stale_threshold,
            folder,
            max_content_chars,
        }): Parameters<CheckDocDriftParams>,
    ) -> Result<CallToolResult, McpError> {
        let k = top_k.unwrap_or(5);
        let threshold = stale_threshold.unwrap_or(0.35);
        let max_chars = max_content_chars.unwrap_or(DEFAULT_MAX_CONTENT_CHARS);

        let (hits, likely_stale) = self
            .store
            .check_doc_drift(&path, start_line, k, threshold, folder.as_deref())
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let results: Vec<JsonHit> = hits
            .iter()
            .map(|h| JsonHit {
                path: h.path.clone(),
                start_line: h.start_line,
                end_line: h.end_line,
                chunk_kind: h.chunk_kind.clone(),
                score: h.score,
                content: truncate_content(&h.content, max_chars),
            })
            .collect();

        let body = json!({ "results": results, "likely_stale": likely_stale });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            body.to_string(),
        )]))
    }
}

#[tool_handler]
impl ServerHandler for ReferenceServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder().enable_tools().build(),
        )
        .with_server_info(Implementation::from_build_env())
        .with_instructions(
            "Read-only semantic search over the reference index (~/.reference). \
             Only covers folders a human has already opted into watching via \
             the reference desktop app; this server cannot add folders."
                .to_string(),
        )
    }
}

// Regression tests for the agent-facing MCP contract — the `search` tool's
// JSON shape, `top_k`, and `folder` scoping. Deliberately bypasses
// `ReferenceServer::new()`, which always opens the real `~/.reference` index
// (see `paths::default_db_uri`); tests build a `ReferenceServer` directly
// against a temp store instead, so running this suite never touches a real
// user's index. Real candle model, same rationale as core's tests.
#[cfg(test)]
mod tests {
    use super::*;
    use reference_core::chunk;
    use reference_core::embedding::EmbeddingModel;
    use reference_core::store::ChunkRecord;
    use tokio::sync::OnceCell;

    static EMBEDDER: OnceCell<std::sync::Arc<Embedder>> = OnceCell::const_new();

    async fn embedder() -> std::sync::Arc<Embedder> {
        EMBEDDER
            .get_or_init(|| async { std::sync::Arc::new(Embedder::load(EmbeddingModel::MiniLmL6).await.unwrap()) })
            .await
            .clone()
    }

    async fn records_for(embedder: &Embedder, extension: &str, source: &str) -> Vec<ChunkRecord> {
        let chunks = chunk::chunk_or_whole_file(extension, source);
        let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
        let embeddings = embedder.embed_batch_with_truncation(&texts).unwrap();
        chunks
            .into_iter()
            .zip(embeddings)
            .map(|(c, (embedding, truncated))| ChunkRecord {
                start_line: c.start_line,
                end_line: c.end_line,
                kind: c.kind,
                content: c.content,
                embedding,
                name: c.name.unwrap_or_default(),
                truncated,
            })
            .collect()
    }

    async fn test_server() -> (tempfile::TempDir, ReferenceServer) {
        let dir = tempfile::tempdir().expect("tempdir");
        let embedder = embedder().await;
        let store = Store::open(dir.path().to_str().unwrap()).await.expect("open store");
        let server = ReferenceServer {
            embedder,
            store: std::sync::Arc::new(store),
            tool_router: ReferenceServer::tool_router(),
        };
        (dir, server)
    }

    fn extract_json(result: &CallToolResult) -> serde_json::Value {
        let text = result.content[0].as_text().expect("search must return a text content block");
        serde_json::from_str(&text.text).expect("search result must be valid JSON")
    }

    #[tokio::test]
    async fn search_ranks_the_semantically_relevant_file_first() {
        let (_dir, server) = test_server().await;
        let embedder = embedder().await;

        server
            .store
            .replace_chunks(
                "/proj/config.rs",
                records_for(&embedder, "rs", "fn parse_config(path: &str) -> bool { path.ends_with(\".toml\") }").await,
            )
            .await
            .unwrap();
        server
            .store
            .replace_chunks(
                "/proj/widget.rs",
                records_for(&embedder, "rs", "fn render_widget(name: &str) -> String { format!(\"<widget {name}>\") }").await,
            )
            .await
            .unwrap();

        let result = server
            .search(Parameters(SearchParams {
                query: "reading a configuration file from disk".to_string(),
                top_k: None,
                folder: None,
                exclude_folder: None,
                max_content_chars: None,
            }))
            .await
            .expect("search must succeed");

        let body = extract_json(&result);
        let results = body["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0]["path"], "/proj/config.rs");
        assert!(results[0]["content"].as_str().unwrap().contains("parse_config"));
    }

    #[tokio::test]
    async fn search_top_k_limits_the_returned_result_count() {
        let (_dir, server) = test_server().await;
        let embedder = embedder().await;

        for i in 0..5 {
            server
                .store
                .replace_chunks(
                    &format!("/proj/file{i}.rs"),
                    records_for(&embedder, "rs", &format!("fn function_{i}() -> i32 {{ {i} }}")).await,
                )
                .await
                .unwrap();
        }

        let result = server
            .search(Parameters(SearchParams {
                query: "a function".to_string(),
                top_k: Some(2),
                folder: None,
                exclude_folder: None,
                max_content_chars: None,
            }))
            .await
            .expect("search must succeed");

        let body = extract_json(&result);
        assert_eq!(body["results"].as_array().unwrap().len(), 2);
    }

    /// The exact regression `docs/mcp-agent-usage.md` calls out as an actual
    /// observed failure mode: an unrelated watched folder that happens to
    /// share vocabulary with the query outranking the file that's actually
    /// relevant, unless `folder` scoping excludes it entirely.
    #[tokio::test]
    async fn search_folder_param_excludes_other_watched_folders() {
        let (_dir, server) = test_server().await;
        let embedder = embedder().await;

        server
            .store
            .replace_chunks(
                "/watched/proj_a/widget.rs",
                records_for(&embedder, "rs", "fn render_widget(name: &str) -> String { format!(\"<widget {name}>\") }").await,
            )
            .await
            .unwrap();
        server
            .store
            .replace_chunks(
                "/watched/proj_b/widget.rs",
                records_for(&embedder, "rs", "fn render_widget(name: &str) -> String { format!(\"<other-widget {name}>\") }").await,
            )
            .await
            .unwrap();

        let result = server
            .search(Parameters(SearchParams {
                query: "render a widget".to_string(),
                top_k: None,
                folder: Some("/watched/proj_a".to_string()),
                exclude_folder: None,
                max_content_chars: None,
            }))
            .await
            .expect("search must succeed");

        let body = extract_json(&result);
        let results = body["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert!(
            results.iter().all(|r| r["path"].as_str().unwrap().starts_with("/watched/proj_a/")),
            "folder scoping must exclude every hit from other watched folders: {results:?}"
        );
    }

    /// Cross-repo recall (docs/mcp-tool-ideas.md idea 3): an agent working in
    /// one watched project asking "how did I solve this elsewhere" wants the
    /// current project's own code excluded, not just some other folder
    /// included — otherwise the thing already being worked on (which best
    /// matches its own description, trivially) drowns out prior art.
    #[tokio::test]
    async fn search_exclude_folder_param_omits_the_current_project_but_keeps_others() {
        let (_dir, server) = test_server().await;
        let embedder = embedder().await;

        server
            .store
            .replace_chunks(
                "/watched/current_proj/widget.rs",
                records_for(&embedder, "rs", "fn render_widget(name: &str) -> String { format!(\"<widget {name}>\") }").await,
            )
            .await
            .unwrap();
        server
            .store
            .replace_chunks(
                "/watched/other_proj/widget.rs",
                records_for(&embedder, "rs", "fn render_widget(name: &str) -> String { format!(\"<other-widget {name}>\") }").await,
            )
            .await
            .unwrap();

        let result = server
            .search(Parameters(SearchParams {
                query: "render a widget".to_string(),
                top_k: None,
                folder: None,
                exclude_folder: Some("/watched/current_proj".to_string()),
                max_content_chars: None,
            }))
            .await
            .expect("search must succeed");

        let body = extract_json(&result);
        let results = body["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert!(
            results.iter().all(|r| !r["path"].as_str().unwrap().starts_with("/watched/current_proj/")),
            "exclude_folder must omit every hit from the excluded folder: {results:?}"
        );
        assert!(
            results.iter().any(|r| r["path"].as_str().unwrap().starts_with("/watched/other_proj/")),
            "exclude_folder must still return hits from other watched folders: {results:?}"
        );
    }

    /// The exact gap idea 4 (docs/mcp-tool-ideas.md) targets: `search`'s
    /// `is_question` heuristic skips synthesis for a bare identifier query,
    /// even though an agent asking about a specific function by name clearly
    /// wants an explanation, not just a location. `explain` must synthesize
    /// regardless.
    #[tokio::test]
    async fn explain_synthesizes_citations_for_a_non_question_shaped_query() {
        let (_dir, server) = test_server().await;
        let embedder = embedder().await;

        server
            .store
            .replace_chunks(
                "/proj/config.rs",
                records_for(&embedder, "rs", "fn parse_config(path: &str) -> bool { path.ends_with(\".toml\") }").await,
            )
            .await
            .unwrap();

        // Same literal-identifier query the `search` test proves gets zero
        // citations there.
        let result = server
            .explain(Parameters(ExplainParams {
                query: "parse_config".to_string(),
                top_k: None,
                folder: None,
                exclude_folder: None,
                max_content_chars: None,
            }))
            .await
            .expect("explain must succeed");

        let body = extract_json(&result);
        let citations = body["citations"].as_array().unwrap();
        assert!(
            !citations.is_empty(),
            "explain must synthesize citations even for a non-question-shaped query"
        );
        assert!(citations[0]["snippet"].as_str().unwrap().contains("parse_config"));
    }

    #[tokio::test]
    async fn find_similar_excludes_the_source_chunk_and_ranks_the_related_one_first() {
        let (_dir, server) = test_server().await;
        let embedder = embedder().await;

        server
            .store
            .replace_chunks(
                "/proj/lib.rs",
                records_for(
                    &embedder,
                    "rs",
                    "fn parse_config(path: &str) -> bool { path.ends_with(\".toml\") }\n\nfn load_settings(path: &str) -> bool { path.ends_with(\".json\") }\n\nfn render_widget(name: &str) -> String { format!(\"<widget {name}>\") }",
                )
                .await,
            )
            .await
            .unwrap();

        let target = server.store.find_by_name("parse_config", None).await.unwrap();
        let start_line = target[0].start_line;

        let result = server
            .find_similar(Parameters(FindSimilarParams {
                path: "/proj/lib.rs".to_string(),
                start_line,
                top_k: None,
                folder: None,
                max_content_chars: None,
            }))
            .await
            .expect("find_similar must succeed");

        let body = extract_json(&result);
        let results = body["results"].as_array().unwrap();
        assert!(
            results.iter().all(|r| !(r["path"] == "/proj/lib.rs" && r["start_line"] == start_line)),
            "source chunk must not appear in its own results: {results:?}"
        );
        assert!(results[0]["content"].as_str().unwrap().contains("load_settings"));
    }

    #[tokio::test]
    async fn check_doc_drift_flags_docs_with_no_close_code_match() {
        let (_dir, server) = test_server().await;
        let embedder = embedder().await;

        server
            .store
            .replace_chunks(
                "/proj/docs/config.md",
                records_for(
                    &embedder,
                    "md",
                    "# config loading\n\nthis project reads a toml configuration file from disk on startup and validates its fields before use.",
                )
                .await,
            )
            .await
            .unwrap();
        server
            .store
            .replace_chunks(
                "/proj/lib.rs",
                records_for(
                    &embedder,
                    "rs",
                    "fn parse_config(path: &str) -> bool { path.ends_with(\".toml\") }",
                )
                .await,
            )
            .await
            .unwrap();

        let matching = server
            .check_doc_drift(Parameters(CheckDocDriftParams {
                path: "/proj/docs/config.md".to_string(),
                start_line: 1,
                top_k: None,
                stale_threshold: Some(0.3),
                folder: None,
                max_content_chars: None,
            }))
            .await
            .expect("check_doc_drift must succeed");
        let matching_body = extract_json(&matching);
        assert_eq!(matching_body["likely_stale"], false);
        assert!(matching_body["results"][0]["content"].as_str().unwrap().contains("parse_config"));

        server
            .store
            .replace_chunks(
                "/proj/docs/marketing.md",
                records_for(
                    &embedder,
                    "md",
                    "# unrelated topic\n\nthis document discusses quarterly marketing budget allocation across regions.",
                )
                .await,
            )
            .await
            .unwrap();

        let unrelated = server
            .check_doc_drift(Parameters(CheckDocDriftParams {
                path: "/proj/docs/marketing.md".to_string(),
                start_line: 1,
                top_k: None,
                stale_threshold: Some(0.3),
                folder: None,
                max_content_chars: None,
            }))
            .await
            .expect("check_doc_drift must succeed");
        let unrelated_body = extract_json(&unrelated);
        assert_eq!(unrelated_body["likely_stale"], true);
    }

    #[tokio::test]
    async fn search_returns_citations_only_for_question_shaped_queries() {
        let (_dir, server) = test_server().await;
        let embedder = embedder().await;

        server
            .store
            .replace_chunks(
                "/proj/config.rs",
                records_for(&embedder, "rs", "fn parse_config(path: &str) -> bool { path.ends_with(\".toml\") }").await,
            )
            .await
            .unwrap();

        let question = server
            .search(Parameters(SearchParams {
                query: "how do we parse a config file".to_string(),
                top_k: None,
                folder: None,
                exclude_folder: None,
                max_content_chars: None,
            }))
            .await
            .expect("search must succeed");
        let question_body = extract_json(&question);
        assert!(
            !question_body["citations"].as_array().unwrap().is_empty(),
            "a question-shaped query with matches should synthesize citations"
        );

        let keyword = server
            .search(Parameters(SearchParams {
                query: "parse_config".to_string(),
                top_k: None,
                folder: None,
                exclude_folder: None,
                max_content_chars: None,
            }))
            .await
            .expect("search must succeed");
        let keyword_body = extract_json(&keyword);
        assert!(
            keyword_body["citations"].as_array().unwrap().is_empty(),
            "a literal identifier query is not question-shaped and should skip synthesis"
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let server = ReferenceServer::new().await?;
    let service = server.serve(stdio()).await.inspect_err(|e| {
        tracing::error!("serving error: {:?}", e);
    })?;

    service.waiting().await?;
    Ok(())
}
