//! Persistent SSCD nearest-neighbour index used by same-image aggregation.
//!
//! The index is a search accelerator only. It stores the already-computed
//! SSCD vectors plus a compact offset-to-exact-representative mapping. The
//! caller still applies the authoritative cosine threshold and exact-copy
//! expansion from the SQLite manifest. No same-image memberships are
//! persisted here.

#[derive(Clone, Debug)]
pub struct AnnCandidate {
    pub representative_asset_id: i64,
    pub score: f64,
}

#[cfg(feature = "image-index-qdrant")]
mod enabled {
    use std::collections::HashMap;
    use std::fs;
    use std::io::{BufReader, BufWriter, Read, Write};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use anyhow::Context;
    use qdrant_common::budget::ResourceBudget;
    use qdrant_common::counter::hardware_accumulator::HwMeasurementAcc;
    use qdrant_common::counter::hardware_counter::HardwareCounterCell;
    use qdrant_common::flags::FeatureFlags;
    use qdrant_common::progress_tracker::{new_progress_tracker, ProgressTree, ProgressView};
    use qdrant_segment::data_types::query_context::QueryContext;
    use qdrant_segment::data_types::vectors::{
        only_default_vector, QueryVector, DEFAULT_VECTOR_NAME,
    };
    use qdrant_segment::entry::entry_point::SegmentEntry;
    use qdrant_segment::index::hnsw_index::hnsw::{HNSWIndex, HnswIndexOpenArgs};
    use qdrant_segment::index::hnsw_index::num_rayon_threads;
    use qdrant_segment::index::VectorIndex;
    use qdrant_segment::segment_constructor::{build_segment, load_segment, VectorIndexBuildArgs};
    use qdrant_segment::types::{
        Distance, HnswConfig, HnswGlobalConfig, Indexes, PayloadStorageType, SearchParams,
        SegmentConfig, SeqNumberType, VectorDataConfig, VectorStorageType,
    };
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use rusqlite::{Connection, OpenFlags};
    use serde::{Deserialize, Serialize};
    use serde_json::{json, Value};

    use super::AnnCandidate;

    const INDEX_SCHEMA_VERSION: u32 = 1;
    const INDEX_DIRECTORY: &str = "sscd-qdrant-v1";
    const METADATA_FILE: &str = "metadata.json";
    const REPRESENTATIVES_FILE: &str = "representatives.i64le";
    const HNSW_M: usize = 16;
    const HNSW_EF_CONSTRUCT: usize = 100;
    const HNSW_EF_SEARCH: usize = 256;
    const HNSW_BUILD_THREADS: usize = 16;
    const DEFAULT_RESULT_LIMIT: usize = 128;

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct IndexMetadata {
        schema_version: u32,
        manifest_identity: String,
        model: String,
        dim: usize,
        points: usize,
        max_embedding_rowid: i64,
        hnsw_m: usize,
        hnsw_ef_construct: usize,
        built_at: String,
    }

    struct LoadedIndex {
        metadata: IndexMetadata,
        hnsw: HNSWIndex,
        representative_ids: Vec<i64>,
    }

    #[derive(Default)]
    struct IndexState {
        phase: &'static str,
        processed: usize,
        total: usize,
        started_at: Option<Instant>,
        elapsed_ms: u64,
        error: Option<String>,
        loaded: Option<Arc<LoadedIndex>>,
        hnsw_progress: Option<ProgressView>,
        generation: u64,
    }

    pub struct SscdVectorIndex {
        state: Mutex<IndexState>,
        cancelled: AtomicBool,
        search_count: AtomicU64,
        search_total_us: AtomicU64,
        search_last_us: AtomicU64,
        search_max_us: AtomicU64,
    }

    impl Default for SscdVectorIndex {
        fn default() -> Self {
            Self {
                state: Mutex::new(IndexState {
                    phase: "cold",
                    ..IndexState::default()
                }),
                cancelled: AtomicBool::new(false),
                search_count: AtomicU64::new(0),
                search_total_us: AtomicU64::new(0),
                search_last_us: AtomicU64::new(0),
                search_max_us: AtomicU64::new(0),
            }
        }
    }

    impl SscdVectorIndex {
        pub fn is_ready(&self) -> bool {
            self.state
                .lock()
                .expect("SSCD vector index state poisoned")
                .loaded
                .is_some()
        }

        pub fn generation(&self) -> u64 {
            self.state
                .lock()
                .expect("SSCD vector index state poisoned")
                .generation
        }

        pub fn terminal_error(&self) -> Option<String> {
            let state = self.state.lock().expect("SSCD vector index state poisoned");
            (state.phase == "failed")
                .then(|| state.error.clone())
                .flatten()
        }

        pub fn ensure_ready(&self, manifest: &Path, manifest_identity: &str) -> anyhow::Result<()> {
            {
                let state = self.state.lock().expect("SSCD vector index state poisoned");
                if state
                    .loaded
                    .as_ref()
                    .is_some_and(|loaded| loaded.metadata.manifest_identity == manifest_identity)
                    || matches!(state.phase, "loading" | "building" | "indexing" | "mapping")
                {
                    return Ok(());
                }
            }
            let started = Instant::now();
            {
                let mut state = self.state.lock().expect("SSCD vector index state poisoned");
                state.phase = "loading";
                state.started_at = Some(started);
                state.error = None;
                state.hnsw_progress = None;
            }
            let index_path = index_path(manifest)?;
            let result = match read_metadata(&index_path) {
                Ok(metadata)
                    if metadata.schema_version == INDEX_SCHEMA_VERSION
                        && metadata.manifest_identity == manifest_identity =>
                {
                    self.ensure_representative_mapping(manifest, &index_path, &metadata)?;
                    load_index(&index_path, metadata)
                }
                _ => self.build(manifest, manifest_identity, &index_path),
            };
            let mut state = self.state.lock().expect("SSCD vector index state poisoned");
            state.elapsed_ms = started.elapsed().as_millis() as u64;
            state.started_at = None;
            match result {
                Ok(index) => {
                    state.processed = index.metadata.points;
                    state.total = index.metadata.points;
                    state.loaded = Some(Arc::new(index));
                    state.hnsw_progress = None;
                    state.phase = "ready";
                    state.generation = state.generation.wrapping_add(1);
                    state.error = None;
                    Ok(())
                }
                Err(error) => {
                    state.loaded = None;
                    state.hnsw_progress = None;
                    state.phase = "failed";
                    state.error = Some(error.to_string());
                    Err(error)
                }
            }
        }

        fn build(
            &self,
            manifest: &Path,
            manifest_identity: &str,
            final_path: &Path,
        ) -> anyhow::Result<LoadedIndex> {
            self.cancelled.store(false, Ordering::Relaxed);
            let build_path = final_path.with_extension(format!("building-{}", std::process::id()));
            if build_path.exists() {
                fs::remove_dir_all(&build_path)?;
            }
            fs::create_dir_all(&build_path)?;
            let segment_base = build_path.join("segment");
            let hnsw_path = build_path.join("hnsw");
            fs::create_dir_all(&hnsw_path)?;

            let con = Connection::open_with_flags(
                manifest,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            con.execute_batch("pragma query_only=on; pragma cache_size=-131072;")?;
            let model: String = con
                .query_row(
                    r#"select model from image_embeddings
                       indexed by image_embeddings_kind_model_idx
                       where kind='sscd' and dim>0 and length(vec)=dim*4
                       order by model desc limit 1"#,
                    [],
                    |row| row.get(0),
                )
                .context("no persisted SSCD vectors are available")?;
            let (dim, total): (i64, i64) = con.query_row(
                r#"select dim,count(*) as valid_points
                   from image_embeddings
                   indexed by image_embeddings_kind_model_idx
                   where kind='sscd' and model=?1 and dim>0
                     and length(vec)=dim*4
                   group by dim
                   order by valid_points desc,dim desc
                   limit 1"#,
                [model.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            anyhow::ensure!(dim > 0, "invalid SSCD vector dimension {dim}");
            let vector_bytes = dim.saturating_mul(4);
            {
                let mut state = self.state.lock().expect("SSCD vector index state poisoned");
                state.phase = "building";
                state.processed = 0;
                state.total = total.max(0) as usize;
            }

            let mut segment = build_segment(
                &segment_base,
                &SegmentConfig {
                    vector_data: HashMap::from([(
                        DEFAULT_VECTOR_NAME.to_owned(),
                        VectorDataConfig {
                            size: dim as usize,
                            distance: Distance::Dot,
                            storage_type: VectorStorageType::ChunkedMmap,
                            index: Indexes::Plain {},
                            quantization_config: None,
                            multivector_config: None,
                            datatype: None,
                        },
                    )]),
                    sparse_vector_data: HashMap::new(),
                    payload_storage_type: PayloadStorageType::Mmap,
                },
                true,
            )?;
            let hardware_counter = HardwareCounterCell::disposable();
            let mut statement = con.prepare(
                r#"select rowid,vec from image_embeddings
                   indexed by image_embeddings_kind_model_idx
                   where kind='sscd' and model=?1 and dim=?2
                     and length(vec)=?3"#,
            )?;
            let mut rows = statement.query(rusqlite::params![model, dim, vector_bytes])?;
            let mut points = 0_usize;
            let mut max_embedding_rowid = 0_i64;
            while let Some(row) = rows.next()? {
                anyhow::ensure!(
                    !self.cancelled.load(Ordering::Relaxed),
                    "SSCD vector index build cancelled"
                );
                let rowid = row.get::<_, i64>(0)?;
                let blob = row.get::<_, Vec<u8>>(1)?;
                let Some(vector) = decode_vector(&blob, dim as usize) else {
                    continue;
                };
                segment.upsert_point(
                    points as SeqNumberType,
                    (rowid as u64).into(),
                    only_default_vector(&vector),
                    &hardware_counter,
                )?;
                points += 1;
                max_embedding_rowid = max_embedding_rowid.max(rowid);
                if points % 10_000 == 0 {
                    self.state
                        .lock()
                        .expect("SSCD vector index state poisoned")
                        .processed = points;
                }
            }
            anyhow::ensure!(points > 0, "no valid SSCD vectors were indexed");
            segment.flush(true)?;

            let hnsw_config = hnsw_config();
            let permit_cpus = num_rayon_threads(hnsw_config.max_indexing_threads);
            let budget = ResourceBudget::new(permit_cpus, permit_cpus);
            let permit = budget
                .try_acquire(permit_cpus, permit_cpus)
                .context("failed to acquire HNSW build resources")?;
            let stopped = AtomicBool::new(false);
            let mut rng = StdRng::seed_from_u64(0x5353_4344);
            let (progress_view, progress) = new_progress_tracker();
            {
                let mut state = self.state.lock().expect("SSCD vector index state poisoned");
                state.phase = "indexing";
                state.processed = 0;
                state.total = points;
                state.hnsw_progress = Some(progress_view);
            }
            let hnsw = HNSWIndex::build(
                HnswIndexOpenArgs {
                    path: &hnsw_path,
                    id_tracker: segment.id_tracker.clone(),
                    vector_storage: segment.vector_data[DEFAULT_VECTOR_NAME]
                        .vector_storage
                        .clone(),
                    quantized_vectors: segment.vector_data[DEFAULT_VECTOR_NAME]
                        .quantized_vectors
                        .clone(),
                    payload_index: segment.payload_index.clone(),
                    hnsw_config,
                },
                VectorIndexBuildArgs {
                    permit: Arc::new(permit),
                    old_indices: &[],
                    gpu_device: None,
                    rng: &mut rng,
                    stopped: &stopped,
                    hnsw_global_config: &HnswGlobalConfig::default(),
                    feature_flags: FeatureFlags::default(),
                    progress,
                },
            )?;

            let metadata = IndexMetadata {
                schema_version: INDEX_SCHEMA_VERSION,
                manifest_identity: manifest_identity.to_string(),
                model,
                dim: dim as usize,
                points,
                max_embedding_rowid,
                hnsw_m: HNSW_M,
                hnsw_ef_construct: HNSW_EF_CONSTRUCT,
                built_at: chrono::Utc::now().to_rfc3339(),
            };
            fs::write(
                build_path.join(METADATA_FILE),
                serde_json::to_vec_pretty(&metadata)?,
            )?;
            drop(hnsw);
            drop(segment);
            publish_index(&build_path, final_path)?;
            self.ensure_representative_mapping(manifest, final_path, &metadata)?;
            load_index(final_path, metadata)
        }

        fn ensure_representative_mapping(
            &self,
            manifest: &Path,
            index_path: &Path,
            metadata: &IndexMetadata,
        ) -> anyhow::Result<()> {
            let mapping_path = index_path.join(REPRESENTATIVES_FILE);
            let expected_bytes = metadata.points as u64 * 8;
            if mapping_path
                .metadata()
                .is_ok_and(|metadata| metadata.len() == expected_bytes)
            {
                return Ok(());
            }
            {
                let mut state = self.state.lock().expect("SSCD vector index state poisoned");
                state.phase = "mapping";
                state.processed = 0;
                state.total = metadata.points.saturating_mul(2);
            }
            let con = Connection::open_with_flags(
                manifest,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            con.execute_batch("pragma query_only=on; pragma cache_size=-262144;")?;
            let mut path_to_representative = HashMap::<String, i64>::with_capacity(metadata.points);
            let mut stmt = con.prepare(
                r#"select a.path,g.representative_asset_id
                   from image_assets a
                   cross join image_exact_groups g on g.asset_id=a.id
                   where a.stale=0"#,
            )?;
            let mut rows = stmt.query([])?;
            let mut mapped_assets = 0_usize;
            while let Some(row) = rows.next()? {
                path_to_representative.insert(row.get(0)?, row.get(1)?);
                mapped_assets += 1;
                if mapped_assets % 10_000 == 0 {
                    self.state
                        .lock()
                        .expect("SSCD vector index state poisoned")
                        .processed = mapped_assets.min(metadata.points);
                }
            }

            let model_points: i64 = con.query_row(
                r#"select count(*) from image_embeddings
                   indexed by image_embeddings_kind_model_idx
                   where kind='sscd' and model=?1 and dim=?2
                     and length(vec)=?3"#,
                rusqlite::params![
                    metadata.model.as_str(),
                    metadata.dim as i64,
                    metadata.dim.saturating_mul(4) as i64
                ],
                |row| row.get(0),
            )?;
            anyhow::ensure!(
                model_points == metadata.points as i64,
                "SSCD embedding count changed: expected {}, found {model_points}",
                metadata.points
            );
            let mut rowid_representatives = Vec::<(i64, i64)>::with_capacity(metadata.points);
            let mut stmt = con.prepare(
                // The primary-key index covers rowid/path/kind/model. Scanning
                // it avoids both the multi-kilobyte vector BLOB and millions
                // of random table lookups through the kind/model index.
                r#"select rowid,path from image_embeddings
                   indexed by sqlite_autoindex_image_embeddings_1
                   where kind='sscd' and model=?1 and dim=?2
                     and length(vec)=?3"#,
            )?;
            let mut rows = stmt.query(rusqlite::params![
                metadata.model.as_str(),
                metadata.dim as i64,
                metadata.dim.saturating_mul(4) as i64
            ])?;
            let mut points = 0_usize;
            while let Some(row) = rows.next()? {
                let rowid = row.get::<_, i64>(0)?;
                let path = row.get::<_, String>(1)?;
                let representative = path_to_representative.get(&path).copied().unwrap_or(0);
                rowid_representatives.push((rowid, representative));
                points += 1;
                if points % 10_000 == 0 {
                    self.state
                        .lock()
                        .expect("SSCD vector index state poisoned")
                        .processed = metadata.points.saturating_add(points);
                }
            }
            anyhow::ensure!(
                points == metadata.points,
                "representative mapping point count changed: expected {}, found {points}",
                metadata.points
            );
            // Qdrant points were inserted by the kind/model index, whose
            // equal-key suffix is SQLite rowid order.
            rowid_representatives.sort_unstable_by_key(|(rowid, _)| *rowid);
            let temporary_path =
                mapping_path.with_extension(format!("building-{}", std::process::id()));
            let mut writer = BufWriter::new(fs::File::create(&temporary_path)?);
            for (_, representative) in rowid_representatives {
                writer.write_all(&representative.to_le_bytes())?;
            }
            writer.flush()?;
            drop(writer);
            if mapping_path.exists() {
                fs::remove_file(&mapping_path)?;
            }
            fs::rename(&temporary_path, &mapping_path)?;
            Ok(())
        }

        pub fn search(
            &self,
            model: &str,
            dim: usize,
            query: &[f32],
            limit: usize,
        ) -> anyhow::Result<Option<Vec<AnnCandidate>>> {
            let loaded = {
                let state = self.state.lock().expect("SSCD vector index state poisoned");
                state.loaded.as_ref().cloned()
            };
            let Some(loaded) = loaded else {
                return Ok(None);
            };
            if loaded.metadata.model != model || loaded.metadata.dim != dim || query.len() != dim {
                return Ok(None);
            }
            let vector: QueryVector = query.to_vec().into();
            let params = SearchParams {
                hnsw_ef: Some(HNSW_EF_SEARCH.max(limit)),
                exact: false,
                ..Default::default()
            };
            let query_context = QueryContext::new(usize::MAX, HwMeasurementAcc::disposable());
            let segment_context = query_context.get_segment_query_context();
            let vector_context = segment_context.get_vector_context(DEFAULT_VECTOR_NAME);
            let started = Instant::now();
            let results = loaded.hnsw.search(
                &[&vector],
                None,
                limit.max(1).max(DEFAULT_RESULT_LIMIT),
                Some(&params),
                &vector_context,
            )?;
            let elapsed_us = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
            self.search_count.fetch_add(1, Ordering::Relaxed);
            self.search_total_us
                .fetch_add(elapsed_us, Ordering::Relaxed);
            self.search_last_us.store(elapsed_us, Ordering::Relaxed);
            self.search_max_us.fetch_max(elapsed_us, Ordering::Relaxed);
            Ok(Some(
                results[0]
                    .iter()
                    .filter_map(|point| {
                        let representative_asset_id =
                            *loaded.representative_ids.get(point.idx as usize)?;
                        if representative_asset_id <= 0 {
                            return None;
                        }
                        Some(AnnCandidate {
                            representative_asset_id,
                            score: point.score as f64,
                        })
                    })
                    .collect(),
            ))
        }

        pub fn metrics_json(&self) -> Value {
            let state = self.state.lock().expect("SSCD vector index state poisoned");
            let loaded_index = state.loaded.as_deref();
            let loaded = loaded_index.map(|index| &index.metadata);
            let progress_tree = state
                .hnsw_progress
                .as_ref()
                .map(|progress| progress.snapshot("sscd_hnsw"));
            let (processed, total) = progress_tree
                .as_ref()
                .and_then(hnsw_progress)
                .unwrap_or((state.processed as u64, state.total as u64));
            let search_count = self.search_count.load(Ordering::Relaxed);
            let search_total_us = self.search_total_us.load(Ordering::Relaxed);
            json!({
                "backend": "qdrant-segment-hnsw",
                "phase": state.phase,
                "processed": processed,
                "total": total,
                "percent": if total == 0 { 0.0 } else { processed as f64 / total as f64 },
                "hnsw_progress": progress_tree,
                "elapsed_ms": state.started_at.map(|started| started.elapsed().as_millis() as u64).unwrap_or(state.elapsed_ms),
                "error": state.error,
                "generation": state.generation,
                "points": loaded.map(|metadata| metadata.points).unwrap_or(0),
                "model": loaded.map(|metadata| metadata.model.as_str()),
                "dim": loaded.map(|metadata| metadata.dim),
                "hnsw_m": HNSW_M,
                "ef_construct": HNSW_EF_CONSTRUCT,
                "ef_search": HNSW_EF_SEARCH,
                "hnsw_on_disk": loaded_index.map(|index| index.hnsw.is_on_disk()).unwrap_or(true),
                "vector_storage": "chunked_mmap",
                "search_count": search_count,
                "search_avg_ms": if search_count == 0 { 0.0 } else { search_total_us as f64 / search_count as f64 / 1000.0 },
                "search_last_ms": self.search_last_us.load(Ordering::Relaxed) as f64 / 1000.0,
                "search_max_ms": self.search_max_us.load(Ordering::Relaxed) as f64 / 1000.0
            })
        }
    }

    fn index_path(manifest: &Path) -> anyhow::Result<PathBuf> {
        Ok(manifest
            .parent()
            .context("image manifest has no parent directory")?
            .join(INDEX_DIRECTORY))
    }

    fn read_metadata(index_path: &Path) -> anyhow::Result<IndexMetadata> {
        Ok(serde_json::from_slice(&fs::read(
            index_path.join(METADATA_FILE),
        )?)?)
    }

    fn actual_segment_path(base: &Path) -> anyhow::Result<PathBuf> {
        if base.join("segment.json").is_file() {
            return Ok(base.to_owned());
        }
        for entry in fs::read_dir(base)? {
            let path = entry?.path();
            if path.join("segment.json").is_file() {
                return Ok(path);
            }
        }
        anyhow::bail!("no Qdrant segment below {}", base.display())
    }

    fn load_index(index_path: &Path, metadata: IndexMetadata) -> anyhow::Result<LoadedIndex> {
        let stopped = AtomicBool::new(false);
        let segment_path = actual_segment_path(&index_path.join("segment"))?;
        ensure_on_disk_layout(&segment_path)?;
        let segment = load_segment(&segment_path, &stopped)?
            .with_context(|| format!("failed to load {}", segment_path.display()))?;
        anyhow::ensure!(
            segment.available_point_count() == metadata.points,
            "Qdrant point count changed: expected {}, found {}",
            metadata.points,
            segment.available_point_count()
        );
        let id_tracker = segment.id_tracker.clone();
        let representative_ids =
            read_representative_mapping(&index_path.join(REPRESENTATIVES_FILE), metadata.points)?;
        let hnsw = HNSWIndex::open(HnswIndexOpenArgs {
            path: &index_path.join("hnsw"),
            id_tracker: id_tracker.clone(),
            vector_storage: segment.vector_data[DEFAULT_VECTOR_NAME]
                .vector_storage
                .clone(),
            quantized_vectors: segment.vector_data[DEFAULT_VECTOR_NAME]
                .quantized_vectors
                .clone(),
            payload_index: segment.payload_index.clone(),
            hnsw_config: hnsw_config(),
        })?;
        Ok(LoadedIndex {
            metadata,
            hnsw,
            representative_ids,
        })
    }

    /// Older accelerator builds forced every vector and HNSW edge into RAM.
    /// Qdrant's chunk files and graph format are identical for the mmap mode,
    /// so this is a metadata-only, restart-safe upgrade rather than a
    /// multi-hour rebuild of the 3.7-million-point index.
    fn ensure_on_disk_layout(segment_path: &Path) -> anyhow::Result<()> {
        let segment_config_path = segment_path.join("segment.json");
        let mut segment_config: Value = serde_json::from_slice(&fs::read(&segment_config_path)?)?;
        let storage_type = segment_config
            .pointer_mut("/config/vector_data//storage_type")
            .context("Qdrant segment is missing the default vector storage type")?;
        if storage_type.as_str() == Some("InRamChunkedMmap") {
            *storage_type = Value::String("ChunkedMmap".to_string());
            replace_small_json(&segment_config_path, &segment_config)?;
        }

        let vector_config_path = segment_path
            .join("vector_storage")
            .join("vectors")
            .join("config.json");
        let mut vector_config: Value = serde_json::from_slice(&fs::read(&vector_config_path)?)?;
        if vector_config["populate"].as_bool() != Some(false) {
            vector_config["populate"] = Value::Bool(false);
            replace_small_json(&vector_config_path, &vector_config)?;
        }
        Ok(())
    }

    fn replace_small_json(path: &Path, value: &Value) -> anyhow::Result<()> {
        let temporary = path.with_extension(format!("json.upgrade-{}", std::process::id()));
        let bytes = serde_json::to_vec(value)?;
        let mut file = fs::File::create(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        #[cfg(not(windows))]
        {
            fs::rename(&temporary, path)?;
        }
        #[cfg(windows)]
        {
            let backup = path.with_extension(format!("json.backup-{}", std::process::id()));
            if backup.exists() {
                fs::remove_file(&backup)?;
            }
            fs::rename(path, &backup)?;
            if let Err(error) = fs::rename(&temporary, path) {
                let _ = fs::rename(&backup, path);
                return Err(error.into());
            }
            fs::remove_file(backup)?;
        }
        Ok(())
    }

    fn read_representative_mapping(path: &Path, points: usize) -> anyhow::Result<Vec<i64>> {
        let mut reader = BufReader::new(fs::File::open(path)?);
        let mut bytes = vec![0_u8; points.saturating_mul(8)];
        reader.read_exact(&mut bytes)?;
        Ok(bytes
            .chunks_exact(8)
            .map(|chunk| i64::from_le_bytes(chunk.try_into().expect("eight-byte chunk")))
            .collect())
    }

    fn publish_index(build_path: &Path, final_path: &Path) -> anyhow::Result<()> {
        if !final_path.exists() {
            fs::rename(build_path, final_path)?;
            return Ok(());
        }
        let backup = final_path.with_extension(format!(
            "replaced-{}",
            chrono::Utc::now().timestamp_millis()
        ));
        fs::rename(final_path, &backup)?;
        if let Err(error) = fs::rename(build_path, final_path) {
            let _ = fs::rename(&backup, final_path);
            return Err(error.into());
        }
        let _ = fs::remove_dir_all(backup);
        Ok(())
    }

    fn decode_vector(blob: &[u8], dim: usize) -> Option<Vec<f32>> {
        if blob.len() != dim * 4 {
            return None;
        }
        Some(
            blob.chunks_exact(4)
                .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect(),
        )
    }

    fn hnsw_config() -> HnswConfig {
        HnswConfig {
            m: HNSW_M,
            ef_construct: HNSW_EF_CONSTRUCT,
            full_scan_threshold: 10,
            max_indexing_threads: HNSW_BUILD_THREADS,
            on_disk: Some(true),
            payload_m: None,
            inline_storage: None,
        }
    }

    fn hnsw_progress(tree: &ProgressTree) -> Option<(u64, u64)> {
        if tree.name == "main_graph" {
            if let (Some(done), Some(total)) = (tree.done, tree.total) {
                return Some((done, total));
            }
        }
        tree.children
            .iter()
            .find_map(hnsw_progress)
            .or_else(|| tree.done.zip(tree.total).filter(|(_, total)| *total > 0))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn blob(vector: &[f32]) -> Vec<u8> {
            vector
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect()
        }

        #[test]
        fn builds_reopens_and_searches_persisted_vectors() -> anyhow::Result<()> {
            let root = std::env::temp_dir().join(format!(
                "qq-analyzer-qdrant-smoke-{}-{}",
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ));
            fs::create_dir_all(&root)?;
            let manifest = root.join("manifest.sqlite");
            let con = Connection::open(&manifest)?;
            con.execute_batch(
                r#"create table image_embeddings(
                       path text not null,
                       kind text not null,
                       model text not null,
                       dim integer not null,
                       vec blob not null,
                       primary key(path,kind,model)
                   );
                   create index image_embeddings_kind_model_idx
                     on image_embeddings(kind,model);
                   create table image_assets(
                       id integer primary key,
                       path text not null,
                       stale integer not null
                   );
                   create table image_exact_groups(
                       asset_id integer primary key,
                       representative_asset_id integer not null
                   );"#,
            )?;
            for (id, path, representative, vector) in [
                (11_i64, "a", 11_i64, [1.0_f32, 0.0, 0.0, 0.0]),
                (12, "b", 11, [0.99995_f32, 0.01, 0.0, 0.0]),
                (13, "c", 13, [0.0_f32, 1.0, 0.0, 0.0]),
            ] {
                con.execute(
                    "insert into image_embeddings(path,kind,model,dim,vec) values(?1,'sscd','test',4,?2)",
                    rusqlite::params![path, blob(&vector)],
                )?;
                con.execute(
                    "insert into image_assets(id,path,stale) values(?1,?2,0)",
                    rusqlite::params![id, path],
                )?;
                con.execute(
                    "insert into image_exact_groups(asset_id,representative_asset_id) values(?1,?2)",
                    rusqlite::params![id, representative],
                )?;
            }
            for (id, path, dim, vector) in [
                (14_i64, "wrong-dimension", 3_i64, blob(&[1.0, 0.0, 0.0])),
                (15, "truncated-blob", 4, vec![0_u8; 7]),
            ] {
                con.execute(
                    "insert into image_embeddings(path,kind,model,dim,vec) values(?1,'sscd','test',?2,?3)",
                    rusqlite::params![path, dim, vector],
                )?;
                con.execute(
                    "insert into image_assets(id,path,stale) values(?1,?2,0)",
                    rusqlite::params![id, path],
                )?;
                con.execute(
                    "insert into image_exact_groups(asset_id,representative_asset_id) values(?1,?1)",
                    [id],
                )?;
            }
            drop(con);

            let first = SscdVectorIndex::default();
            first.ensure_ready(&manifest, "manifest-one")?;
            {
                let state = first.state.lock().unwrap();
                let loaded = state.loaded.as_deref().context("index was not loaded")?;
                assert!(loaded.hnsw.is_on_disk());
                assert_eq!(loaded.metadata.points, 3);
                assert!(state.started_at.is_none());
            }
            let index = index_path(&manifest)?;
            let segment = actual_segment_path(&index.join("segment"))?;
            let segment_config: Value =
                serde_json::from_slice(&fs::read(segment.join("segment.json"))?)?;
            assert_eq!(
                segment_config.pointer("/config/vector_data//storage_type"),
                Some(&Value::String("ChunkedMmap".to_string()))
            );
            let vector_config: Value = serde_json::from_slice(&fs::read(
                segment
                    .join("vector_storage")
                    .join("vectors")
                    .join("config.json"),
            )?)?;
            assert_eq!(vector_config["populate"], false);
            let matches = first
                .search("test", 4, &[1.0, 0.0, 0.0, 0.0], 3)?
                .context("index was not ready")?;
            assert_eq!(
                matches.first().map(|item| item.representative_asset_id),
                Some(11)
            );

            let reopened = SscdVectorIndex::default();
            reopened.ensure_ready(&manifest, "manifest-one")?;
            let matches = reopened
                .search("test", 4, &[0.0, 1.0, 0.0, 0.0], 3)?
                .context("reopened index was not ready")?;
            assert_eq!(
                matches.first().map(|item| item.representative_asset_id),
                Some(13)
            );

            fs::remove_dir_all(root)?;
            Ok(())
        }
    }

    pub use SscdVectorIndex as Implementation;
}

#[cfg(not(feature = "image-index-qdrant"))]
mod disabled {
    use std::path::Path;

    use serde_json::{json, Value};

    use super::AnnCandidate;

    #[derive(Default)]
    pub struct SscdVectorIndex;

    impl SscdVectorIndex {
        pub fn is_ready(&self) -> bool {
            false
        }

        pub fn generation(&self) -> u64 {
            0
        }

        pub fn terminal_error(&self) -> Option<String> {
            Some("此构建未启用 image-index-qdrant，无法保证完整的 SSCD ≥ 0.98 同图召回".to_string())
        }

        pub fn ensure_ready(
            &self,
            _manifest: &Path,
            _manifest_identity: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        pub fn search(
            &self,
            _model: &str,
            _dim: usize,
            _query: &[f32],
            _limit: usize,
        ) -> anyhow::Result<Option<Vec<AnnCandidate>>> {
            Ok(None)
        }

        pub fn metrics_json(&self) -> Value {
            json!({
                "backend": "unavailable",
                "phase": "unavailable",
                "processed": 0,
                "total": 0,
                "percent": 0.0,
                "elapsed_ms": 0,
                "error": "此构建未启用 image-index-qdrant，无法保证完整的 SSCD ≥ 0.98 同图召回",
                "generation": 0,
                "points": 0,
                "model": null,
                "dim": null,
                "hnsw_m": 0,
                "ef_construct": 0,
                "ef_search": 0,
                "search_count": 0,
                "search_avg_ms": 0.0,
                "search_last_ms": 0.0,
                "search_max_ms": 0.0
            })
        }
    }

    pub use SscdVectorIndex as Implementation;
}

#[cfg(not(feature = "image-index-qdrant"))]
pub use disabled::Implementation as SscdVectorIndex;
#[cfg(feature = "image-index-qdrant")]
pub use enabled::Implementation as SscdVectorIndex;
