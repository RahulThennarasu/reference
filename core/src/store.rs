use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::{
    cast::AsArray, types::Float32Type, FixedSizeListArray, RecordBatch, RecordBatchIterator,
    StringArray,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::{connect, Connection, Table};

use crate::embedding::EMBEDDING_DIM;

const TABLE_NAME: &str = "files";

// Hybrid ranking blends a semantic similarity score (embedding cosine
// similarity, since stored/query embeddings are already L2-normalized) with
// a fuzzy filename match score, so a literal/typo'd filename match can
// surface a file even when its embedding similarity is mediocre.
const SEMANTIC_WEIGHT: f32 = 0.65;
const FUZZY_WEIGHT: f32 = 0.35;
// Soft-normalizes fuzzy-matcher's unbounded integer score into [0, 1):
// score / (score + K), so it saturates smoothly instead of clipping.
const FUZZY_SATURATION: f32 = 50.0;

pub struct HybridHit {
    pub path: String,
    pub content: String,
    pub score: f32,
}

pub struct Store {
    table: Table,
}

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("path", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, false),
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

impl Store {
    pub async fn open(db_uri: &str) -> Result<Self> {
        let db: Connection = connect(db_uri).execute().await?;

        let existing = db.table_names().execute().await?;
        let table = if existing.iter().any(|n| n == TABLE_NAME) {
            db.open_table(TABLE_NAME).execute().await?
        } else {
            db.create_empty_table(TABLE_NAME, schema()).execute().await?
        };

        Ok(Self { table })
    }

    /// Upserts a single file's embedding, keyed on `path`, via `merge_insert`.
    pub async fn upsert(&self, path: &str, content: &str, embedding: Vec<f32>) -> Result<()> {
        let schema = schema();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec![path])),
                Arc::new(StringArray::from(vec![content])),
                Arc::new(FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                    vec![Some(embedding.into_iter().map(Some).collect::<Vec<_>>())],
                    EMBEDDING_DIM as i32,
                )),
            ],
        )?;
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema.clone());

        let mut merge_insert = self.table.merge_insert(&["path"]);
        merge_insert
            .when_matched_update_all(None)
            .when_not_matched_insert_all();
        merge_insert
            .execute(Box::new(reader))
            .await
            .context("merge_insert upsert failed")?;

        Ok(())
    }

    /// Ranks every indexed file by a blend of semantic similarity (against
    /// `query_embedding`) and fuzzy filename match (against `query_text`),
    /// returning the top `k`. Scores every row in the table directly rather
    /// than going through LanceDB's ANN index, which is simple and exact at
    /// the scale of a personal file index; revisit if that stops being true.
    pub async fn hybrid_search(
        &self,
        query_text: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<HybridHit>> {
        let batches = self
            .table
            .query()
            .select(Select::Columns(vec![
                "path".to_string(),
                "content".to_string(),
                "embedding".to_string(),
            ]))
            .execute()
            .await?
            .try_collect::<Vec<_>>()
            .await?;

        let matcher = SkimMatcherV2::default();
        let mut hits = Vec::new();

        for batch in &batches {
            let paths = batch
                .column_by_name("path")
                .context("missing path column")?
                .as_string::<i32>();
            let contents = batch
                .column_by_name("content")
                .context("missing content column")?
                .as_string::<i32>();
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

                let filename = Path::new(&path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&path);
                let fuzzy_raw = matcher.fuzzy_match(filename, query_text).unwrap_or(0) as f32;
                let fuzzy_sim = fuzzy_raw / (fuzzy_raw + FUZZY_SATURATION);

                let score = SEMANTIC_WEIGHT * semantic_sim + FUZZY_WEIGHT * fuzzy_sim;
                hits.push(HybridHit { path, content, score });
            }
        }

        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(k);
        Ok(hits)
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
