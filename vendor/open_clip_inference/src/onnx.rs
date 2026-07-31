use crate::ClipError;
use ort::ep::ExecutionProviderDispatch;
use ort::session::{Session, builder::GraphOptimizationLevel};
use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

const QQ_ANALYZER_CLIP_ORT_PROFILE_ENV: &str = "QQ_ANALYZER_CLIP_ORT_PROFILE";
const OPEN_CLIP_ORT_PROFILE_ENV: &str = "OPEN_CLIP_ORT_PROFILE";
const QQ_ANALYZER_CLIP_ORT_INTRA_THREADS_ENV: &str = "QQ_ANALYZER_CLIP_ORT_INTRA_THREADS";
const OPEN_CLIP_ORT_INTRA_THREADS_ENV: &str = "OPEN_CLIP_ORT_INTRA_THREADS";

#[derive(Debug)]
pub struct OnnxSession {
    pub session: RwLock<Session>,
    pub execution_providers: Vec<ExecutionProviderDispatch>,
    ort_profile_path: Option<PathBuf>,
    ort_profile_ended: AtomicBool,
}

impl OnnxSession {
    pub fn new(
        path: impl AsRef<Path>,
        execution_providers: &[ExecutionProviderDispatch],
    ) -> Result<Self, ClipError> {
        let path = path.as_ref();
        let threads = ort_intra_threads().unwrap_or_else(|| {
            if execution_providers.is_empty() {
                num_cpus::get()
            } else {
                1
            }
        });
        let ort_profile_path = ort_profile_path_for_model(path)?;
        let builder = Session::builder()?
            .with_execution_providers(execution_providers)?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_intra_threads(threads)?;
        let mut builder = if let Some(profile_path) = ort_profile_path.as_ref() {
            builder.with_profiling(profile_path)?
        } else {
            builder
        };
        let session = builder.commit_from_file(path)?;

        Ok(Self {
            session: RwLock::new(session),
            execution_providers: execution_providers.to_vec(),
            ort_profile_path,
            ort_profile_ended: AtomicBool::new(false),
        })
    }

    pub fn ort_profile_path(&self) -> Option<String> {
        self.ort_profile_path
            .as_ref()
            .map(|path| path.display().to_string())
    }

    pub fn finish_ort_profiling(&self) -> Result<Option<String>, ClipError> {
        if self.ort_profile_path.is_none() || self.ort_profile_ended.swap(true, Ordering::AcqRel) {
            return Ok(None);
        }
        let mut session = self.session.write()?;
        let path = session.end_profiling()?;
        Ok(Some(path))
    }

    /// Helper to check if the model expects a specific input name
    pub fn has_input(&self, name: &str) -> Result<bool, ClipError> {
        let session = self.session.read()?;
        Ok(session.inputs().iter().any(|i| i.name() == name))
    }

    /// Helper to find the first likely input name for a specific role
    pub fn find_input(&self, possibilities: &[&str]) -> Result<Option<String>, ClipError> {
        let session = self.session.read()?;
        for &p in possibilities {
            if session.inputs().iter().any(|i| i.name() == p) {
                return Ok(Some(p.to_string()));
            }
        }
        Ok(None)
    }
}

impl Drop for OnnxSession {
    fn drop(&mut self) {
        let _ = self.finish_ort_profiling();
    }
}

fn ort_intra_threads() -> Option<usize> {
    env::var(QQ_ANALYZER_CLIP_ORT_INTRA_THREADS_ENV)
        .ok()
        .or_else(|| env::var(OPEN_CLIP_ORT_INTRA_THREADS_ENV).ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
}

fn ort_profile_path_for_model(path: &Path) -> Result<Option<PathBuf>, ClipError> {
    let Some(value) = env::var_os(QQ_ANALYZER_CLIP_ORT_PROFILE_ENV)
        .or_else(|| env::var_os(OPEN_CLIP_ORT_PROFILE_ENV))
    else {
        return Ok(None);
    };
    let base = PathBuf::from(value);
    if base.as_os_str().is_empty() {
        return Ok(None);
    }
    let model_stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("clip");
    let profile_path = if base.extension().is_some() {
        let base_stem = base
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty())
            .unwrap_or("clip_ort_profile");
        let extension = base
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| !extension.is_empty())
            .unwrap_or("json");
        base.with_file_name(format!("{base_stem}.{model_stem}.{extension}"))
    } else {
        base.join(format!("{model_stem}.ort-profile.json"))
    };
    if let Some(parent) = profile_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(Some(profile_path))
}
