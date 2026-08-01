use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::{
    cast::AsArray, types::Float32Type, BooleanArray, FixedSizeListArray, Int32Array, RecordBatch,
    RecordBatchIterator, StringArray,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::table::NewColumnTransform;
use lancedb::{connect, Connection, Table};
use serde::{Deserialize, Serialize};

use crate::embedding::EMBEDDING_DIM;
use crate::synthesize::is_question;

const TABLE_NAME: &str = "files";

// Hybrid ranking blends three signals: semantic similarity (embedding cosine,
// since stored/query embeddings are already L2-normalized), a fuzzy filename
// match (so a literal/typo'd filename match can surface a file even when its
// embedding similarity is mediocre), and literal term overlap against the
// chunk's own body text (embedding similarity alone can rank a chunk that
// merely discusses a similar topic above one that verbatim contains the
// words the query used — this catches that case without needing an extra
// model). Weights are re-split (not just fuzzy zeroed) for question-shaped
// queries, per the same reasoning as `is_question` below: filename fuzzy
// matching stops making sense against a full sentence, but content overlap
// still does, so its share of the total stays fixed across both branches.
// These are now user-adjustable defaults (see `RankingWeights` below), not
// hardwired — gap #5 in docs/feature-gaps.md.
const DEFAULT_SEMANTIC_WEIGHT: f32 = 0.55;
const DEFAULT_FUZZY_WEIGHT: f32 = 0.30;
const DEFAULT_CONTENT_MATCH_WEIGHT: f32 = 0.15;
// Soft-normalizes fuzzy-matcher's unbounded integer score into [0, 1):
// score / (score + K), so it saturates smoothly instead of clipping. Not
// user-adjustable: it's a normalization detail of the fuzzy matcher, not a
// ranking preference.
const FUZZY_SATURATION: f32 = 50.0;
// Query tokens shorter than this are dropped before content-overlap scoring
// — short words ("a", "is", "to") are too common to be a meaningful signal
// and would inflate overlap fraction for nearly any chunk.
const CONTENT_MATCH_MIN_TOKEN_LEN: usize = 3;

/// User-adjustable weights for `hybrid_search`'s three ranking signals.
/// Query-time scoring only — not baked into stored embeddings, so changing
/// these never requires a reindex (unlike gap #3/#4's schema columns).
/// `Default` matches this project's original hand-tuned constants.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RankingWeights {
    pub semantic: f32,
    pub fuzzy: f32,
    pub content: f32,
}

impl Default for RankingWeights {
    fn default() -> Self {
        Self {
            semantic: DEFAULT_SEMANTIC_WEIGHT,
            fuzzy: DEFAULT_FUZZY_WEIGHT,
            content: DEFAULT_CONTENT_MATCH_WEIGHT,
        }
    }
}

pub struct HybridHit {
    pub path: String,
    pub content: String,
    pub score: f32,
    pub start_line: i32,
    pub end_line: i32,
    pub chunk_kind: String,
    pub name: String,
    pub truncated: bool,
}

/// One chunk of a file (a function, an impl block, or the whole file as a
/// fallback), already embedded and ready to write. `name` is the chunk's own
/// identifier when `chunk::extract_name` found one (empty string otherwise —
/// stored as a plain, non-nullable column like every other field here rather
/// than `Option`, matching this schema's existing convention), and is what
/// `Store::find_by_name` matches on for exact-symbol lookup. `truncated` is
/// whether `Embedder::embed_batch_with_truncation` had to cut this chunk's
/// content down to fit the model's token limit before embedding — see gap
/// #4 in docs/feature-gaps.md: without this, a chunk long enough to get
/// silently truncated just has degraded search quality with no visible
/// reason why.
pub struct ChunkRecord {
    pub start_line: i32,
    pub end_line: i32,
    pub kind: String,
    pub content: String,
    pub embedding: Vec<f32>,
    pub name: String,
    pub truncated: bool,
}

pub struct Store {
    table: Table,
}

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("path", DataType::Utf8, false),
        Field::new("start_line", DataType::Int32, false),
        Field::new("end_line", DataType::Int32, false),
        Field::new("chunk_kind", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("truncated", DataType::Boolean, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                EMBEDDING_DIM as i32,
            ),
            false,
        ),
    ]))
}

// Columns added to `schema()` after the table was first shipped, paired with
// a SQL literal default for any row written before that column existed
// (`''` for the empty-string convention `name` already uses for "no
// identifier found", `false` for "not truncated" — matching what a fresh
// index would have produced for old content anyway). `Store::open` backfills
// these into an on-disk table that predates them instead of erroring: gap
// #3 and #4 in docs/feature-gaps.md each added one of these columns, and
// until now the only fix for an old table was `rm -rf ~/.reference/index`
// and a full reindex — fine for a dev rebuilding from source every time, not
// something a real installed user updating across a release should have to
// discover from a `missing name column` error.
const MIGRATABLE_COLUMNS: &[(&str, &str)] = &[("name", "''"), ("truncated", "false")];

async fn migrate_schema(table: &Table) -> Result<()> {
    let current = table.schema().await?;
    let missing: Vec<(String, String)> = MIGRATABLE_COLUMNS
        .iter()
        .filter(|(name, _)| current.field_with_name(name).is_err())
        .map(|(name, default)| (name.to_string(), default.to_string()))
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    let names: Vec<&str> = missing.iter().map(|(n, _)| n.as_str()).collect();
    println!("migrating index schema: adding column(s) {} to existing table", names.join(", "));
    table.add_columns(NewColumnTransform::SqlExpressions(missing), None).await?;
    Ok(())
}

impl Store {
    pub async fn open(db_uri: &str) -> Result<Self> {
        let db: Connection = connect(db_uri).execute().await?;

        let existing = db.table_names().execute().await?;
        let table = if existing.iter().any(|n| n == TABLE_NAME) {
            let table = db.open_table(TABLE_NAME).execute().await?;
            migrate_schema(&table).await?;
            table
        } else {
            db.create_empty_table(TABLE_NAME, schema()).execute().await?
        };

        Ok(Self { table })
    }

    /// Replaces every chunk row for `path` with `chunks`. Chunking is a
    /// one-to-many relationship (one file -> N chunk rows), which
    /// `merge_insert`'s upsert semantics can't express on their own: it can
    /// add/update rows but has no way to notice that a function was deleted
    /// and its row should disappear too. So every re-index deletes all
    /// existing rows for the exact path first, then inserts the freshly
    /// parsed chunks via `merge_insert` (keyed on path + start_line) — still
    /// going through `merge_insert` per this project's write rule, just
    /// preceded by an explicit delete so stale chunks can't linger.
    pub async fn replace_chunks(&self, path: &str, chunks: Vec<ChunkRecord>) -> Result<()> {
        let escaped = path.replace('\'', "''");
        self.table.delete(&format!("path = '{escaped}'")).await?;

        if chunks.is_empty() {
            return Ok(());
        }

        let schema = schema();
        let n = chunks.len();
        let mut paths = Vec::with_capacity(n);
        let mut start_lines = Vec::with_capacity(n);
        let mut end_lines = Vec::with_capacity(n);
        let mut kinds = Vec::with_capacity(n);
        let mut contents = Vec::with_capacity(n);
        let mut names = Vec::with_capacity(n);
        let mut truncated = Vec::with_capacity(n);
        let mut embeddings = Vec::with_capacity(n);
        for c in chunks {
            paths.push(path.to_string());
            start_lines.push(c.start_line);
            end_lines.push(c.end_line);
            kinds.push(c.kind);
            contents.push(c.content);
            names.push(c.name);
            truncated.push(c.truncated);
            embeddings.push(Some(c.embedding.into_iter().map(Some).collect::<Vec<_>>()));
        }

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(paths)),
                Arc::new(Int32Array::from(start_lines)),
                Arc::new(Int32Array::from(end_lines)),
                Arc::new(StringArray::from(kinds)),
                Arc::new(StringArray::from(contents)),
                Arc::new(StringArray::from(names)),
                Arc::new(BooleanArray::from(truncated)),
                Arc::new(FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                    embeddings,
                    EMBEDDING_DIM as i32,
                )),
            ],
        )?;
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema.clone());

        let mut merge_insert = self.table.merge_insert(&["path", "start_line"]);
        merge_insert
            .when_matched_update_all(None)
            .when_not_matched_insert_all();
        merge_insert
            .execute(Box::new(reader))
            .await
            .context("merge_insert chunk upsert failed")?;

        Ok(())
    }

    /// Ranks every indexed chunk by a blend of semantic similarity (against
    /// `query_embedding`) and fuzzy filename match (against `query_text`),
    /// returning the top `k`. Scores every row in the table directly rather
    /// than going through LanceDB's ANN index, which is simple and exact at
    /// the scale of a personal file index; revisit if that stops being true
    /// now that chunking means several rows per file instead of one.
    ///
    /// `folder`, when set, scopes the scan to rows under that path — the
    /// same `path LIKE '<folder>/%'` predicate `delete_under` uses, applied
    /// at the query level via `only_if` so out-of-scope rows are never
    /// fetched at all, not filtered out after the fact. Without this, a
    /// query about one watched project can be outranked by an unrelated
    /// file from a completely different watched folder that merely shares
    /// some vocabulary — an actual observed failure mode, not a
    /// hypothetical one (see docs/mcp-agent-usage.md).
    ///
    /// `exclude_folder`, when set, is the inverse: scans every watched
    /// folder *except* that one. For "how did I solve this in a different
    /// project" recall — the current project's own code isn't prior art for
    /// itself, so an agent working in one repo wants other watched repos
    /// only, not the one it's already sitting in. Both filters can combine
    /// (`only_if` ANDs them), though in practice only one is set at a time.
    pub async fn hybrid_search(
        &self,
        query_text: &str,
        query_embedding: &[f32],
        k: usize,
        folder: Option<&str>,
        exclude_folder: Option<&str>,
        weights: &RankingWeights,
    ) -> Result<Vec<HybridHit>> {
        // A long-lived handle (the MCP server, kept alive for a whole agent
        // session) otherwise never sees rows committed by another process
        // (the app, reindexing a folder) — see docs/mcp-agent-usage.md's
        // "reindex may need a session restart" caveat. Cheap: just a
        // manifest read, not a rescan of table contents.
        self.table.checkout_latest().await?;
        let mut query = self.table.query().select(Select::Columns(vec![
            "path".to_string(),
            "start_line".to_string(),
            "end_line".to_string(),
            "chunk_kind".to_string(),
            "content".to_string(),
            "name".to_string(),
            "truncated".to_string(),
            "embedding".to_string(),
        ]));
        if let Some(folder) = folder {
            let folder = folder.trim_end_matches('/').replace('\'', "''");
            query = query.only_if(format!("path LIKE '{folder}/%'"));
        }
        if let Some(exclude_folder) = exclude_folder {
            let exclude_folder = exclude_folder.trim_end_matches('/').replace('\'', "''");
            query = query.only_if(format!("path NOT LIKE '{exclude_folder}/%'"));
        }
        let batches = query.execute().await?.try_collect::<Vec<_>>().await?;

        let matcher = SkimMatcherV2::default();
        let mut hits = Vec::new();

        // Fuzzy filename matching assumes a short, filename-shaped query
        // ("store.rs", "auth"). Run it against a full natural-language
        // question instead and it degenerates into a coincidental
        // character-subsequence match against whatever filename happens to
        // be long enough to contain the query's letters in order, which can
        // outrank the file that's actually semantically relevant. Question-
        // shaped queries (per the same heuristic `synthesize` uses to decide
        // whether to answer at all) skip the fuzzy component entirely and
        // rank on semantic similarity alone.
        let fuzzy_weight = if is_question(query_text) { 0.0 } else { weights.fuzzy };
        let semantic_weight = if is_question(query_text) {
            weights.semantic + weights.fuzzy
        } else {
            weights.semantic
        };
        let query_terms = content_match_terms(query_text);

        for batch in &batches {
            let paths = batch
                .column_by_name("path")
                .context("missing path column")?
                .as_string::<i32>();
            let start_lines = batch
                .column_by_name("start_line")
                .context("missing start_line column")?
                .as_primitive::<arrow_array::types::Int32Type>();
            let end_lines = batch
                .column_by_name("end_line")
                .context("missing end_line column")?
                .as_primitive::<arrow_array::types::Int32Type>();
            let chunk_kinds = batch
                .column_by_name("chunk_kind")
                .context("missing chunk_kind column")?
                .as_string::<i32>();
            let contents = batch
                .column_by_name("content")
                .context("missing content column")?
                .as_string::<i32>();
            let names = batch
                .column_by_name("name")
                .context("missing name column")?
                .as_string::<i32>();
            let truncated_flags = batch
                .column_by_name("truncated")
                .context("missing truncated column")?
                .as_boolean();
            let embeddings = batch
                .column_by_name("embedding")
                .context("missing embedding column")?
                .as_fixed_size_list();

            for i in 0..batch.num_rows() {
                let path = paths.value(i).to_string();
                let content = contents.value(i).to_string();
                let row_embedding = embeddings.value(i);
                let row_embedding = row_embedding.as_primitive::<Float32Type>().values();

                let semantic_sim = dot(query_embedding, row_embedding).max(0.0);

                let fuzzy_sim = if fuzzy_weight == 0.0 {
                    0.0
                } else {
                    let filename = Path::new(&path)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&path);
                    let fuzzy_raw = matcher.fuzzy_match(filename, query_text).unwrap_or(0) as f32;
                    fuzzy_raw / (fuzzy_raw + FUZZY_SATURATION)
                };

                let content_match_sim = content_match_score(&query_terms, &content);

                let score = semantic_weight * semantic_sim
                    + fuzzy_weight * fuzzy_sim
                    + weights.content * content_match_sim;
                hits.push(HybridHit {
                    path,
                    content,
                    score,
                    start_line: start_lines.value(i),
                    end_line: end_lines.value(i),
                    chunk_kind: chunk_kinds.value(i).to_string(),
                    name: names.value(i).to_string(),
                    truncated: truncated_flags.value(i),
                });
            }
        }

        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(k);
        Ok(hits)
    }

    /// Exact, case-sensitive lookup by a chunk's own identifier (function,
    /// method, class, ... name — see `chunk::extract_name`) — the "find this
    /// exact function by name" path `hybrid_search`'s fuzzy/semantic/overlap
    /// blend can't guarantee on its own, since it always ranks by
    /// similarity, never by literal identity. An equality filter pushed down
    /// via `only_if` rather than a table scan, so this stays fast regardless
    /// of index size — no embedding or per-row scoring involved. Every hit
    /// gets `score = 1.0`: there's no ranking question once a match is
    /// exact, they're all equally "the thing you asked for" (e.g. same
    /// method name implemented on several types).
    pub async fn find_by_name(&self, name: &str, folder: Option<&str>) -> Result<Vec<HybridHit>> {
        // See the same call in `hybrid_search` above.
        self.table.checkout_latest().await?;
        let escaped = name.replace('\'', "''");
        let mut predicate = format!("name = '{escaped}'");
        if let Some(folder) = folder {
            let folder = folder.trim_end_matches('/').replace('\'', "''");
            predicate = format!("{predicate} AND path LIKE '{folder}/%'");
        }

        let batches = self
            .table
            .query()
            .select(Select::Columns(vec![
                "path".to_string(),
                "start_line".to_string(),
                "end_line".to_string(),
                "chunk_kind".to_string(),
                "content".to_string(),
                "name".to_string(),
                "truncated".to_string(),
            ]))
            .only_if(predicate)
            .execute()
            .await?
            .try_collect::<Vec<_>>()
            .await?;

        let mut hits = Vec::new();
        for batch in &batches {
            let paths = batch.column_by_name("path").context("missing path column")?.as_string::<i32>();
            let start_lines = batch
                .column_by_name("start_line")
                .context("missing start_line column")?
                .as_primitive::<arrow_array::types::Int32Type>();
            let end_lines = batch
                .column_by_name("end_line")
                .context("missing end_line column")?
                .as_primitive::<arrow_array::types::Int32Type>();
            let chunk_kinds = batch
                .column_by_name("chunk_kind")
                .context("missing chunk_kind column")?
                .as_string::<i32>();
            let contents = batch
                .column_by_name("content")
                .context("missing content column")?
                .as_string::<i32>();
            let names = batch.column_by_name("name").context("missing name column")?.as_string::<i32>();
            let truncated_flags = batch
                .column_by_name("truncated")
                .context("missing truncated column")?
                .as_boolean();

            for i in 0..batch.num_rows() {
                hits.push(HybridHit {
                    path: paths.value(i).to_string(),
                    content: contents.value(i).to_string(),
                    score: 1.0,
                    start_line: start_lines.value(i),
                    end_line: end_lines.value(i),
                    chunk_kind: chunk_kinds.value(i).to_string(),
                    name: names.value(i).to_string(),
                    truncated: truncated_flags.value(i),
                });
            }
        }

        Ok(hits)
    }

    /// Finds chunks whose embedding is closest to the chunk at `path` +
    /// `start_line` — "what else in the index looks like this," not "what
    /// matches this text." No query embedding to compute: the target
    /// chunk's own stored vector is looked up first, then reused as the
    /// query vector against every other row's embedding, same dot-product
    /// scoring `hybrid_search` uses for its semantic component alone (no
    /// fuzzy/content-overlap blend here — there's no query text to match
    /// against a filename or content, only a vector to compare against
    /// other vectors). The source chunk itself is excluded from its own
    /// results by path + start_line, not by score, since a chunk is always
    /// its own top match at similarity 1.0.
    pub async fn find_similar(
        &self,
        path: &str,
        start_line: i32,
        k: usize,
        folder: Option<&str>,
    ) -> Result<Vec<HybridHit>> {
        let target_embedding = self.chunk_embedding(path, start_line).await?;
        let mut hits = self.scan_by_embedding(&target_embedding, path, start_line, folder).await?;
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(k);
        Ok(hits)
    }

    /// Checks whether a doc chunk still matches the code it describes.
    /// Markdown isn't chunked at function granularity (see
    /// `docs/code-aware-chunking.md`), so a doc file's chunks always carry
    /// `chunk_kind = "file"` — that's the signal used here to tell "the doc
    /// itself" apart from "actual code constructs" among the candidates,
    /// so a prose doc chunk is compared only against real functions/types/
    /// etc., not against other prose that happens to share vocabulary.
    /// `likely_stale` is a threshold read on the top match's score, not a
    /// guarantee: a low score means nothing in the index reads as
    /// semantically close to this doc chunk anymore, which is what code
    /// drifting out from under stale documentation looks like.
    pub async fn check_doc_drift(
        &self,
        path: &str,
        start_line: i32,
        k: usize,
        stale_threshold: f32,
        folder: Option<&str>,
    ) -> Result<(Vec<HybridHit>, bool)> {
        let target_embedding = self.chunk_embedding(path, start_line).await?;
        let mut hits = self.scan_by_embedding(&target_embedding, path, start_line, folder).await?;
        hits.retain(|h| h.chunk_kind != "file");
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(k);
        let likely_stale = hits.first().map(|h| h.score < stale_threshold).unwrap_or(true);
        Ok((hits, likely_stale))
    }

    /// Looks up the stored embedding for the chunk at `path` + `start_line`
    /// — shared by every "compare this chunk against the rest of the index"
    /// tool (`find_similar`, `check_doc_drift`) instead of each re-querying
    /// for it.
    async fn chunk_embedding(&self, path: &str, start_line: i32) -> Result<Vec<f32>> {
        // Shared by `find_similar`/`check_doc_drift` — see the same call in
        // `hybrid_search` above.
        self.table.checkout_latest().await?;
        let escaped_path = path.replace('\'', "''");
        let batches = self
            .table
            .query()
            .select(Select::Columns(vec!["embedding".to_string()]))
            .only_if(format!("path = '{escaped_path}' AND start_line = {start_line}"))
            .execute()
            .await?
            .try_collect::<Vec<_>>()
            .await?;

        batches
            .iter()
            .find(|b| b.num_rows() > 0)
            .map(|b| {
                let embeddings = b
                    .column_by_name("embedding")
                    .context("missing embedding column")?
                    .as_fixed_size_list();
                let row = embeddings.value(0);
                Ok::<Vec<f32>, anyhow::Error>(row.as_primitive::<Float32Type>().values().to_vec())
            })
            .context("no chunk found at that path/start_line")?
    }

    /// Scores every indexed chunk against `target_embedding` by plain dot
    /// product (both sides already L2-normalized), excluding the chunk at
    /// `exclude_path` + `exclude_start_line` — a chunk is always its own
    /// top match at similarity 1.0, so the source chunk is dropped by
    /// identity rather than by score. Returned unsorted/untruncated; callers
    /// apply their own filtering, sort, and `k` limit on top of this.
    async fn scan_by_embedding(
        &self,
        target_embedding: &[f32],
        exclude_path: &str,
        exclude_start_line: i32,
        folder: Option<&str>,
    ) -> Result<Vec<HybridHit>> {
        let mut query = self.table.query().select(Select::Columns(vec![
            "path".to_string(),
            "start_line".to_string(),
            "end_line".to_string(),
            "chunk_kind".to_string(),
            "content".to_string(),
            "name".to_string(),
            "truncated".to_string(),
            "embedding".to_string(),
        ]));
        if let Some(folder) = folder {
            let folder = folder.trim_end_matches('/').replace('\'', "''");
            query = query.only_if(format!("path LIKE '{folder}/%'"));
        }
        let batches = query.execute().await?.try_collect::<Vec<_>>().await?;

        let mut hits = Vec::new();
        for batch in &batches {
            let paths = batch.column_by_name("path").context("missing path column")?.as_string::<i32>();
            let start_lines = batch
                .column_by_name("start_line")
                .context("missing start_line column")?
                .as_primitive::<arrow_array::types::Int32Type>();
            let end_lines = batch
                .column_by_name("end_line")
                .context("missing end_line column")?
                .as_primitive::<arrow_array::types::Int32Type>();
            let chunk_kinds = batch
                .column_by_name("chunk_kind")
                .context("missing chunk_kind column")?
                .as_string::<i32>();
            let contents = batch
                .column_by_name("content")
                .context("missing content column")?
                .as_string::<i32>();
            let names = batch.column_by_name("name").context("missing name column")?.as_string::<i32>();
            let truncated_flags = batch
                .column_by_name("truncated")
                .context("missing truncated column")?
                .as_boolean();
            let embeddings = batch
                .column_by_name("embedding")
                .context("missing embedding column")?
                .as_fixed_size_list();

            for i in 0..batch.num_rows() {
                let row_path = paths.value(i).to_string();
                let row_start_line = start_lines.value(i);
                if row_path == exclude_path && row_start_line == exclude_start_line {
                    continue;
                }

                let row_embedding = embeddings.value(i);
                let row_embedding = row_embedding.as_primitive::<Float32Type>().values();
                let score = dot(target_embedding, row_embedding).max(0.0);

                hits.push(HybridHit {
                    path: row_path,
                    content: contents.value(i).to_string(),
                    score,
                    start_line: row_start_line,
                    end_line: end_lines.value(i),
                    chunk_kind: chunk_kinds.value(i).to_string(),
                    name: names.value(i).to_string(),
                    truncated: truncated_flags.value(i),
                });
            }
        }

        Ok(hits)
    }

    /// Deletes every indexed row whose path is under `folder`. `merge_insert`
    /// only ever adds/updates rows, so without this, un-watching a folder
    /// would stop it from being *updated* but leave everything already
    /// indexed permanently searchable — not what "un-watch" should mean.
    pub async fn delete_under(&self, folder: &str) -> Result<()> {
        let folder = folder.trim_end_matches('/').replace('\'', "''");
        let predicate = format!("path LIKE '{folder}/%'");
        self.table.delete(&predicate).await?;
        Ok(())
    }

    /// Total row count across the whole table. Chunking turned this from
    /// "one row per file" into "one row per function/class", so this is the
    /// number to watch if `hybrid_search`'s full-table scan ever needs
    /// revisiting per the note in docs/code-aware-chunking.md.
    pub async fn row_count(&self) -> Result<usize> {
        Ok(self.table.count_rows(None).await?)
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

// Regression tests for the store/upsert layer, run against a real candle
// model rather than stubbed embeddings — the point is to catch real
// embedding/merge_insert regressions before a release build, not just
// exercise the arrow plumbing with fake vectors. Model weights are
// downloaded once (hf-hub caches to disk), so the first run is slow and
// needs network; subsequent runs load from the local cache.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk;
    use crate::embedding::{Embedder, EmbeddingModel};
    use lancedb::index::Index;
    use tokio::sync::OnceCell;

    // Loaded once and shared across tests in this binary: reloading the
    // model per-test would multiply an already-slow (network + safetensors
    // parse) operation for no benefit, since the model itself is read-only.
    static EMBEDDER: OnceCell<Embedder> = OnceCell::const_new();

    async fn embedder() -> &'static Embedder {
        EMBEDDER
            .get_or_init(|| async { Embedder::load(EmbeddingModel::MiniLmL6).await.unwrap() })
            .await
    }

    async fn open_temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path().to_str().unwrap()).await.expect("open store");
        (dir, store)
    }

    /// Chunks `source` with the real tree-sitter chunker and embeds every
    /// chunk with the real model — mirrors what `watcher::index_file` does,
    /// without needing filesystem I/O.
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

    #[tokio::test]
    async fn open_migrates_a_table_missing_name_and_truncated_columns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_uri = dir.path().to_str().unwrap();

        // Simulates an on-disk index built before gap #3/#4 (docs/feature-gaps.md)
        // added the `name`/`truncated` columns — the exact shape `Store::open`
        // has to tolerate instead of erroring with `missing name column`.
        let old_schema = Arc::new(Schema::new(vec![
            Field::new("path", DataType::Utf8, false),
            Field::new("start_line", DataType::Int32, false),
            Field::new("end_line", DataType::Int32, false),
            Field::new("chunk_kind", DataType::Utf8, false),
            Field::new("content", DataType::Utf8, false),
            Field::new(
                "embedding",
                DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), EMBEDDING_DIM as i32),
                false,
            ),
        ]));

        let embedder = embedder().await;
        let embedding = embedder.embed("fn old() {}").unwrap();
        let batch = RecordBatch::try_new(
            old_schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["/proj/old.rs"])),
                Arc::new(Int32Array::from(vec![1])),
                Arc::new(Int32Array::from(vec![5])),
                Arc::new(StringArray::from(vec!["function"])),
                Arc::new(StringArray::from(vec!["fn old() {}"])),
                Arc::new(FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                    vec![Some(embedding.into_iter().map(Some).collect::<Vec<_>>())],
                    EMBEDDING_DIM as i32,
                )),
            ],
        )
        .unwrap();
        let reader = RecordBatchIterator::new(vec![Ok(batch)], old_schema);

        let db = connect(db_uri).execute().await.unwrap();
        db.create_table(TABLE_NAME, reader).execute().await.unwrap();

        // Reopening through `Store::open` (not raw `db.open_table`) must add
        // the missing columns rather than leaving `hybrid_search`/
        // `find_by_name` erroring against a table shaped like an old release.
        let store = Store::open(db_uri).await.expect("open should migrate, not error");

        let hits = store.find_by_name("old", None).await.unwrap();
        assert!(hits.is_empty(), "pre-migration row has no name, so it can't be found by name");

        let query_embedding = embedder.embed("old function").unwrap();
        let hits = store
            .hybrid_search("old function", &query_embedding, 5, None, None, &RankingWeights::default())
            .await
            .expect("hybrid_search must not error against a migrated table");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "", "backfilled default for a pre-migration row");
        assert!(!hits[0].truncated, "backfilled default for a pre-migration row");
    }

    #[tokio::test]
    async fn hybrid_search_and_find_by_name_roundtrip() {
        let embedder = embedder().await;
        let (_dir, store) = open_temp_store().await;

        let source = r#"
fn parse_config(path: &str) -> bool {
    // reads a toml config file from disk and validates its fields
    path.ends_with(".toml")
}

fn render_widget(name: &str) -> String {
    // draws a ui widget to the screen given its name
    format!("<widget {name}>")
}
"#;
        let records = records_for(embedder, "rs", source).await;
        store.replace_chunks("/proj/lib.rs", records).await.unwrap();

        let query_embedding = embedder.embed("reading a configuration file from disk").unwrap();
        let hits = store
            .hybrid_search(
                "reading a configuration file from disk",
                &query_embedding,
                5,
                None,
                None,
                &RankingWeights::default(),
            )
            .await
            .unwrap();

        assert!(!hits.is_empty(), "expected at least one hit");
        assert_eq!(hits[0].name, "parse_config", "semantic search should rank the config-parsing function first");

        let by_name = store.find_by_name("render_widget", None).await.unwrap();
        assert_eq!(by_name.len(), 1);
        assert!(by_name[0].content.contains("render_widget"));

        assert!(store.find_by_name("does_not_exist", None).await.unwrap().is_empty());
    }

    /// Targets the risk CLAUDE.md calls out directly: `replace_chunks` has
    /// to delete-then-merge_insert, because merge_insert alone can't express
    /// "this chunk's underlying function was deleted". A regression here
    /// (e.g. someone changing this back to a bare merge_insert without the
    /// preceding delete) would leave stale, deleted-function chunks
    /// permanently searchable.
    #[tokio::test]
    async fn replace_chunks_upsert_removes_chunks_for_deleted_functions() {
        let embedder = embedder().await;
        let (_dir, store) = open_temp_store().await;

        let before = r#"
fn keep_me() -> i32 { 1 }

fn delete_me() -> i32 { 2 }
"#;
        let records = records_for(embedder, "rs", before).await;
        // No preamble chunk: everything before the first `fn` is blank, and
        // `chunk_or_whole_file` only emits a preamble chunk for non-blank
        // leading content (see `chunk::chunk_with`).
        assert_eq!(records.len(), 2, "2 functions, no preamble");
        store.replace_chunks("/proj/edit.rs", records).await.unwrap();
        assert_eq!(store.row_count().await.unwrap(), 2);

        let after = r#"
fn keep_me() -> i32 { 1 }
"#;
        let records = records_for(embedder, "rs", after).await;
        store.replace_chunks("/proj/edit.rs", records).await.unwrap();

        assert_eq!(
            store.row_count().await.unwrap(),
            1,
            "stale chunk for the deleted function must not survive re-indexing"
        );
        assert!(store.find_by_name("delete_me", None).await.unwrap().is_empty());
        assert_eq!(store.find_by_name("keep_me", None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn find_similar_excludes_self_and_ranks_by_embedding_distance() {
        let embedder = embedder().await;
        let (_dir, store) = open_temp_store().await;

        let source = r#"
fn parse_config(path: &str) -> bool {
    // reads a toml config file from disk and validates its fields
    path.ends_with(".toml")
}

fn load_settings(path: &str) -> bool {
    // reads a json settings file from disk and validates its fields
    path.ends_with(".json")
}

fn render_widget(name: &str) -> String {
    // draws a ui widget to the screen given its name
    format!("<widget {name}>")
}
"#;
        let records = records_for(embedder, "rs", source).await;
        store.replace_chunks("/proj/lib.rs", records).await.unwrap();

        let target = store.find_by_name("parse_config", None).await.unwrap();
        assert_eq!(target.len(), 1);
        let target_start_line = target[0].start_line;

        let hits = store.find_similar("/proj/lib.rs", target_start_line, 5, None).await.unwrap();

        assert!(
            hits.iter().all(|h| !(h.path == "/proj/lib.rs" && h.start_line == target_start_line)),
            "the source chunk must not appear in its own similarity results"
        );
        assert_eq!(hits[0].name, "load_settings", "the other file-reading function should rank closer than the unrelated widget renderer");
    }

    #[tokio::test]
    async fn check_doc_drift_flags_a_doc_chunk_with_no_close_code_match() {
        let embedder = embedder().await;
        let (_dir, store) = open_temp_store().await;

        // A doc chunk (markdown falls back to one whole-file "file" chunk,
        // see chunk::chunk_or_whole_file) that closely describes real code
        // also present in the index.
        let doc = "# config loading\n\nthis project reads a toml configuration file from disk on startup and validates its fields before use.";
        let doc_records = records_for(embedder, "md", doc).await;
        assert_eq!(doc_records.len(), 1, "markdown has no code-aware chunker, should fall back to one file chunk");
        store.replace_chunks("/proj/docs/config.md", doc_records).await.unwrap();

        let code = r#"
fn parse_config(path: &str) -> bool {
    // reads a toml config file from disk and validates its fields
    path.ends_with(".toml")
}
"#;
        store.replace_chunks("/proj/lib.rs", records_for(embedder, "rs", code).await).await.unwrap();

        let (matches, likely_stale) = store.check_doc_drift("/proj/docs/config.md", 1, 5, 0.3, None).await.unwrap();
        assert!(!matches.is_empty(), "the doc should still match the code it describes");
        assert_eq!(matches[0].name, "parse_config");
        assert!(!likely_stale, "a doc whose top code match scores above the threshold should not be flagged");

        // An unrelated doc with nothing matching in the index at all should
        // come back with a low top score and get flagged.
        let unrelated_doc = "# unrelated topic\n\nthis document discusses quarterly marketing budget allocation across regions.";
        let unrelated_records = records_for(embedder, "md", unrelated_doc).await;
        store.replace_chunks("/proj/docs/marketing.md", unrelated_records).await.unwrap();

        let (_matches, likely_stale) = store.check_doc_drift("/proj/docs/marketing.md", 1, 5, 0.3, None).await.unwrap();
        assert!(likely_stale, "a doc chunk with no semantically close code should be flagged as likely stale");
    }

    #[tokio::test]
    async fn delete_under_only_removes_matching_folder() {
        let embedder = embedder().await;
        let (_dir, store) = open_temp_store().await;

        store
            .replace_chunks("/watched/proj/a.rs", records_for(embedder, "rs", "fn a() {}").await)
            .await
            .unwrap();
        store
            .replace_chunks("/watched/other/b.rs", records_for(embedder, "rs", "fn b() {}").await)
            .await
            .unwrap();

        store.delete_under("/watched/proj").await.unwrap();

        assert!(store.find_by_name("a", None).await.unwrap().is_empty());
        assert_eq!(store.find_by_name("b", None).await.unwrap().len(), 1);
    }

    /// Known gap from CLAUDE.md: "merge_insert has a reported (possibly
    /// fixed) issue when called on a table that already has a vector index
    /// built. test this specifically once indexing is running past the
    /// prototype stage, not just on a fresh unindexed table." This builds a
    /// real IVF-PQ index (needs enough rows to train), then runs the same
    /// upsert path a live edit would trigger, and confirms the index isn't
    /// left in a state that breaks search or hides the update.
    #[tokio::test]
    async fn hybrid_search_exclude_folder_omits_that_folder_but_keeps_others() {
        let embedder = embedder().await;
        let (_dir, store) = open_temp_store().await;

        store
            .replace_chunks(
                "/watched/current_proj/widget.rs",
                records_for(embedder, "rs", "fn render_widget(name: &str) -> String { format!(\"<widget {name}>\") }").await,
            )
            .await
            .unwrap();
        store
            .replace_chunks(
                "/watched/other_proj/widget.rs",
                records_for(embedder, "rs", "fn render_widget(name: &str) -> String { format!(\"<other-widget {name}>\") }").await,
            )
            .await
            .unwrap();

        let query_embedding = embedder.embed("render a widget").unwrap();
        let hits = store
            .hybrid_search(
                "render a widget",
                &query_embedding,
                5,
                None,
                Some("/watched/current_proj"),
                &RankingWeights::default(),
            )
            .await
            .unwrap();

        assert!(!hits.is_empty());
        assert!(
            hits.iter().all(|h| !h.path.starts_with("/watched/current_proj/")),
            "exclude_folder must omit every hit from the excluded folder: {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        assert!(hits.iter().any(|h| h.path.starts_with("/watched/other_proj/")));
    }

    #[tokio::test]
    async fn merge_insert_after_vector_index_is_built() {
        let embedder = embedder().await;
        let (_dir, store) = open_temp_store().await;

        // Filler rows purely to give IVF-PQ enough vectors to train a
        // partition on — content is irrelevant, only the row count and the
        // fact they're real (non-degenerate) embeddings matters.
        let filler_texts: Vec<String> = (0..300).map(|i| format!("filler document number {i} about topic {}", i % 17)).collect();
        let filler_embeddings = embedder.embed_batch(&filler_texts).unwrap();
        let filler_records: Vec<ChunkRecord> = filler_texts
            .iter()
            .zip(filler_embeddings)
            .enumerate()
            .map(|(i, (text, embedding))| ChunkRecord {
                start_line: i as i32,
                end_line: i as i32,
                kind: "file".to_string(),
                content: text.clone(),
                embedding,
                name: String::new(),
                truncated: false,
            })
            .collect();
        store.replace_chunks("/filler/doc.txt", filler_records).await.unwrap();

        let before = r#"
fn keep_me() -> i32 { 1 }

fn delete_me() -> i32 { 2 }
"#;
        store
            .replace_chunks("/proj/edit.rs", records_for(embedder, "rs", before).await)
            .await
            .unwrap();

        store
            .table
            .create_index(&["embedding"], Index::Auto)
            .execute()
            .await
            .expect("vector index build should succeed");

        // Same upsert a live file edit triggers, now against an indexed table.
        let after = r#"
fn keep_me() -> i32 { 1 }
"#;
        store
            .replace_chunks("/proj/edit.rs", records_for(embedder, "rs", after).await)
            .await
            .expect("merge_insert upsert must still succeed once a vector index exists");

        assert!(
            store.find_by_name("delete_me", None).await.unwrap().is_empty(),
            "stale chunk must not survive an upsert against an indexed table"
        );
        assert_eq!(store.find_by_name("keep_me", None).await.unwrap().len(), 1);

        let query_embedding = embedder.embed("keep_me").unwrap();
        let hits = store
            .hybrid_search("keep_me", &query_embedding, 5, None, None, &RankingWeights::default())
            .await
            .unwrap();
        assert!(
            hits.iter().any(|h| h.name == "keep_me"),
            "post-index upsert must remain searchable"
        );
    }
}

/// Lowercased, deduplicated query words worth checking for literal presence
/// in a chunk's content — short/common tokens are dropped since they'd match
/// almost any chunk and add noise rather than signal.
fn content_match_terms(query_text: &str) -> Vec<String> {
    let mut terms: Vec<String> = query_text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() >= CONTENT_MATCH_MIN_TOKEN_LEN)
        .map(|s| s.to_string())
        .collect();
    terms.sort_unstable();
    terms.dedup();
    terms
}

/// Fraction of `query_terms` that appear as whole words (case-insensitive)
/// in `content` — a lightweight complement to embedding similarity, which
/// can rank a chunk that merely discusses a similar topic above one that
/// verbatim contains the words actually searched for.
///
/// Word-boundary-aware, not a raw substring check: a naive `content.
/// contains("build")` matches inside "rebuild", "builds", "building" too,
/// which turned this into noise rather than signal for any query
/// containing a short, common word — a query for "how does X build Y"
/// coincidentally boosted files whose only connection was talking about
/// `cargo build`/"rebuild" in an unrelated comment.
fn content_match_score(query_terms: &[String], content: &str) -> f32 {
    if query_terms.is_empty() {
        return 0.0;
    }
    let content_lower = content.to_lowercase();
    let content_words: std::collections::HashSet<&str> = content_lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    let matched = query_terms
        .iter()
        .filter(|t| content_words.contains(t.as_str()))
        .count();
    matched as f32 / query_terms.len() as f32
}
