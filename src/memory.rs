use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;

use arrow::array::types::Float32Type;
use arrow::array::{FixedSizeListArray, Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use chrono::Utc;
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{connect, Connection, Table};
use serde::{Deserialize, Serialize};

const MEMORY_DB_PATH: &str = "data/sirin_memory";
const MEMORY_TABLE: &str = "task_memory";

#[derive(Debug, Clone)]
pub struct MemoryHit {
    pub text: String,
    pub distance: f32,
}

fn schema_for_dimension(dimension: i32) -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("text", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dimension,
            ),
            false,
        ),
    ]))
}

fn make_batch(
    text: &str,
    vector: &[f32],
) -> Result<RecordBatch, Box<dyn std::error::Error + Send + Sync>> {
    let dim = vector.len() as i32;
    let schema = schema_for_dimension(dim);
    let id = chrono::Utc::now().timestamp_micros();

    let vector_array = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        [Some(vector.iter().copied().map(Some).collect::<Vec<_>>())],
        dim,
    );

    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![id])),
            Arc::new(StringArray::from(vec![text])),
            Arc::new(vector_array),
        ],
    )?;

    Ok(batch)
}

async fn open_db() -> Result<Connection, Box<dyn std::error::Error + Send + Sync>> {
    Ok(connect(MEMORY_DB_PATH).execute().await?)
}

async fn open_or_create_table(
    db: &Connection,
    dimension: i32,
) -> Result<Table, Box<dyn std::error::Error + Send + Sync>> {
    let names = db.table_names().execute().await?;
    if names.iter().any(|name| name == MEMORY_TABLE) {
        return Ok(db.open_table(MEMORY_TABLE).execute().await?);
    }

    let schema = schema_for_dimension(dimension);
    Ok(db.create_empty_table(MEMORY_TABLE, schema).execute().await?)
}

pub async fn add_to_memory(
    text: &str,
    vector: Vec<f32>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if text.trim().is_empty() {
        return Err("text must not be empty".into());
    }
    if vector.is_empty() {
        return Err("vector must not be empty".into());
    }

    let db = open_db().await?;
    let table = open_or_create_table(&db, vector.len() as i32).await?;
    let batch = make_batch(text, &vector)?;

    table.add(batch).execute().await?;
    Ok(())
}

pub async fn search_memory(
    query_vector: Vec<f32>,
) -> Result<Vec<MemoryHit>, Box<dyn std::error::Error + Send + Sync>> {
    if query_vector.is_empty() {
        return Err("query_vector must not be empty".into());
    }

    let db = open_db().await?;
    let names = db.table_names().execute().await?;
    if !names.iter().any(|name| name == MEMORY_TABLE) {
        return Ok(Vec::new());
    }

    let table = db.open_table(MEMORY_TABLE).execute().await?;
    let batches = table
        .query()
        .nearest_to(query_vector.as_slice())?
        .limit(3)
        .execute()
        .await?
        .try_collect::<Vec<RecordBatch>>()
        .await?;

    let mut hits = Vec::new();

    for batch in batches {
        let Some(text_col) = batch.column_by_name("text") else {
            continue;
        };
        let Some(texts) = text_col.as_any().downcast_ref::<StringArray>() else {
            continue;
        };

        let distance_col = batch.column_by_name("_distance");

        for row in 0..batch.num_rows() {
            let text = texts.value(row).to_string();
            let distance = if let Some(col) = distance_col {
                if let Some(values) = col.as_any().downcast_ref::<arrow::array::Float32Array>() {
                    values.value(row)
                } else if let Some(values) = col.as_any().downcast_ref::<arrow::array::Float64Array>() {
                    values.value(row) as f32
                } else {
                    0.0
                }
            } else {
                0.0
            };

            hits.push(MemoryHit { text, distance });
        }
    }

    hits.sort_by(|a, b| a.distance.total_cmp(&b.distance));
    hits.truncate(3);
    Ok(hits)
}

// ── Simple JSONL conversation context ─────────────────────────────────────────

fn context_log_path(peer_id: Option<i64>) -> std::path::PathBuf {
    let filename = match peer_id {
        Some(id) => format!("sirin_context_{id}.jsonl"),
        None => "sirin_context.jsonl".to_string(),
    };
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        return std::path::Path::new(&local_app_data)
            .join("Sirin")
            .join("tracking")
            .join(&filename);
    }
    std::path::Path::new("data")
        .join("tracking")
        .join(&filename)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEntry {
    pub timestamp: String,
    pub user_msg: String,
    pub assistant_reply: String,
}

/// Append a conversation turn to the context log for a specific peer.
pub fn append_context(
    user_msg: &str,
    assistant_reply: &str,
    peer_id: Option<i64>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = context_log_path(peer_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let entry = ContextEntry {
        timestamp: Utc::now().to_rfc3339(),
        user_msg: user_msg.to_string(),
        assistant_reply: assistant_reply.to_string(),
    };
    let line = serde_json::to_string(&entry)?;
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Load the most recent `limit` context entries for a specific peer.
pub fn load_recent_context(
    limit: usize,
    peer_id: Option<i64>,
) -> Result<Vec<ContextEntry>, Box<dyn std::error::Error + Send + Sync>> {
    let path = context_log_path(peer_id);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(&path)?;
    let reader = BufReader::new(file);

    let mut ring: VecDeque<ContextEntry> = VecDeque::with_capacity(limit);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<ContextEntry>(&line) {
            if ring.len() == limit {
                ring.pop_front();
            }
            ring.push_back(entry);
        }
    }
    Ok(ring.into_iter().collect())
}

/// Truncate the context log for a specific peer (wipe all history).
pub fn clear_context(peer_id: Option<i64>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = context_log_path(peer_id);
    if path.exists() {
        fs::write(&path, b"")?;
    }
    Ok(())
}
