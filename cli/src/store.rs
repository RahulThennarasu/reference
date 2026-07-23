use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::{
    cast::AsArray, types::Float32Type, FixedSizeListArray, RecordBatch, RecordBatchIterator,
    StringArray,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{connect, Connection, Table};

use crate::embedding::EMBEDDING_DIM;

const TABLE_NAME: &str = "files";

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

    /// Returns the top `k` files closest to `query_embedding`, as (path, distance) pairs.
    pub async fn search(&self, query_embedding: &[f32], k: usize) -> Result<Vec<(String, f32)>> {
        let batches = self
            .table
            .query()
            .nearest_to(query_embedding)?
            .limit(k)
            .execute()
            .await?
            .try_collect::<Vec<_>>()
            .await?;

        let mut results = Vec::new();
        for batch in &batches {
            let paths = batch
                .column_by_name("path")
                .context("missing path column")?
                .as_string::<i32>();
            let distances = batch
                .column_by_name("_distance")
                .context("missing _distance column")?
                .as_primitive::<arrow_array::types::Float32Type>();

            for i in 0..batch.num_rows() {
                results.push((paths.value(i).to_string(), distances.value(i)));
            }
        }

        results.sort_by(|a, b| a.1.total_cmp(&b.1));
        Ok(results)
    }
}
