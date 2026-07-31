use crate::config::{ModelConfig, OpenClipConfig};
use crate::error::ClipError;
use crate::model_manager;
use crate::model_manager::get_default_base_folder;
use crate::onnx::OnnxSession;
use bon::bon;
#[cfg(feature = "fast_image_resize")]
use fast_image_resize::{
    images::{Image, ImageRef},
    FilterType as FirFilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
};
#[cfg(not(feature = "fast_image_resize"))]
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};
use ndarray::{s, Array2, Array4, ArrayView, Axis, IxDyn};
use ort::ep::ExecutionProviderDispatch;
use ort::memory::{AllocationDevice, Allocator, AllocatorType, MemoryInfo, MemoryType};
use ort::value::{Tensor, TensorElementType, TensorValueType, Value, ValueType};
use rayon::prelude::*;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;
use std::{env, fmt};

#[cfg(feature = "half_io")]
use half::f16;

#[derive(Debug)]
pub struct VisionEmbedder {
    pub session: OnnxSession,
    pub config: OpenClipConfig,
    pub model_config: ModelConfig,
    pub input_name: String,
    pub model_dir: PathBuf,
    io_binding: Mutex<Option<VisionIoBinding>>,
}

impl Drop for VisionEmbedder {
    fn drop(&mut self) {
        if !clip_io_binding_enabled() {
            return;
        }
        if let Ok(state) = self.io_binding.get_mut() {
            if let Some(binding) = state.take() {
                std::mem::forget(binding);
            }
        }
    }
}

#[derive(Debug)]
pub struct VisionPreprocessedBatch {
    batch_tensor: VisionBatchTensor,
    pub preprocess_ms: f64,
    pub resize_cpu_ms: f64,
    pub normalize_cpu_ms: f64,
    pub images: usize,
}

#[derive(Debug)]
enum VisionBatchTensor {
    F32(Array4<f32>),
    #[cfg(feature = "half_io")]
    F16(Array4<f16>),
}

struct VisionIoBinding {
    input_name: String,
    output_name: String,
    input_shape: Vec<usize>,
    output_shape: Vec<usize>,
    state: VisionIoBindingState,
}

enum VisionIoBindingState {
    F32(VisionIoBindingF32),
    #[cfg(feature = "half_io")]
    F16(VisionIoBindingF16),
}

struct VisionIoBindingF32 {
    input_cpu: Tensor<f32>,
    input_cuda: Tensor<f32>,
    output_cpu: Tensor<f32>,
    binding: ort::session::IoBinding,
}

#[cfg(feature = "half_io")]
struct VisionIoBindingF16 {
    input_cpu: Tensor<f16>,
    input_cuda: Tensor<f16>,
    output_cpu: Tensor<f32>,
    binding: ort::session::IoBinding,
}

impl fmt::Debug for VisionIoBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VisionIoBinding")
            .field("input_name", &self.input_name)
            .field("output_name", &self.output_name)
            .field("input_shape", &self.input_shape)
            .field("output_shape", &self.output_shape)
            .field("state", &self.state_name())
            .finish_non_exhaustive()
    }
}

impl Drop for VisionIoBinding {
    fn drop(&mut self) {
        match &mut self.state {
            VisionIoBindingState::F32(state) => {
                let _ = state.binding.synchronize();
                state.binding.clear();
            }
            #[cfg(feature = "half_io")]
            VisionIoBindingState::F16(state) => {
                let _ = state.binding.synchronize();
                state.binding.clear();
            }
        }
    }
}

impl VisionIoBinding {
    fn state_name(&self) -> &'static str {
        match &self.state {
            VisionIoBindingState::F32(_) => "f32",
            #[cfg(feature = "half_io")]
            VisionIoBindingState::F16(_) => "f16",
        }
    }
}

#[derive(Debug)]
pub struct VisionProfiledEmbeddings {
    pub embeddings: Array2<f32>,
    pub preprocess_ms: f64,
    pub preprocess_resize_cpu_ms: f64,
    pub preprocess_normalize_cpu_ms: f64,
    pub tensor_ms: f64,
    pub run_ms: f64,
    pub extract_ms: f64,
    pub total_ms: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct VisionPreprocessTimings {
    resize_cpu_ms: f64,
    normalize_cpu_ms: f64,
}

impl VisionPreprocessTimings {
    fn add(self, other: Self) -> Self {
        Self {
            resize_cpu_ms: self.resize_cpu_ms + other.resize_cpu_ms,
            normalize_cpu_ms: self.normalize_cpu_ms + other.normalize_cpu_ms,
        }
    }
}

#[bon]
impl VisionEmbedder {
    /// Load vision embedder from a `HuggingFace` model ID
    #[builder(finish_fn = build)]
    #[cfg(feature = "hf-hub")]
    pub async fn from_hf(
        #[builder(start_fn)] model_id: &str,
        with_execution_providers: Option<&[ExecutionProviderDispatch]>,
    ) -> Result<Self, ClipError> {
        let model_dir = model_manager::get_hf_model(model_id).await?;
        Self::from_local_dir(&model_dir)
            .maybe_with_execution_providers(with_execution_providers)
            .build()
    }

    /// Load vision embedder from a locally converted model ID
    #[builder(finish_fn = build)]
    pub fn from_local_id(
        #[builder(start_fn)] model_id: &str,
        base_folder: Option<&Path>,
        with_execution_providers: Option<&[ExecutionProviderDispatch]>,
    ) -> Result<Self, ClipError> {
        let base_folder = base_folder.map_or_else(get_default_base_folder, ToOwned::to_owned);
        Self::from_local_dir(&base_folder.join(model_id))
            .maybe_with_execution_providers(with_execution_providers)
            .build()
    }

    /// Load vision embedder from a specific directory
    #[builder(finish_fn = build)]
    pub fn from_local_dir(
        #[builder(start_fn)] model_dir: &Path,
        with_execution_providers: Option<&[ExecutionProviderDispatch]>,
    ) -> Result<Self, ClipError> {
        model_manager::verify_model_dir(model_dir)?;
        let model_path = model_dir.join("visual.onnx");
        let config_path = model_dir.join("open_clip_config.json");
        let local_config_path = model_dir.join("model_config.json");
        let execution_providers = with_execution_providers.unwrap_or_default();

        let session = OnnxSession::new(model_path, execution_providers)?;
        let config = OpenClipConfig::from_file(config_path)?;
        let model_config = ModelConfig::from_file(local_config_path)?;

        let input_name = session
            .find_input(&["pixel_values", "input"])?
            .ok_or_else(|| ClipError::Config("Could not find vision input node".to_string()))?;

        Ok(Self {
            session,
            config,
            model_config,
            input_name,
            model_dir: model_dir.to_path_buf(),
            io_binding: Mutex::new(None),
        })
    }

    /// Create a new instance of the model
    pub fn duplicate(&self) -> Result<Self, ClipError> {
        Self::from_local_dir(&self.model_dir)
            .with_execution_providers(&self.session.execution_providers)
            .build()
    }

    /// Embed a single image
    pub fn embed_image(&self, image: &DynamicImage) -> Result<ndarray::Array1<f32>, ClipError> {
        let embs = self.embed_images(std::slice::from_ref(image))?;
        let len = embs.len();
        Ok(embs.into_shape_with_order(len)?)
    }

    /// Embed a batch of images
    #[allow(clippy::significant_drop_tightening)]
    pub fn embed_images(&self, images: &[DynamicImage]) -> Result<Array2<f32>, ClipError> {
        Ok(self.embed_images_profiled(images)?.embeddings)
    }

    /// Embed a batch of images and return coarse timing for preprocessing and ONNX Runtime.
    #[allow(clippy::significant_drop_tightening)]
    pub fn embed_images_profiled(
        &self,
        images: &[DynamicImage],
    ) -> Result<VisionProfiledEmbeddings, ClipError> {
        let total_started = Instant::now();
        let preprocessed = self.preprocess_batch_profiled(images)?;
        let mut profile = self.embed_preprocessed_batch_profiled(preprocessed)?;
        profile.total_ms = elapsed_ms_f64(total_started);
        Ok(profile)
    }

    pub fn preprocess_batch_profiled(
        &self,
        images: &[DynamicImage],
    ) -> Result<VisionPreprocessedBatch, ClipError> {
        let started = Instant::now();
        let (batch_tensor, timings) = self.preprocess_batch_for_session_profiled(images)?;
        Ok(VisionPreprocessedBatch {
            batch_tensor,
            preprocess_ms: elapsed_ms_f64(started),
            resize_cpu_ms: timings.resize_cpu_ms,
            normalize_cpu_ms: timings.normalize_cpu_ms,
            images: images.len(),
        })
    }

    #[allow(clippy::significant_drop_tightening)]
    pub fn embed_preprocessed_batch_profiled(
        &self,
        preprocessed: VisionPreprocessedBatch,
    ) -> Result<VisionProfiledEmbeddings, ClipError> {
        if self.can_use_io_binding(&preprocessed)? {
            return self.embed_preprocessed_batch_profiled_iobinding(preprocessed);
        }
        let total_started = Instant::now();
        let preprocess_ms = preprocessed.preprocess_ms;
        let preprocess_resize_cpu_ms = preprocessed.resize_cpu_ms;
        let preprocess_normalize_cpu_ms = preprocessed.normalize_cpu_ms;
        let started = Instant::now();
        let input_tensor = match preprocessed.batch_tensor {
            VisionBatchTensor::F32(input) => Value::from_array(input)?.into_dyn(),
            #[cfg(feature = "half_io")]
            VisionBatchTensor::F16(input) => Value::from_array(input)?.into_dyn(),
        };
        let tensor_ms = elapsed_ms_f64(started);
        let mut session = self.session.session.write()?;
        let output_type = session
            .outputs()
            .first()
            .and_then(|output| output.dtype().tensor_type())
            .unwrap_or(TensorElementType::Float32);
        let started = Instant::now();
        let outputs = session.run(ort::inputs![&self.input_name => input_tensor])?;
        let run_ms = elapsed_ms_f64(started);
        let started = Instant::now();
        let embeddings =
            extract_clip_output_embeddings(&outputs[0], output_type, preprocessed.images)?;
        let extract_ms = elapsed_ms_f64(started);

        Ok(VisionProfiledEmbeddings {
            embeddings,
            preprocess_ms,
            preprocess_resize_cpu_ms,
            preprocess_normalize_cpu_ms,
            tensor_ms,
            run_ms,
            extract_ms,
            total_ms: elapsed_ms_f64(total_started) + preprocess_ms,
        })
    }

    fn can_use_io_binding(&self, preprocessed: &VisionPreprocessedBatch) -> Result<bool, ClipError> {
        let input_supported = match &preprocessed.batch_tensor {
            VisionBatchTensor::F32(_) => true,
            #[cfg(feature = "half_io")]
            VisionBatchTensor::F16(_) => true,
        };
        Ok(clip_io_binding_enabled()
            && input_supported
            && self.static_input_batch_size()?.is_some()
            && self.output_tensor_type()? == TensorElementType::Float32
            && self.static_output_shape()?.is_some())
    }

    fn embed_preprocessed_batch_profiled_iobinding(
        &self,
        preprocessed: VisionPreprocessedBatch,
    ) -> Result<VisionProfiledEmbeddings, ClipError> {
        let total_started = Instant::now();
        let preprocess_ms = preprocessed.preprocess_ms;
        let preprocess_resize_cpu_ms = preprocessed.resize_cpu_ms;
        let preprocess_normalize_cpu_ms = preprocessed.normalize_cpu_ms;
        let output_shape = self
            .static_output_shape()?
            .ok_or_else(|| ClipError::Inference("CLIP output shape is dynamic".to_string()))?;
        let output_name = self.output_name()?;
        match preprocessed.batch_tensor {
            VisionBatchTensor::F32(input) => self.embed_preprocessed_batch_profiled_iobinding_f32(
                input,
                preprocessed.images,
                preprocess_ms,
                preprocess_resize_cpu_ms,
                preprocess_normalize_cpu_ms,
                output_shape,
                output_name,
                total_started,
            ),
            #[cfg(feature = "half_io")]
            VisionBatchTensor::F16(input) => self.embed_preprocessed_batch_profiled_iobinding_f16(
                input,
                preprocessed.images,
                preprocess_ms,
                preprocess_resize_cpu_ms,
                preprocess_normalize_cpu_ms,
                output_shape,
                output_name,
                total_started,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn embed_preprocessed_batch_profiled_iobinding_f32(
        &self,
        input: Array4<f32>,
        images: usize,
        preprocess_ms: f64,
        preprocess_resize_cpu_ms: f64,
        preprocess_normalize_cpu_ms: f64,
        output_shape: Vec<usize>,
        output_name: String,
        total_started: Instant,
    ) -> Result<VisionProfiledEmbeddings, ClipError> {
        let input_shape = input.shape().to_vec();
        let input_slice = input
            .as_slice()
            .ok_or_else(|| ClipError::Inference("CLIP input tensor is not contiguous".to_string()))?;
        let started = Instant::now();
        let mut session = self.session.session.write()?;
        let mut binding_guard = self
            .io_binding
            .lock()
            .map_err(|err| ClipError::Inference(format!("CLIP IO binding lock poisoned: {err}")))?;
        let binding = ensure_clip_iobinding_f32(
            &mut binding_guard,
            &session,
            &self.input_name,
            &output_name,
            &input_shape,
            &output_shape,
        )?;
        if !cuda_copy_host_to_device_f32(binding.input_cuda.data_ptr_mut(), input_slice)? {
            let (_, input_data) = binding.input_cpu.extract_tensor_mut();
            input_data.copy_from_slice(input_slice);
            binding.input_cpu.copy_into_async(&mut binding.input_cuda)?;
        }
        binding
            .binding
            .bind_input(&self.input_name, &binding.input_cuda)?;
        let tensor_ms = elapsed_ms_f64(started);
        let started = Instant::now();
        let outputs = session.run_binding(&binding.binding)?;
        let run_ms = elapsed_ms_f64(started);
        let started = Instant::now();
        let output = outputs.get(&output_name).ok_or_else(|| {
            ClipError::Inference(format!("CLIP output {output_name} missing from IO binding run"))
        })?;
        let output_tensor = output.downcast_ref::<TensorValueType<f32>>()?;
        if !cuda_copy_device_to_host(
            binding.output_cpu.data_ptr_mut(),
            output_tensor.data_ptr(),
            output_shape.iter().product::<usize>(),
        )? {
            output_tensor.copy_into(&mut binding.output_cpu)?;
        }
        let embeddings = extract_clip_tensor_embeddings(&binding.output_cpu, images)?;
        let extract_ms = elapsed_ms_f64(started);
        Ok(VisionProfiledEmbeddings {
            embeddings,
            preprocess_ms,
            preprocess_resize_cpu_ms,
            preprocess_normalize_cpu_ms,
            tensor_ms,
            run_ms,
            extract_ms,
            total_ms: elapsed_ms_f64(total_started) + preprocess_ms,
        })
    }

    #[cfg(feature = "half_io")]
    #[allow(clippy::too_many_arguments)]
    fn embed_preprocessed_batch_profiled_iobinding_f16(
        &self,
        input: Array4<f16>,
        images: usize,
        preprocess_ms: f64,
        preprocess_resize_cpu_ms: f64,
        preprocess_normalize_cpu_ms: f64,
        output_shape: Vec<usize>,
        output_name: String,
        total_started: Instant,
    ) -> Result<VisionProfiledEmbeddings, ClipError> {
        let input_shape = input.shape().to_vec();
        let input_slice = input
            .as_slice()
            .ok_or_else(|| ClipError::Inference("CLIP input tensor is not contiguous".to_string()))?;
        let started = Instant::now();
        let mut session = self.session.session.write()?;
        let mut binding_guard = self
            .io_binding
            .lock()
            .map_err(|err| ClipError::Inference(format!("CLIP IO binding lock poisoned: {err}")))?;
        let binding = ensure_clip_iobinding_f16(
            &mut binding_guard,
            &session,
            &self.input_name,
            &output_name,
            &input_shape,
            &output_shape,
        )?;
        if !cuda_copy_host_to_device_f16(binding.input_cuda.data_ptr_mut(), input_slice)? {
            let (_, input_data) = binding.input_cpu.extract_tensor_mut();
            input_data.copy_from_slice(input_slice);
            binding.input_cpu.copy_into_async(&mut binding.input_cuda)?;
        }
        binding
            .binding
            .bind_input(&self.input_name, &binding.input_cuda)?;
        let tensor_ms = elapsed_ms_f64(started);
        let started = Instant::now();
        let outputs = session.run_binding(&binding.binding)?;
        let run_ms = elapsed_ms_f64(started);
        let started = Instant::now();
        let output = outputs.get(&output_name).ok_or_else(|| {
            ClipError::Inference(format!("CLIP output {output_name} missing from IO binding run"))
        })?;
        let output_tensor = output.downcast_ref::<TensorValueType<f32>>()?;
        if !cuda_copy_device_to_host(
            binding.output_cpu.data_ptr_mut(),
            output_tensor.data_ptr(),
            output_shape.iter().product::<usize>(),
        )? {
            output_tensor.copy_into(&mut binding.output_cpu)?;
        }
        let embeddings = extract_clip_tensor_embeddings(&binding.output_cpu, images)?;
        let extract_ms = elapsed_ms_f64(started);
        Ok(VisionProfiledEmbeddings {
            embeddings,
            preprocess_ms,
            preprocess_resize_cpu_ms,
            preprocess_normalize_cpu_ms,
            tensor_ms,
            run_ms,
            extract_ms,
            total_ms: elapsed_ms_f64(total_started) + preprocess_ms,
        })
    }

    /// Preprocess batch of images
    pub fn preprocess_batch(&self, images: &[DynamicImage]) -> Result<Array4<f32>, ClipError> {
        if images.is_empty() {
            return Err(ClipError::Inference("Empty batch".to_string()));
        }

        let batch_size = images.len();
        let size = self.config.model_cfg.vision_cfg.image_size as usize;
        let mut batch_tensor = Array4::<f32>::zeros((batch_size, 3, size, size));
        batch_tensor
            .axis_iter_mut(Axis(0))
            .into_par_iter()
            .zip(images.par_iter())
            .try_for_each(|(mut slot, img)| self.preprocess_into(img, &mut slot))?;

        Ok(batch_tensor)
    }

    fn preprocess_batch_for_session_profiled(
        &self,
        images: &[DynamicImage],
    ) -> Result<(VisionBatchTensor, VisionPreprocessTimings), ClipError> {
        match self.input_tensor_type()? {
            TensorElementType::Float32 => {
                let (batch, timings) = self.preprocess_batch_profiled_tensor(images)?;
                Ok((VisionBatchTensor::F32(batch), timings))
            }
            #[cfg(feature = "half_io")]
            TensorElementType::Float16 => {
                let (batch, timings) = self.preprocess_batch_profiled_tensor_f16(images)?;
                Ok((VisionBatchTensor::F16(batch), timings))
            }
            other => Err(ClipError::Inference(format!(
                "unsupported CLIP vision input tensor type: {other}"
            ))),
        }
    }

    fn input_tensor_type(&self) -> Result<TensorElementType, ClipError> {
        let session = self.session.session.read()?;
        session
            .inputs()
            .iter()
            .find(|input| input.name() == self.input_name)
            .and_then(|input| input.dtype().tensor_type())
            .ok_or_else(|| {
                ClipError::Config(format!(
                    "could not determine tensor type for vision input {}",
                    self.input_name
                ))
            })
    }

    fn output_tensor_type(&self) -> Result<TensorElementType, ClipError> {
        let session = self.session.session.read()?;
        session
            .outputs()
            .first()
            .and_then(|output| output.dtype().tensor_type())
            .ok_or_else(|| ClipError::Config("could not determine CLIP output tensor type".to_string()))
    }

    fn output_name(&self) -> Result<String, ClipError> {
        let session = self.session.session.read()?;
        session
            .outputs()
            .first()
            .map(|output| output.name().to_string())
            .ok_or_else(|| ClipError::Config("CLIP model has no outputs".to_string()))
    }

    fn preprocess_batch_profiled_tensor(
        &self,
        images: &[DynamicImage],
    ) -> Result<(Array4<f32>, VisionPreprocessTimings), ClipError> {
        if images.is_empty() {
            return Err(ClipError::Inference("Empty batch".to_string()));
        }

        let actual_batch_size = images.len();
        let batch_size = self.input_batch_size_for(actual_batch_size)?;
        let size = self.config.model_cfg.vision_cfg.image_size as usize;
        let mut batch_tensor = Array4::<f32>::zeros((batch_size, 3, size, size));
        let timings = batch_tensor
            .axis_iter_mut(Axis(0))
            .into_par_iter()
            .take(actual_batch_size)
            .zip(images.par_iter())
            .map(|(mut slot, img)| self.preprocess_into_profiled(img, &mut slot))
            .try_reduce(VisionPreprocessTimings::default, |left, right| Ok(left.add(right)))?;

        Ok((batch_tensor, timings))
    }

    #[cfg(feature = "half_io")]
    fn preprocess_batch_profiled_tensor_f16(
        &self,
        images: &[DynamicImage],
    ) -> Result<(Array4<f16>, VisionPreprocessTimings), ClipError> {
        if images.is_empty() {
            return Err(ClipError::Inference("Empty batch".to_string()));
        }

        let actual_batch_size = images.len();
        let batch_size = self.input_batch_size_for(actual_batch_size)?;
        let size = self.config.model_cfg.vision_cfg.image_size as usize;
        let mut batch_tensor = Array4::<f16>::from_elem((batch_size, 3, size, size), f16::ZERO);
        let timings = batch_tensor
            .axis_iter_mut(Axis(0))
            .into_par_iter()
            .take(actual_batch_size)
            .zip(images.par_iter())
            .map(|(mut slot, img)| self.preprocess_into_profiled_f16(img, &mut slot))
            .try_reduce(VisionPreprocessTimings::default, |left, right| Ok(left.add(right)))?;

        Ok((batch_tensor, timings))
    }

    fn input_batch_size_for(&self, actual_batch_size: usize) -> Result<usize, ClipError> {
        let Some(static_batch_size) = self.static_input_batch_size()? else {
            return Ok(actual_batch_size);
        };
        if actual_batch_size > static_batch_size {
            return Err(ClipError::Inference(format!(
                "CLIP batch size {actual_batch_size} exceeds static model batch {static_batch_size}"
            )));
        }
        Ok(static_batch_size)
    }

    fn static_input_batch_size(&self) -> Result<Option<usize>, ClipError> {
        let session = self.session.session.read()?;
        let Some(input) = session
            .inputs()
            .iter()
            .find(|input| input.name() == self.input_name)
        else {
            return Err(ClipError::Config(format!(
                "could not find vision input {}",
                self.input_name
            )));
        };
        let ValueType::Tensor { shape, .. } = input.dtype() else {
            return Ok(None);
        };
        Ok(shape.first().and_then(|dim| usize::try_from(*dim).ok()).filter(|dim| *dim > 0))
    }

    fn static_output_shape(&self) -> Result<Option<Vec<usize>>, ClipError> {
        let session = self.session.session.read()?;
        let Some(output) = session.outputs().first() else {
            return Err(ClipError::Config("CLIP model has no outputs".to_string()));
        };
        let ValueType::Tensor { shape, .. } = output.dtype() else {
            return Ok(None);
        };
        let mut out = Vec::with_capacity(shape.len());
        for dim in shape.iter().copied() {
            let Ok(dim) = usize::try_from(dim) else {
                return Ok(None);
            };
            if dim == 0 {
                return Ok(None);
            }
            out.push(dim);
        }
        Ok(Some(out))
    }

    /// Preprocess single image
    pub fn preprocess(&self, image: &DynamicImage) -> Result<Array4<f32>, ClipError> {
        self.preprocess_batch(std::slice::from_ref(image))
    }

    fn preprocess_into(
        &self,
        image: &DynamicImage,
        out_view: &mut ndarray::ArrayViewMut3<f32>,
    ) -> Result<(), ClipError> {
        let size = self.config.model_cfg.vision_cfg.image_size;

        #[cfg(feature = "fast_image_resize")]
        let pixels_vec = self.resize_with_fast_image_resize(image, size)?;
        #[cfg(feature = "fast_image_resize")]
        let pixels = &pixels_vec;

        #[cfg(not(feature = "fast_image_resize"))]
        let resized = self.resize_with_image(image, size);
        #[cfg(not(feature = "fast_image_resize"))]
        let pixels = resized.as_raw();

        self.normalize_pixels(pixels, size, out_view)?;

        Ok(())
    }

    fn preprocess_into_profiled(
        &self,
        image: &DynamicImage,
        out_view: &mut ndarray::ArrayViewMut3<f32>,
    ) -> Result<VisionPreprocessTimings, ClipError> {
        let size = self.config.model_cfg.vision_cfg.image_size;

        let resize_started = Instant::now();
        #[cfg(feature = "fast_image_resize")]
        let pixels_vec = self.resize_with_fast_image_resize(image, size)?;
        #[cfg(feature = "fast_image_resize")]
        let pixels = &pixels_vec;

        #[cfg(not(feature = "fast_image_resize"))]
        let resized = self.resize_with_image(image, size);
        #[cfg(not(feature = "fast_image_resize"))]
        let pixels = resized.as_raw();
        let resize_cpu_ms = elapsed_ms_f64(resize_started);

        let normalize_started = Instant::now();
        self.normalize_pixels(pixels, size, out_view)?;
        let normalize_cpu_ms = elapsed_ms_f64(normalize_started);

        Ok(VisionPreprocessTimings {
            resize_cpu_ms,
            normalize_cpu_ms,
        })
    }

    #[cfg(feature = "half_io")]
    fn preprocess_into_profiled_f16(
        &self,
        image: &DynamicImage,
        out_view: &mut ndarray::ArrayViewMut3<f16>,
    ) -> Result<VisionPreprocessTimings, ClipError> {
        let size = self.config.model_cfg.vision_cfg.image_size;

        let resize_started = Instant::now();
        #[cfg(feature = "fast_image_resize")]
        let pixels_vec = self.resize_with_fast_image_resize(image, size)?;
        #[cfg(feature = "fast_image_resize")]
        let pixels = &pixels_vec;

        #[cfg(not(feature = "fast_image_resize"))]
        let resized = self.resize_with_image(image, size);
        #[cfg(not(feature = "fast_image_resize"))]
        let pixels = resized.as_raw();
        let resize_cpu_ms = elapsed_ms_f64(resize_started);

        let normalize_started = Instant::now();
        self.normalize_pixels_f16(pixels, size, out_view)?;
        let normalize_cpu_ms = elapsed_ms_f64(normalize_started);

        Ok(VisionPreprocessTimings {
            resize_cpu_ms,
            normalize_cpu_ms,
        })
    }

    #[cfg(feature = "fast_image_resize")]
    fn resize_with_fast_image_resize(
        &self,
        image: &DynamicImage,
        size: u32,
    ) -> Result<Vec<u8>, ClipError> {
        let (width, height) = image.dimensions();
        let mut dst_image = Image::new(size, size, PixelType::U8x3);

        let resize_alg = match self.config.preprocess_cfg.interpolation.as_str() {
            "bicubic" => ResizeAlg::Convolution(FirFilterType::CatmullRom),
            "bilinear" => ResizeAlg::Convolution(FirFilterType::Bilinear),
            _ => ResizeAlg::Nearest,
        };

        let mut options = ResizeOptions::new().resize_alg(resize_alg);

        if self.config.preprocess_cfg.resize_mode.as_str() != "squash" {
            #[allow(clippy::cast_precision_loss)]
            let scale = f64::from(size) / f64::from(width.min(height));
            let crop_w = f64::from(size) / scale;
            let crop_h = f64::from(size) / scale;
            let crop_x = (f64::from(width) - crop_w) / 2.0;
            let crop_y = (f64::from(height) - crop_h) / 2.0;
            options = options.crop(crop_x, crop_y, crop_w, crop_h);
        }

        let mut resizer = Resizer::new();
        if let Some(rgb_image) = image.as_rgb8() {
            let src_image = ImageRef::new(width, height, rgb_image.as_raw(), PixelType::U8x3)?;
            resizer.resize(&src_image, &mut dst_image, &options)?;
        } else {
            let rgb_image = image.to_rgb8();
            let src_image =
                Image::from_vec_u8(width, height, rgb_image.into_raw(), PixelType::U8x3)?;
            resizer.resize(&src_image, &mut dst_image, &options)?;
        }

        Ok(dst_image.into_vec())
    }

    #[cfg(not(feature = "fast_image_resize"))]
    fn resize_with_image(
        &self,
        image: &DynamicImage,
        size: u32,
    ) -> image::ImageBuffer<image::Rgb<u8>, Vec<u8>> {
        let interp = match self.config.preprocess_cfg.interpolation.as_str() {
            "bicubic" => FilterType::CatmullRom,
            "bilinear" => FilterType::Triangle,
            _ => FilterType::Nearest,
        };

        #[allow(
            clippy::single_match_else,
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let img_resized = match self.config.preprocess_cfg.resize_mode.as_str() {
            "squash" => image.resize_exact(size, size, interp),
            _ => {
                let (width, height) = image.dimensions();
                let scale = size as f32 / width.min(height) as f32;
                let scaled_width = (width as f32 * scale).round() as u32;
                let scaled_height = (height as f32 * scale).round() as u32;
                let resized = image.resize_exact(scaled_width, scaled_height, interp);
                let x = ((scaled_width as f32 - size as f32) / 2.0).round() as u32;
                let y = ((scaled_height as f32 - size as f32) / 2.0).round() as u32;
                resized.crop_imm(x, y, size, size)
            }
        };

        img_resized.to_rgb8()
    }

    fn normalize_pixels(
        &self,
        pixels: &[u8],
        size: u32,
        out_view: &mut ndarray::ArrayViewMut3<f32>,
    ) -> Result<(), ClipError> {
        let (mean, std) = (
            self.config.preprocess_cfg.mean,
            self.config.preprocess_cfg.std,
        );

        let channel_len = (size as usize).pow(2);
        let flat = out_view
            .as_slice_mut()
            .ok_or_else(|| ClipError::Inference("Layout mismatch".into()))?;
        if flat.len() != channel_len * 3 || pixels.len() != channel_len * 3 {
            return Err(ClipError::Inference("Layout mismatch".into()));
        }

        let (red, rest) = flat.split_at_mut(channel_len);
        let (green, blue) = rest.split_at_mut(channel_len);
        let mut table = [[0.0f32; 256]; 3];
        for c in 0..3 {
            for value in 0..=u8::MAX {
                table[c][usize::from(value)] = (f32::from(value) / 255.0 - mean[c]) / std[c];
            }
        }
        for (offset, pixel) in pixels.chunks_exact(3).enumerate() {
            red[offset] = table[0][usize::from(pixel[0])];
            green[offset] = table[1][usize::from(pixel[1])];
            blue[offset] = table[2][usize::from(pixel[2])];
        }

        Ok(())
    }

    #[cfg(feature = "half_io")]
    fn normalize_pixels_f16(
        &self,
        pixels: &[u8],
        size: u32,
        out_view: &mut ndarray::ArrayViewMut3<f16>,
    ) -> Result<(), ClipError> {
        let (mean, std) = (
            self.config.preprocess_cfg.mean,
            self.config.preprocess_cfg.std,
        );

        let channel_len = (size as usize).pow(2);
        let flat = out_view
            .as_slice_mut()
            .ok_or_else(|| ClipError::Inference("Layout mismatch".into()))?;
        if flat.len() != channel_len * 3 || pixels.len() != channel_len * 3 {
            return Err(ClipError::Inference("Layout mismatch".into()));
        }

        let (red, rest) = flat.split_at_mut(channel_len);
        let (green, blue) = rest.split_at_mut(channel_len);
        let mut table = [[f16::ZERO; 256]; 3];
        for c in 0..3 {
            for value in 0..=u8::MAX {
                table[c][usize::from(value)] =
                    f16::from_f32((f32::from(value) / 255.0 - mean[c]) / std[c]);
            }
        }
        for (offset, pixel) in pixels.chunks_exact(3).enumerate() {
            red[offset] = table[0][usize::from(pixel[0])];
            green[offset] = table[1][usize::from(pixel[1])];
            blue[offset] = table[2][usize::from(pixel[2])];
        }

        Ok(())
    }
}

fn clip_io_binding_enabled() -> bool {
    env::var("QQ_ANALYZER_CLIP_IO_BINDING")
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn cuda_copy_host_to_device_f32(dst: *mut c_void, src: &[f32]) -> Result<bool, ClipError> {
    cuda_copy_host_to_device_bytes(dst, src.as_ptr().cast(), std::mem::size_of_val(src))
}

#[cfg(feature = "half_io")]
fn cuda_copy_host_to_device_f16(dst: *mut c_void, src: &[f16]) -> Result<bool, ClipError> {
    cuda_copy_host_to_device_bytes(dst, src.as_ptr().cast(), std::mem::size_of_val(src))
}

fn cuda_copy_host_to_device_bytes(
    dst: *mut c_void,
    src: *const c_void,
    bytes: usize,
) -> Result<bool, ClipError> {
    #[cfg(windows)]
    {
        let Some(runtime) = cuda_runtime_copy::runtime() else {
            return Ok(false);
        };
        runtime.copy_host_to_device(dst, src, bytes)?;
        Ok(true)
    }
    #[cfg(not(windows))]
    {
        let _ = (dst, src, bytes);
        Ok(false)
    }
}

fn cuda_copy_device_to_host(
    dst: *mut c_void,
    src: *const c_void,
    elements: usize,
) -> Result<bool, ClipError> {
    #[cfg(windows)]
    {
        let Some(runtime) = cuda_runtime_copy::runtime() else {
            return Ok(false);
        };
        runtime.copy_device_to_host(dst, src, elements.saturating_mul(std::mem::size_of::<f32>()))?;
        Ok(true)
    }
    #[cfg(not(windows))]
    {
        let _ = (dst, src, elements);
        Ok(false)
    }
}

#[cfg(windows)]
	mod cuda_runtime_copy {
	    use super::ClipError;
	    use std::ffi::{c_char, c_void, OsString};
	    use std::ptr;
	    use std::sync::OnceLock;
	    use std::{env, iter};

	    type HModule = *mut c_void;
	    type CudaMemcpy = unsafe extern "C" fn(*mut c_void, *const c_void, usize, i32) -> i32;
	    type CudaMemcpyAsync =
	        unsafe extern "C" fn(*mut c_void, *const c_void, usize, i32, *mut c_void) -> i32;
	    type CudaStreamCreate = unsafe extern "C" fn(*mut *mut c_void) -> i32;
	    type CudaStreamSynchronize = unsafe extern "C" fn(*mut c_void) -> i32;

	    const CUDA_SUCCESS: i32 = 0;
	    const CUDA_MEMCPY_HOST_TO_DEVICE: i32 = 1;
	    const CUDA_MEMCPY_DEVICE_TO_HOST: i32 = 2;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(lpLibFileName: *const u16) -> HModule;
        fn GetProcAddress(hModule: HModule, lpProcName: *const c_char) -> *mut c_void;
    }

	    #[derive(Clone, Copy)]
	    pub struct CudaRuntime {
	        memcpy: CudaMemcpy,
	        memcpy_async: Option<CudaMemcpyAsync>,
	        stream_synchronize: Option<CudaStreamSynchronize>,
	        stream: *mut c_void,
	    }

    unsafe impl Send for CudaRuntime {}
    unsafe impl Sync for CudaRuntime {}

    pub fn runtime() -> Option<&'static CudaRuntime> {
        if !direct_cuda_copy_enabled() {
            return None;
        }
        static RUNTIME: OnceLock<Option<CudaRuntime>> = OnceLock::new();
        RUNTIME.get_or_init(load_runtime).as_ref()
    }

    impl CudaRuntime {
        pub fn copy_host_to_device(
            &self,
            dst: *mut c_void,
            src: *const c_void,
            bytes: usize,
        ) -> Result<(), ClipError> {
            self.copy(dst, src, bytes, CUDA_MEMCPY_HOST_TO_DEVICE, "cudaMemcpy HostToDevice")
        }

        pub fn copy_device_to_host(
            &self,
            dst: *mut c_void,
            src: *const c_void,
            bytes: usize,
        ) -> Result<(), ClipError> {
            self.copy(dst, src, bytes, CUDA_MEMCPY_DEVICE_TO_HOST, "cudaMemcpy DeviceToHost")
        }

	        fn copy(
	            &self,
	            dst: *mut c_void,
	            src: *const c_void,
	            bytes: usize,
	            kind: i32,
	            label: &str,
	        ) -> Result<(), ClipError> {
	            if bytes == 0 {
	                return Ok(());
	            }
	            let status = if async_cuda_copy_enabled() {
	                match (self.memcpy_async, self.stream_synchronize) {
	                    (Some(memcpy_async), Some(stream_synchronize)) if !self.stream.is_null() => {
	                        let copy_status =
	                            unsafe { memcpy_async(dst, src, bytes, kind, self.stream) };
	                        if copy_status != CUDA_SUCCESS {
	                            copy_status
	                        } else {
	                            unsafe { stream_synchronize(self.stream) }
	                        }
	                    }
	                    _ => unsafe { (self.memcpy)(dst, src, bytes, kind) },
	                }
	            } else {
	                unsafe { (self.memcpy)(dst, src, bytes, kind) }
	            };
	            if status == CUDA_SUCCESS {
	                Ok(())
	            } else {
	                Err(ClipError::Inference(format!("{label} failed with CUDA error {status}")))
	            }
        }
    }

	    fn direct_cuda_copy_enabled() -> bool {
	        env::var("QQ_ANALYZER_CLIP_CUDA_MEMCPY")
	            .ok()
	            .map(|value| {
	                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
	            .unwrap_or(false)
	    }

	    fn async_cuda_copy_enabled() -> bool {
	        env::var("QQ_ANALYZER_CLIP_CUDA_MEMCPY_ASYNC")
	            .ok()
	            .map(|value| {
	                matches!(
	                    value.trim().to_ascii_lowercase().as_str(),
	                    "1" | "true" | "yes" | "on"
	                )
	            })
	            .unwrap_or(false)
	    }

    fn load_runtime() -> Option<CudaRuntime> {
        let mut candidates = Vec::new();
        if let Some(path) = env::var_os("QQ_ANALYZER_CUDART_DLL") {
            candidates.push(path);
        }
        candidates.push(OsString::from("cudart64_12.dll"));
        candidates.push(OsString::from("cudart64_110.dll"));
        for candidate in candidates {
            let wide = candidate
                .to_string_lossy()
                .encode_utf16()
                .chain(iter::once(0))
                .collect::<Vec<_>>();
            let module = unsafe { LoadLibraryW(wide.as_ptr()) };
	            if module.is_null() {
	                continue;
	            }
	            let memcpy = unsafe { load_proc::<CudaMemcpy>(module, b"cudaMemcpy\0")? };
	            let memcpy_async =
	                unsafe { load_proc::<CudaMemcpyAsync>(module, b"cudaMemcpyAsync\0") };
	            let stream_create =
	                unsafe { load_proc::<CudaStreamCreate>(module, b"cudaStreamCreate\0") };
	            let stream_synchronize = unsafe {
	                load_proc::<CudaStreamSynchronize>(module, b"cudaStreamSynchronize\0")
	            };
	            let mut stream = ptr::null_mut();
	            if let Some(stream_create) = stream_create {
	                let status = unsafe { stream_create(&mut stream) };
	                if status != CUDA_SUCCESS {
	                    stream = ptr::null_mut();
	                }
	            }
	            return Some(CudaRuntime {
	                memcpy,
	                memcpy_async,
	                stream_synchronize,
	                stream,
	            });
	        }
	        None
	    }

    unsafe fn load_proc<T: Copy>(module: HModule, name: &[u8]) -> Option<T> {
        let ptr = unsafe { GetProcAddress(module, name.as_ptr().cast()) };
        if ptr == ptr::null_mut() {
            None
        } else {
            Some(unsafe { std::mem::transmute_copy(&ptr) })
        }
    }
}

fn ensure_clip_iobinding_f32<'a>(
    state: &'a mut Option<VisionIoBinding>,
    session: &ort::session::Session,
    input_name: &str,
    output_name: &str,
    input_shape: &[usize],
    output_shape: &[usize],
) -> Result<&'a mut VisionIoBindingF32, ClipError> {
    let recreate = state
        .as_ref()
        .map(|state| {
            !matches!(state.state, VisionIoBindingState::F32(_))
                || state.input_name != input_name
                || state.output_name != output_name
                || state.input_shape.as_slice() != input_shape
                || state.output_shape.as_slice() != output_shape
        })
        .unwrap_or(true);
    if recreate {
        let input_allocator = Allocator::new(
            session,
            MemoryInfo::new(
                AllocationDevice::CUDA_PINNED,
                0,
                AllocatorType::Device,
                MemoryType::CPUInput,
            )?,
        )?;
        let input_cpu = Tensor::<f32>::new(&input_allocator, input_shape.to_vec())?;
        let cuda_allocator = Allocator::new(
            session,
            MemoryInfo::new(
                AllocationDevice::CUDA,
                0,
                AllocatorType::Device,
                MemoryType::Default,
            )?,
        )?;
        let input_cuda = Tensor::<f32>::new(&cuda_allocator, input_shape.to_vec())?;
        let output_cuda = Tensor::<f32>::new(&cuda_allocator, output_shape.to_vec())?;
        let output_cpu_allocator = Allocator::new(
            session,
            MemoryInfo::new(
                AllocationDevice::CUDA_PINNED,
                0,
                AllocatorType::Device,
                MemoryType::CPUOutput,
            )?,
        )?;
        let output_cpu = Tensor::<f32>::new(&output_cpu_allocator, output_shape.to_vec())?;
        let mut binding = session.create_binding()?;
        binding.bind_output(output_name, output_cuda)?;
        *state = Some(VisionIoBinding {
            input_name: input_name.to_string(),
            output_name: output_name.to_string(),
            input_shape: input_shape.to_vec(),
            output_shape: output_shape.to_vec(),
            state: VisionIoBindingState::F32(VisionIoBindingF32 {
                input_cpu,
                input_cuda,
                output_cpu,
                binding,
            }),
        });
    }
    match state.as_mut() {
        Some(VisionIoBinding {
            state: VisionIoBindingState::F32(state),
            ..
        }) => Ok(state),
        _ => Err(ClipError::Inference(
            "failed to initialize CLIP f32 IO binding".to_string(),
        )),
    }
}

#[cfg(feature = "half_io")]
fn ensure_clip_iobinding_f16<'a>(
    state: &'a mut Option<VisionIoBinding>,
    session: &ort::session::Session,
    input_name: &str,
    output_name: &str,
    input_shape: &[usize],
    output_shape: &[usize],
) -> Result<&'a mut VisionIoBindingF16, ClipError> {
    let recreate = state
        .as_ref()
        .map(|state| {
            !matches!(state.state, VisionIoBindingState::F16(_))
                || state.input_name != input_name
                || state.output_name != output_name
                || state.input_shape.as_slice() != input_shape
                || state.output_shape.as_slice() != output_shape
        })
        .unwrap_or(true);
    if recreate {
        let input_allocator = Allocator::new(
            session,
            MemoryInfo::new(
                AllocationDevice::CUDA_PINNED,
                0,
                AllocatorType::Device,
                MemoryType::CPUInput,
            )?,
        )?;
        let input_cpu = Tensor::<f16>::new(&input_allocator, input_shape.to_vec())?;
        let cuda_allocator = Allocator::new(
            session,
            MemoryInfo::new(
                AllocationDevice::CUDA,
                0,
                AllocatorType::Device,
                MemoryType::Default,
            )?,
        )?;
        let input_cuda = Tensor::<f16>::new(&cuda_allocator, input_shape.to_vec())?;
        let output_cuda = Tensor::<f32>::new(&cuda_allocator, output_shape.to_vec())?;
        let output_cpu_allocator = Allocator::new(
            session,
            MemoryInfo::new(
                AllocationDevice::CUDA_PINNED,
                0,
                AllocatorType::Device,
                MemoryType::CPUOutput,
            )?,
        )?;
        let output_cpu = Tensor::<f32>::new(&output_cpu_allocator, output_shape.to_vec())?;
        let mut binding = session.create_binding()?;
        binding.bind_output(output_name, output_cuda)?;
        *state = Some(VisionIoBinding {
            input_name: input_name.to_string(),
            output_name: output_name.to_string(),
            input_shape: input_shape.to_vec(),
            output_shape: output_shape.to_vec(),
            state: VisionIoBindingState::F16(VisionIoBindingF16 {
                input_cpu,
                input_cuda,
                output_cpu,
                binding,
            }),
        });
    }
    match state.as_mut() {
        Some(VisionIoBinding {
            state: VisionIoBindingState::F16(state),
            ..
        }) => Ok(state),
        _ => Err(ClipError::Inference(
            "failed to initialize CLIP f16 IO binding".to_string(),
        )),
    }
}

fn elapsed_ms_f64(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn extract_clip_output_embeddings(
    output: &ort::value::DynValue,
    output_type: TensorElementType,
    expected_batch: usize,
) -> Result<Array2<f32>, ClipError> {
    let embeddings = match output_type {
        TensorElementType::Float32 => {
            return extract_clip_dyn_f32_embeddings(output, expected_batch);
        }
        #[cfg(feature = "half_io")]
        TensorElementType::Float16 => {
            let (shape, data) = output.try_extract_tensor::<f16>()?;
            if shape.len() != 2 {
                return Err(ClipError::Inference(format!(
                    "CLIP output shape is {:?}, expected [batch, dim]",
                    shape
                )));
            }
            let batch = shape[0] as usize;
            let dim = shape[1] as usize;
            let converted = data.iter().map(|value| value.to_f32()).collect::<Vec<_>>();
            Array2::from_shape_vec((batch, dim), converted).map_err(ClipError::from)?
        }
        other => Err(ClipError::Inference(format!(
            "unsupported CLIP output tensor type: {other}"
        )))?,
    };
    if embeddings.nrows() < expected_batch {
        return Err(ClipError::Inference(format!(
            "CLIP output batch {} is smaller than expected {expected_batch}",
            embeddings.nrows()
        )));
    }
    if embeddings.nrows() == expected_batch {
        return Ok(embeddings);
    }
    Ok(embeddings.slice(s![0..expected_batch, ..]).to_owned())
}

fn extract_clip_dyn_f32_embeddings(
    output: &ort::value::DynValue,
    expected_batch: usize,
) -> Result<Array2<f32>, ClipError> {
    let (shape, data) = output.try_extract_tensor::<f32>()?;
    extract_clip_f32_embeddings(shape.iter().copied(), data, expected_batch)
}

fn extract_clip_tensor_embeddings(
    output: &Tensor<f32>,
    expected_batch: usize,
) -> Result<Array2<f32>, ClipError> {
    let (shape, data) = output.try_extract_tensor::<f32>()?;
    extract_clip_f32_embeddings(shape.iter().copied(), data, expected_batch)
}

fn extract_clip_f32_embeddings(
    shape: impl Iterator<Item = i64>,
    data: &[f32],
    expected_batch: usize,
) -> Result<Array2<f32>, ClipError> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let shape_usize: Vec<usize> = shape.map(|x| x as usize).collect();
    let view = ArrayView::from_shape(IxDyn(&shape_usize), data)?;
    let embeddings = view.into_dimensionality::<ndarray::Ix2>()?.to_owned();
    if embeddings.nrows() < expected_batch {
        return Err(ClipError::Inference(format!(
            "CLIP output batch {} is smaller than expected {expected_batch}",
            embeddings.nrows()
        )));
    }
    if embeddings.nrows() == expected_batch {
        return Ok(embeddings);
    }
    Ok(embeddings.slice(s![0..expected_batch, ..]).to_owned())
}
