pub mod binary;
pub mod cmp;
pub mod conv2d;
pub mod convert;
pub mod copy;
pub mod gather;
pub mod index_select;
pub mod matmul;
pub mod pool2d;
pub mod reduce;
pub mod rms_norm;
pub mod softmax;
pub mod unary;
pub mod upsample;
pub mod where_cond;

use std::{
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
    num::NonZeroU64,
};

use tracing::{instrument, span, Level};
use super::{
    cache::{BindGroupReferenceBase, BufferReference, CachedBindGroupReference, CachedBuffer},
    device::{
        BindGroupReference, DispatchedBindgroup, MlQueue, OpIsInplaceable, PipelineType, QueueBuffer, META_BUFFER_SIZE
    },
    util::{FixedArray, ToU32},
};

pub use candle_wgpu_kernels::Pipelines as Pipelines;
pub use candle_wgpu_kernels::DType as DType;
pub use crate::wgpu::WgpuDevice as WgpuDevice;
pub use crate::wgpu::cache::BufferReferenceId as BufferReferenceId;

use crate::{Layout, WebGpuError};
use std::{
    borrow::Cow,
    sync::{Arc, MutexGuard},
};
use wgpu::{Device, Queue, ShaderModule};

pub use binary::queue_binary_buffer_from_buffer;
pub use cmp::queue_cmp_buffer_from_buffer;
pub use conv2d::{queue_conv1d, queue_conv1d_transpose, queue_conv2d, queue_conv2d_transpose};
pub use convert::{
    queue_convert_f32_to_u32, queue_convert_f32_to_u8, queue_convert_u32_to_f32,
    queue_convert_u32_to_u8, queue_convert_u8_to_f32,
};
pub use copy::{queue_copy, queue_copy2d, queue_copy3d,queue_copy3d_padded, queue_copy_strided};
pub use gather::{queue_gather, queue_index_add_inplace, queue_scatter_add_inplace};
pub use index_select::queue_index_select;
pub use matmul::queue_matmul_buffer;
pub use pool2d::{queue_avg_pool2d, queue_max_pool2d};
pub use reduce::queue_reduce_from_buffer_op;
pub use rms_norm::queue_rms_norm;
pub use softmax::queue_softmax;
pub use unary::{queue_unary_from_buffer_op, queue_unary_inplace_op};
pub use upsample::{queue_upsample1d, queue_upsample2d};
pub use where_cond::queue_where_cond_u32;

pub const MAX_DISPATCH_SIZE: u32 = 65535;

///Helper Type MetaArray, for constructing the MetaBuffer
#[derive(Debug)]
pub struct MetaArray(pub Vec<u32>);

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ConstArray(pub FixedArray<(candle_wgpu_kernels::Constants, u32), 32>);

pub trait KernelParameterMeta{
    fn write_meta(&self, meta : &mut MetaArray);
}

pub trait KernelParameterConsts{
    fn write_consts(&self, _consts : &mut ConstArray){}
}
pub trait KernelParameter : KernelParameterMeta + KernelParameterConsts{
   
}


impl MetaArray {
    pub fn new(capacity: u32) -> Self {
        MetaArray(Vec::with_capacity(capacity as usize))
    }

    pub fn add<T : KernelParameterMeta>(&mut self, value : T){
        value.write_meta(self);
    }
}

impl<T : ToU32 + Copy> KernelParameterMeta for T{
    fn write_meta(&self, meta : &mut MetaArray) {
        meta.0.push((*self).to_u32());
    }
}

impl ConstArray {
    pub fn new() -> Self {
        ConstArray(FixedArray::new())
    }

    pub fn add<T : KernelParameterConsts>(&mut self, value : T){
        value.write_consts(self);
    }

    pub fn insert<T : ToU32>(&mut self, key : candle_wgpu_kernels::Constants, value : T){
        self.0.push((key, value.to_u32()));
    }
}


const WORKGROUP_SIZE: u32 = 64;

pub fn get_dtype(dtype : crate::DType) -> crate::Result<DType>{
    match dtype{
        crate::DType::U8 =>  Ok(DType::U8),
        crate::DType::U32 => Ok(DType::U32),
        crate::DType::F32 =>  Ok(DType::F32),
        _ => Err(crate::Error::WebGpu(WebGpuError::from(format!("Dtype {:?} not supported on wgpu", dtype)))),
    }
}

#[instrument]
pub fn get_shader(device: &wgpu::Device, shader: &'static str) -> ShaderModule {
    let cs_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: None,
        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(shader)),
    });
    return cs_module;
}

pub fn create_buffer_init<T: bytemuck::Pod>(dev: &WgpuDevice, data: &[T]) -> Arc<BufferReference> {
    return BufferReference::new_init(dev, bytemuck::cast_slice(data));
}


fn enqueue_workgroups(
    command_queue: MutexGuard<QueueBuffer>,
    pipeline: PipelineType,
    bind_group: BindGroupReference,
    x: u32,
    y: u32,
    z: u32,
    workload_size : usize
) {
    enqueue_workgroups_extra(command_queue, pipeline, bind_group, x, y, z, workload_size, #[cfg(feature = "wgpu_debug")]None)
}

fn enqueue_workgroups_extra(
    mut command_queue: MutexGuard<QueueBuffer>,
    pipeline: PipelineType,
    bind_group: BindGroupReference,
    x: u32,
    y: u32,
    z: u32,
    workload_size : usize,
    #[cfg(feature = "wgpu_debug")] _debug: Option<String>,
) {
    if y > MAX_DISPATCH_SIZE || z > MAX_DISPATCH_SIZE  || x > MAX_DISPATCH_SIZE {
        panic!("can not queue y or z higher than 65535 x:{x}, y:{y}, z:{z}, pipeline: {:?}", pipeline);
    }
    let q = MlQueue::Dispatch(super::device::MlQueueDispatch {
        x,
        y,
        z,
        pipeline: pipeline.clone(),
        pipeline_cached : None,
        bindgroup: DispatchedBindgroup::BindgroupReference(bind_group),
        meta: command_queue.current_meta,
        workload_size,
        #[cfg(feature = "wgpu_debug")]
        debug : _debug
    });
    command_queue.command_queue.push(q);
}

fn next_divisible_by_n<T : num_traits::Num + Clone>(value: T, n: T) -> T {
    if n.is_zero(){
        panic!("n must be a non-zero integer");
    }

    if (value.clone() % n.clone()).is_zero() {
        value
    } else {
        value.clone() + (n.clone() - value % n)
    }
}

fn get_meta(dev: &WgpuDevice) -> MutexGuard<QueueBuffer> {
    let mut command_queue = dev.command_queue.lock().unwrap();
    let meta_array_length = command_queue.get_meta().len() as i32;
    let meta_offset = next_divisible_by_n(
        meta_array_length,
        dev.device_limits.min_storage_buffer_offset_alignment as i32 / 4,
    );
    command_queue.current_meta = meta_offset as u32;
    command_queue
        .get_meta_mut()
        .extend(std::iter::repeat(0).take((meta_offset - meta_array_length) as usize));

    return command_queue;
}

#[cfg(feature = "wgpu_debug")]
fn end_debug_queue(
    dev: &WgpuDevice,
    length: u32,
    global_index: u32,
    encoder: &mut wgpu::CommandEncoder,
    query_set: &wgpu::QuerySet,
) {
    if global_index % 256 != 0 {
        panic!("global_index was:{global_index}")
    }

    encoder.resolve_query_set(
        &query_set,
        0..length,
        &dev.debug.query_set_buffer,
        global_index as u64,
    );
    let global_index = global_index + (length * 8) as u32;

    let remainder = global_index % 256;
    let global_index = if remainder == 0 {
        global_index
    } else {
        global_index + (256 - remainder)
    };
    dev.debug
        .counter
        .store(global_index, std::sync::atomic::Ordering::Relaxed);
}

#[instrument]
fn get_command_buffer(
    dev: &WgpuDevice,
    meta_array: &[u32],
    command_queue: &[MlQueue],
    current_meta: usize,
    waiting_buffer : &Option<Arc<CachedBuffer>> //a buffer, we want to wait for, after all commands have been queued
) -> wgpu::CommandBuffer {
    #[cfg(feature = "wgpu_debug")]
    let query_set = &dev.debug.query_set;

    #[cfg(feature = "wgpu_debug")]
    let global_index = dev.debug.counter.load(std::sync::atomic::Ordering::Relaxed);
    
    #[cfg(feature = "wgpu_debug")]
    let mut debug_index = 0;

    let span1 = span!(Level::INFO, "Write Metabuffer");
    let _enter1 = span1.enter();

    let data = bytemuck::cast_slice(&meta_array);
    if data.len() as u32 + 256 > META_BUFFER_SIZE {
        panic!("Meta Buffer was to big, length was: {}", data.len());
    }

    //write Meta Buffer
    dev.queue.write_buffer(&dev.meta_buffer, 0, data);
    drop(_enter1);

    let span1 = span!(Level::INFO, "Create Encoder");
    let _enter1 = span1.enter();

    let mut encoder = dev
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    let mut cpass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
        label: None,
        timestamp_writes: None,
    });

    for q in command_queue.iter(){
        match q {
            MlQueue::Dispatch(q) => {
                if let DispatchedBindgroup::CachedBindgroup(bindgroup) = &q.bindgroup {
                    if let Some(pipeline) = &q.pipeline_cached{
                        let qx = q.x;
                        let qy = q.y;
                        let qz = q.z;
                        let meta = q.meta - current_meta as u32;

                        #[cfg(feature = "wgpu_debug")]
                        cpass.write_timestamp(&query_set, debug_index);
                        let span1 = span!(Level::INFO, "Set Pipeline");
                        let _enter1 = span1.enter();
                        cpass.set_pipeline(&pipeline);
                        drop(_enter1);

                        if meta * 4 >= META_BUFFER_SIZE - 256 {
                            panic!(
                                "meta is to big!: meta was {meta}, q.meta: {}/{current_meta}",
                                q.meta
                            );
                        }

                        let span1 = span!(Level::INFO, "Set Bindgroup");
                        let _enter1 = span1.enter();
                        cpass.set_bind_group(0, &bindgroup.bindgroup, &[meta * 4]);
                        drop(_enter1);
                        
                        let span1 = span!(Level::INFO, "Dispatch Workgroups");
                        let _enter1 = span1.enter();
                        cpass.dispatch_workgroups(qx, qy, qz);
                        drop(_enter1);
                        
                        
                        #[cfg(feature = "wgpu_debug")]
                        {
                            cpass.write_timestamp(&query_set, debug_index + 1);
                            dev.debug.insert_info(global_index + debug_index * 8,(
                                    format!("Pipeline: {:?}, {}", q.pipeline.0, q.debug.to_owned().map_or("".to_string(), |s| s)),
                                    q.workload_size as u64,
                                    q.x,
                                    q.y,
                                    q.z,
                                ),
                            );
                            debug_index += 2;
                        }
                    }
                }
            }
        }
    }

    let span2 = span!(Level::INFO, "Drop Cpass");
    let _enter2 = span2.enter();
    drop(cpass);
    drop(_enter2);
    drop(_enter1);


    if let Some(waiting_buffer) = waiting_buffer  {
        let staging_buffer = &dev.staging_probe_buffer;
        encoder.copy_buffer_to_buffer(&waiting_buffer.buffer, 0, &staging_buffer, 0, 4);
    }


    #[cfg(feature = "wgpu_debug")]
    end_debug_queue(
        dev,
        command_queue.len() as u32 * 2,
        global_index,
        &mut encoder,
        &query_set,
    );

    let span1 = span!(Level::INFO, "Encoder Finish");
    let _enter1 = span1.enter();
    let result = encoder.finish();
    drop(_enter1);
    return result;
}

#[instrument]
fn prepare(dev: &WgpuDevice, queue_buffer: &mut QueueBuffer){
    let mut most_needed_storage;
    let mut total_used_storage;
    let queue = &mut queue_buffer.command_queue;
    {
        let mut hasher = DefaultHasher::new();
        for q in queue.iter() {
            match q {
                MlQueue::Dispatch(q) => {
                    q.pipeline.hash(&mut hasher);
                }
            }
        }
        let current_hash = hasher.finish();
        let mut cache = dev.cache.lock().unwrap();
        cache.mappings.set_current_buffer_mapping(current_hash);

        total_used_storage = cache.buffers.buffer_memory - cache.buffers.buffer_memory_free; //the total amount of memory acutally used
        most_needed_storage = total_used_storage;

        let mut buffers_used_at: HashMap<u64, usize> = HashMap::new();

        for (index, q) in queue.iter().enumerate() {
            let mut check_buffer = |buffer: &Arc<BufferReference>| {
                let key: u64 = Arc::as_ptr(buffer) as u64;
                buffers_used_at.insert(key, index);
            };
            match q {
                MlQueue::Dispatch(q) => match &q.bindgroup {
                    DispatchedBindgroup::BindgroupReference(br) => {
                        match br {
                            BindGroupReferenceBase::Bindgroup0(v0) => {
                                check_buffer(v0);
                            }
                            BindGroupReferenceBase::Bindgroup1(v0, v1, _) => {
                                check_buffer(v0);
                                check_buffer(v1);
                            }
                            BindGroupReferenceBase::Bindgroup2(v0, v1, v2, _) => {
                                check_buffer(v0);
                                check_buffer(v1);
                                check_buffer(v2);
                            }
                            BindGroupReferenceBase::Bindgroup3(v0, v1, v2, v3) => {
                                check_buffer(v0);
                                check_buffer(v1);
                                check_buffer(v2);
                                check_buffer(v3);
                            }
                        }
                    }
                    DispatchedBindgroup::None => {continue;},
                    DispatchedBindgroup::CachedBindgroup(_) => todo!(),
                },
            }
        }
        let mut buffer_used = HashSet::new();
        for (index, q) in queue.iter().enumerate() {
            let mut check_buffer = |buffer: &Arc<BufferReference>| {
                let key: u64 = Arc::as_ptr(buffer) as u64;
                let buffer_last_used_index = buffers_used_at.get(&key).unwrap();
                
                if !buffer_used.contains(&key){
                    buffer_used.insert(key);
                    if buffer.storage.lock().unwrap().is_none() {
                        total_used_storage += buffer.size;
                    }
                }

                if *buffer_last_used_index <= index {
                    
                    if !buffer.is_referenced_by_storage.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        if total_used_storage > most_needed_storage {
                            most_needed_storage = total_used_storage;
                        }
                        total_used_storage -= buffer.size;
                    }
                }
            };
            match q {
                MlQueue::Dispatch(q) => match &q.bindgroup {
                    DispatchedBindgroup::BindgroupReference(br) => {
                        match br {
                            BindGroupReferenceBase::Bindgroup0(v0) => {
                                check_buffer(v0);
                            }
                            BindGroupReferenceBase::Bindgroup1(v0, v1, _) => {
                                check_buffer(v0);
                                check_buffer(v1);
                            }
                            BindGroupReferenceBase::Bindgroup2(v0, v1, v2, _) => {
                                check_buffer(v0);
                                check_buffer(v1);
                                check_buffer(v2);
                            }
                            BindGroupReferenceBase::Bindgroup3(v0, v1, v2, v3) => {
                                check_buffer(v0);
                                check_buffer(v1);
                                check_buffer(v2);
                                check_buffer(v3);
                            }
                        }
                    }
                    DispatchedBindgroup::None => {continue;},
                    DispatchedBindgroup::CachedBindgroup(_) => todo!(),
                },
            }
        }
        //allow 25% margin more:
        let most_needed_storage = (most_needed_storage  * 5) / 4;
        
        if most_needed_storage >  cache.buffers.max_memory_allowed{
            cache.buffers.max_memory_allowed = most_needed_storage;
        }
        else{
            cache.buffers.max_memory_allowed = ((7 *  cache.buffers.max_memory_allowed) / 8) + (most_needed_storage/8);
        }
    }
}

#[instrument]
fn set_buffers(dev: &WgpuDevice, command_buffer: &mut QueueBuffer, index : &mut usize, current_meta: usize, last_meta : &mut usize){
    let queue = &mut command_buffer.command_queue; 
    let mut cache_limit = false;
    let mut total_workload = 0u64; //we only allow a certain amount of workload per commandBuffer 
    let start_index = *index; 
    for q in queue[*index..].iter_mut() {
        #[cfg(feature="wgpu_debug")]{
            let ele_size =  *index-start_index;
            if ele_size >= 4095{
                break;
            }
        }

        *index += 1;
        let mut cache = dev.cache.lock().unwrap();

        match q {
            MlQueue::Dispatch(q) => {

              
                let ele_size =  *index-start_index;
                if (total_workload + q.workload_size as u64)  > super::device::MAX_WORKLOAD_SIZE && ele_size > 1 {
                    *index -= 1;
                    break;
                }
                else{
                    total_workload += q.workload_size as u64;
                }
                
                let span1 = span!(Level::INFO, "SetBuffers_Analyse UnaryBuffer ");
                let _enter1 = span1.enter();
                let mut optimize_unary_inplace = false;
                let mut optimize_binary_inplace = false;
                let mut optimize_copy_inplace = false;
                let mut vdest_ref = None;
                let mut v1_ref = None;
                if let Pipelines::Unary(dtype, candle_wgpu_kernels::unary::Functions::UnaryFromBufferContiguous) = &q.pipeline.0{
                    if q.pipeline.2.input1_inplaceable{
                        if let DispatchedBindgroup::BindgroupReference(
                            bindgroup_reference,
                        ) = &q.bindgroup
                        {
                            if let BindGroupReferenceBase::Bindgroup1(vdest, v1, _) =
                                bindgroup_reference
                            {
                                if Arc::strong_count(&v1) == 1 {
                                    //this Bindgroup is the only one, holding a reference to this BufferReference -> So we can Reuse that Buffer
                                    if vdest.size <= v1.size {
                                        if vdest.storage.lock().unwrap().is_none() {
                                            dev.unary_inplace_counter.inc();
                                            q.pipeline.0 = Pipelines::Unary(dtype.clone(), candle_wgpu_kernels::unary::Functions::UnaryInplaceContiguous);
                                            vdest_ref = Some(vdest.clone());
                                            v1_ref = Some(v1.clone());
                                            q.bindgroup =
                                                DispatchedBindgroup::BindgroupReference(
                                                    BindGroupReferenceBase::Bindgroup0(
                                                        v1.clone(),
                                                    ),
                                                );
                                            optimize_unary_inplace = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                else if let Pipelines::Binary(dtype, candle_wgpu_kernels::binary::Functions::BinaryBufferFromBufferContiguousBoth) = &q.pipeline.0{
                    if q.pipeline.2.input1_inplaceable{
                        if let DispatchedBindgroup::BindgroupReference(
                            bindgroup_reference,
                        ) = &q.bindgroup
                        {
                            if let BindGroupReferenceBase::Bindgroup2(vdest, v1, v2,_) =
                                bindgroup_reference
                            {
                                if Arc::strong_count(&v1) == 1 {
                                    //this Bindgroup is the only one, holding a reference to this BufferReference -> So we can Reuse that Buffer
                                    if vdest.size <= v1.size {
                                        if vdest.storage.lock().unwrap().is_none() {
                                            dev.binary_inplace_counter.inc();
                                            q.pipeline.0 = Pipelines::Binary(dtype.clone(), candle_wgpu_kernels::binary::Functions::BinaryBufferInplace1ContiguousBoth);
                                            vdest_ref = Some(vdest.clone());
                                            v1_ref = Some(v1.clone());
                                            q.bindgroup =
                                                DispatchedBindgroup::BindgroupReference(
                                                    BindGroupReferenceBase::Bindgroup1(
                                                        v1.clone(),
                                                        v2.clone(),
                                                        false
                                                    ),
                                                );
                                            optimize_binary_inplace = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    else if q.pipeline.2.input2_inplaceable{
                        if let DispatchedBindgroup::BindgroupReference(
                            bindgroup_reference,
                        ) = &q.bindgroup
                        {
                            if let BindGroupReferenceBase::Bindgroup2(vdest, v1, v2, _) =
                                bindgroup_reference
                            {
                                if Arc::strong_count(&v2) == 1 {
                                    //this Bindgroup is the only one, holding a reference to this BufferReference -> So we can Reuse that Buffer
                                    if vdest.size <= v2.size {
                                        if vdest.storage.lock().unwrap().is_none() {
                                            dev.binary_inplace_counter.inc();
                                            q.pipeline.0 = Pipelines::Binary(dtype.clone(), candle_wgpu_kernels::binary::Functions::BinaryBufferInplace2ContiguousBoth);
                                            vdest_ref = Some(vdest.clone());
                                            v1_ref = Some(v2.clone());
                                            q.bindgroup =
                                                DispatchedBindgroup::BindgroupReference(
                                                    BindGroupReferenceBase::Bindgroup1(
                                                        v2.clone(),
                                                        v1.clone(),
                                                        false
                                                    ),
                                                );
                                            optimize_binary_inplace = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                else if let Pipelines::Copy(_, candle_wgpu_kernels::copy::Functions::Copy) = &q.pipeline.0{
                    if q.pipeline.2.input1_inplaceable{
                        if let DispatchedBindgroup::BindgroupReference(
                            bindgroup_reference,
                        ) = &q.bindgroup
                        {
                            if let BindGroupReferenceBase::Bindgroup1(vdest, v1, _) =
                                bindgroup_reference
                            {
                                if Arc::strong_count(&v1) == 1 {
                                    //this Bindgroup is the only one, holding a reference to this BufferReference -> So we can Reuse that Buffer
                                    if vdest.size <= v1.size {
                                        if vdest.storage.lock().unwrap().is_none() {
                                            //startoffset = 0?
                                            dev.copy_inplace_counter.inc();
                                            let mut vdest_storage = vdest.storage.lock().unwrap();
                                            let mut v1_storage = v1.storage.lock().unwrap();
                                            *vdest_storage = v1_storage.as_ref().cloned();
                                            *v1_storage = None;
                                            optimize_copy_inplace = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                drop(_enter1);

                if !optimize_copy_inplace {
                    let pl: &wgpu::PipelineLayout = match &q.bindgroup {
                        DispatchedBindgroup::BindgroupReference(bindgroup_reference) => {
                            match bindgroup_reference {
                                BindGroupReferenceBase::Bindgroup0(_) => {
                                    &dev.bindgroup_layouts.pipeline_layout0
                                }
                                BindGroupReferenceBase::Bindgroup1(_, _,false) => {
                                    &dev.bindgroup_layouts.pipeline_layout1
                                }
                                BindGroupReferenceBase::Bindgroup1(_, _, true) => {
                                    &dev.bindgroup_layouts.pipeline_layout1_16
                                }
                                BindGroupReferenceBase::Bindgroup2(_, _, _, false) => {
                                    &dev.bindgroup_layouts.pipeline_layout2
                                }
                                BindGroupReferenceBase::Bindgroup2(_, _, _, true) => {
                                    &dev.bindgroup_layouts.pipeline_layout2_16
                                }
                                BindGroupReferenceBase::Bindgroup3(_, _, _, _) => {
                                    &dev.bindgroup_layouts.pipeline_layout3
                                }
                            }
                        }
                        _ => panic!("not expected"),
                    };
    
                    
                    let consts = &command_buffer.id_to_const_array[q.pipeline.1];
                    let pipeline = dev
                        .get_pipeline( &q.pipeline, pl, consts)
                        .unwrap();
    
                    if let DispatchedBindgroup::BindgroupReference(bindgroup_reference) =
                        &q.bindgroup
                    {
                        let bindgroup = cache.get_bind_group(
                            dev,
                            bindgroup_reference,
                            q.pipeline.clone(),
                        );
    
                        if cache.remove_unused(){ //we hit the max cache size
                            cache_limit = true;
                        }
            
                        //this may drop a bufferReference. The BufferReference needs to access cache, therefore cache was droped
                        drop(cache); 
                        q.bindgroup = DispatchedBindgroup::CachedBindgroup(bindgroup);
                        

                        q.pipeline_cached = Some(pipeline);

                        //needs to be deleayed, we want to set v1_storage to None, but to create a BindGroup, we need to have v1_storage set
                        if optimize_unary_inplace || optimize_binary_inplace{
                            if let Some(vdest_ref) = vdest_ref {
                                if let Some(v1_ref) = v1_ref {
                                    let mut vdest_storage = vdest_ref.storage.lock().unwrap();
                                    let mut v1_storage = v1_ref.storage.lock().unwrap();
                                    *vdest_storage = v1_storage.as_ref().cloned();
                                    *v1_storage = None;
                                }
                            }
                        }
                    }  
                }
                else{
                    q.bindgroup = DispatchedBindgroup::None;
                }
                *last_meta = q.meta as usize;

               
                let meta_size = (*last_meta - current_meta) * 4 + 256 * 3;
                if meta_size > META_BUFFER_SIZE as usize
                {
                    break;
                }
                if cache_limit{
                    break;
                }
                if total_workload > super::device::MAX_WORKLOAD_SIZE{
                    break;
                }
            }
        }
    }
    let meta_size = (*last_meta - current_meta) * 4 + 256 * 3;
    let ele_size =  *index-start_index;
    log::trace!("queue {ele_size}, Meta: {meta_size}, workload: {total_workload}, cache_limit: {cache_limit}");

}

#[instrument]
pub(crate) fn flush_gpu_command(dev: &WgpuDevice, queue_buffer: &mut QueueBuffer) {
    if queue_buffer.command_queue.len() > 0 {
        log::warn!("flush_gpu_command");
        prepare(dev, queue_buffer);
        {
            let mut start_index = 0;
            let mut index = 0;
            let mut current_meta: usize = 0;
            let mut last_meta: usize = 0;

            while index < queue_buffer.command_queue.len() {
                set_buffers(dev, queue_buffer, &mut index, current_meta, &mut last_meta);

                let last_meta_index = (last_meta + 256 / 4).min(queue_buffer.get_meta().len());
              
                let cb = get_command_buffer(
                    dev,
                    &queue_buffer.get_meta()[current_meta..last_meta_index],
                    &queue_buffer.command_queue[start_index..index],
                    current_meta,
                    &None
                );
                
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let span1 = span!(Level::INFO, "Device Poll");
                    let _enter1 = span1.enter();
                    dev.device.poll(wgpu::Maintain::wait()).panic_on_timeout();
                    // if !dev.device.poll(wgpu::Maintain::Poll).is_queue_empty(){
                    //     pollster::block_on(synchronize_device(&dev, &dev.queue)).unwrap();
                    // }
                }

                //set last buffer, so we can wait for it to finish in the future
                match &queue_buffer.command_queue[index - 1]{
                    MlQueue::Dispatch(d) => {
                        match &d.bindgroup{
                            DispatchedBindgroup::CachedBindgroup(c) => {
                                //queue_buffer.last_buffer = Some(c.buffers.get_dest().clone())
                            },
                            _ => {},
                        }

                    }
                }
                

                let span1 = span!(Level::INFO, "Submit");
                let _enter1 = span1.enter();
                dev.queue.submit(Some(cb));
                drop(_enter1); 
               
                start_index = index;
                current_meta = last_meta;
            }
        }
        queue_buffer.clear();
        {
            let mut cache = dev.cache.lock().unwrap();

            log::warn!("current memory {} / {}", cache.buffers.buffer_memory, cache.buffers.max_memory_allowed);
            cache.mappings.finish();
            cache.buffers.remove_unused();
            cache.remove_unused();
        }
    }
}

#[instrument]
pub(crate) async fn flush_gpu_command_async(dev: &WgpuDevice, queue_buffer: &mut QueueBuffer) -> crate::Result<()> {
    if queue_buffer.command_queue.len() > 0 {
        log::warn!("flush_gpu_command_async");
        prepare(dev, queue_buffer);
        {
            let mut start_index = 0;
            let mut index = 0;
            let mut current_meta: usize = 0;
            let mut last_meta: usize = 0;

            while index < queue_buffer.command_queue.len() {
                set_buffers(dev, queue_buffer, &mut index, current_meta, &mut last_meta);

                let last_meta_index = (last_meta + 256 / 4).min(queue_buffer.get_meta().len());
              
                let cb = get_command_buffer(
                    dev,
                    &queue_buffer.get_meta()[current_meta..last_meta_index],
                    &queue_buffer.command_queue[start_index..index],
                    current_meta,
                    &queue_buffer.last_buffer
                );
              
                // let span1 = span!(Level::INFO, "Device Poll");
                // let _enter1 = span1.enter();
                //dev.device.poll(wgpu::Maintain::wait()).panic_on_timeout();
                if !dev.device.poll(wgpu::Maintain::Poll).is_queue_empty(){
                    synchronize_device(&dev, &dev.queue).await?;
                }
                
                // if start_index > 0{
                //     //get buffer of prev group
                //     match(queue_buffer.command_queue[start_index-1]){
                //         MlQueue::Dispatch(d) => {
                //             match(d.bindgroup){
                //                 DispatchedBindgroup::CachedBindgroup(c) => {
                                    
                //                     read_data_from_gpu_async_buffer(dev, &c.buffers.get_dest().buffer);

                //                 },
                //                 _ => {},
                //             }
    
                //         }
                //     }
                // }

                let span1 = span!(Level::INFO, "Submit");
                let _enter1 = span1.enter();
                dev.queue.submit(Some(cb));
                drop(_enter1); 
               
                start_index = index;
                current_meta = last_meta;
            }
        }

      
       
       
        queue_buffer.clear();
        {
            let mut cache = dev.cache.lock().unwrap();
            log::warn!("current memory {} / {}", cache.buffers.buffer_memory, cache.buffers.max_memory_allowed);
        
            cache.mappings.finish();
            cache.buffers.remove_unused();
            cache.remove_unused();
        }
    }
    Ok(())
}



fn enqueue(
    command_queue: MutexGuard<QueueBuffer>,
    pipeline: PipelineType,
    bind_group: BindGroupReference,
    length: u32,
    workload_size : usize
) {
    return enqueue_extra(
        command_queue,
        pipeline,
        bind_group,
        length,
        workload_size,
        #[cfg(feature = "wgpu_debug")]
        None,
    );
}

fn enqueue_extra(
    command_queue: MutexGuard<QueueBuffer>,
    pipeline: PipelineType,
    bind_group: BindGroupReference,
    length: u32,
    workload_size : usize,
    #[cfg(feature = "wgpu_debug")] _debug: Option<String>,
) {
    return enqueue_workgroups_extra(
        command_queue,
        pipeline,
        bind_group,
        (length + WORKGROUP_SIZE - 1) / WORKGROUP_SIZE,
        1,
        1,
        workload_size,
        #[cfg(feature = "wgpu_debug")]
        _debug,
    );
}

fn enqueue_big(
    command_queue: MutexGuard<QueueBuffer>,
    pipeline: PipelineType,
    bind_group: BindGroupReference,
    length: u32
) {
    return enqueue_big_extra(
        command_queue,
        pipeline,
        bind_group,
        length,
        #[cfg(feature = "wgpu_debug")]
        None,
    );
}

fn enqueue_big_extra(
    command_queue: MutexGuard<QueueBuffer>,
    pipeline: PipelineType,
    bind_group: BindGroupReference,
    length: u32,
    #[cfg(feature = "wgpu_debug")] _debug: Option<String>,
) {

    let id = (length + WORKGROUP_SIZE - 1) / WORKGROUP_SIZE;
    let x = id.min(65535);
    let y = (id + 65534) / 65535;

    return enqueue_workgroups_extra(
        command_queue,
        pipeline,
        bind_group,
        x,
        y,
        1,
        length as usize,
        #[cfg(feature = "wgpu_debug")]
        _debug,
    );
}

#[instrument]
pub fn create_buffer(dev: &WgpuDevice, size: u64) -> wgpu::Buffer {
    dev.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size,
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_SRC
            | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

#[instrument]
pub fn create_bindgroup(dev: &WgpuDevice, bindgroup: CachedBindGroupReference) -> wgpu::BindGroup {
    dev.cached_bindgroup_counter.inc();

    let buffer_meta = &dev.meta_buffer;

    let meta_binding = wgpu::BufferBinding {
        buffer: &buffer_meta,
        offset: 0,
        size: Some(NonZeroU64::new(256).unwrap()),
    };
    let meta_binding = wgpu::BindingResource::Buffer(meta_binding);

    let meta_entry = wgpu::BindGroupEntry {
        binding: 1,
        resource: meta_binding,
    };

    let bind_group_layout = match bindgroup {
        BindGroupReferenceBase::Bindgroup0(_) => &dev.bindgroup_layouts.bind_group_layout0,
        BindGroupReferenceBase::Bindgroup1(_, _, false) => &dev.bindgroup_layouts.bind_group_layout1,
        BindGroupReferenceBase::Bindgroup1(_, _, true) => &dev.bindgroup_layouts.bind_group_layout1_16,
        BindGroupReferenceBase::Bindgroup2(_, _, _, false) => &dev.bindgroup_layouts.bind_group_layout2,
        BindGroupReferenceBase::Bindgroup2(_, _, _, true) => &dev.bindgroup_layouts.bind_group_layout2_16,
        BindGroupReferenceBase::Bindgroup3(_, _, _, _) => &dev.bindgroup_layouts.bind_group_layout3,
    };

    match bindgroup {
        CachedBindGroupReference::Bindgroup0(buffer_dest) => {
            let entries = &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer_dest.buffer.as_entire_binding(),
                },
                meta_entry,
            ];
            dev.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &bind_group_layout,
                entries: entries,
            })
        }
        CachedBindGroupReference::Bindgroup1(buffer_dest, buffer_input1, _) => {
            let entries = &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer_dest.buffer.as_entire_binding(),
                },
                meta_entry,
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: buffer_input1.buffer.as_entire_binding(),
                },
            ];
            dev.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &bind_group_layout,
                entries: entries,
            })
        }
        CachedBindGroupReference::Bindgroup2(buffer_dest, buffer_input1, buffer_input2, _) => {
            let entries = &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer_dest.buffer.as_entire_binding(),
                },
                meta_entry,
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: buffer_input1.buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: buffer_input2.buffer.as_entire_binding(),
                },
            ];
            dev.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &bind_group_layout,
                entries: entries,
            })
        }
        CachedBindGroupReference::Bindgroup3(
            buffer_dest,
            buffer_input1,
            buffer_input2,
            buffer_input3,
        ) => {
            let entries = &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer_dest.buffer.as_entire_binding(),
                },
                meta_entry,
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: buffer_input1.buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: buffer_input2.buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: buffer_input3.buffer.as_entire_binding(),
                },
            ];
            dev.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &bind_group_layout,
                entries: entries,
            })
        }
    }
}

fn create_bind_group_input0(buffer_dest: Arc<BufferReference>) -> BindGroupReference {
    BindGroupReference::Bindgroup0(buffer_dest)
}

fn create_bind_group_input1(
    buffer_dest: Arc<BufferReference>,
    buffer_input1: Arc<BufferReference>,
) -> BindGroupReference {
    BindGroupReference::Bindgroup1(buffer_dest, buffer_input1, false)
}

fn create_bind_group_input1_16(
    buffer_dest: Arc<BufferReference>,
    buffer_input1: Arc<BufferReference>,
) -> BindGroupReference {
    BindGroupReference::Bindgroup1(buffer_dest, buffer_input1, true)
}

fn create_bind_group_input2(
    buffer_dest: Arc<BufferReference>,
    buffer_input1: Arc<BufferReference>,
    buffer_input2: Arc<BufferReference>,
) -> BindGroupReference {
    BindGroupReference::Bindgroup2(buffer_dest, buffer_input1, buffer_input2, false)
}

fn create_bind_group_input2_16(
    buffer_dest: Arc<BufferReference>,
    buffer_input1: Arc<BufferReference>,
    buffer_input2: Arc<BufferReference>,
) -> BindGroupReference {
    BindGroupReference::Bindgroup2(buffer_dest, buffer_input1, buffer_input2, true)
}

fn create_bind_group_input3(
    buffer_dest: Arc<BufferReference>,
    buffer_input1: Arc<BufferReference>,
    buffer_input2: Arc<BufferReference>,
    buffer_input3: Arc<BufferReference>,
) -> BindGroupReference {
    BindGroupReference::Bindgroup3(buffer_dest, buffer_input1, buffer_input2, buffer_input3)
}

#[instrument]
pub fn synchronize(dev: &WgpuDevice) -> crate::Result<()> {
    let mut command_queue = dev.command_queue.lock().unwrap();
    if command_queue.command_queue.len() > 0{
        flush_gpu_command(dev, &mut command_queue);
        if let Some(buffer) = &command_queue.last_buffer{
            copy_to_staging_prope(dev, &buffer.buffer);
        }
       
        return pollster::block_on(synchronize_device(&dev, &dev.queue));
    }
    Ok(())
}

#[instrument]
pub async fn synchronize_async(dev: &WgpuDevice) -> crate::Result<()> {
    let mut command_queue = dev.command_queue.lock().unwrap();
    if command_queue.command_queue.len() > 0{
        flush_gpu_command_async(dev, &mut command_queue).await?;
        if let Some(buffer) = &command_queue.last_buffer{
            copy_to_staging_prope(dev, &buffer.buffer);
        }
        return synchronize_device(&dev, &dev.queue).await;
    }
    Ok(())
}


// #[instrument]
// async fn synchronize_device(dev: &Device, queue: &Queue) -> crate::Result<()> {
//     let (sender, receiver) = flume::bounded(1);
//     queue.on_submitted_work_done(move || sender.send(()).unwrap());

//     dev.poll(wgpu::Maintain::wait()).panic_on_timeout();
//     if let Ok(()) = receiver.recv_async().await {
//         return Ok(());
//     }
//     Ok(())
// }


#[instrument]
async fn synchronize_device(dev: &WgpuDevice, queue: &Queue) -> crate::Result<()> {
    wait_for_gpu_buffer_async(dev).await
}


#[instrument]
pub async fn read_data_from_gpu_async<T: bytemuck::Pod>(
    dev: &WgpuDevice,
    buffer: Arc<BufferReference>,
) -> crate::Result<Vec<T>> {
    let mut command_queue = dev.command_queue.lock().unwrap();
    flush_gpu_command_async(dev, &mut command_queue).await?; //send all previous commands to the gpu
  
    let buffer_storage = buffer.storage.lock().unwrap();
    if let Some(buffer) = buffer_storage.as_ref() {
        Ok(read_data_from_gpu_async_buffer(dev, &buffer.buffer).await)
    } else {
        panic!("Unespected error at read_data from gpu. Tensor WgpuStorage did not Point to a wgpu Buffer")
    }
}


pub fn copy_to_staging_prope(dev: &WgpuDevice, buffer: &wgpu::Buffer){
    let mut encoder = dev
    .device
    .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

    let staging_buffer = &dev.staging_probe_buffer;

    encoder.copy_buffer_to_buffer(&buffer, 0, &staging_buffer, 0, 4);

    // Submits command encoder for processing
    dev.queue.submit(Some(encoder.finish()));
}

#[instrument]
//wait for the current staging buffer, 
//the buffer one wants to 
pub async fn wait_for_gpu_buffer_async(
    dev: &WgpuDevice,
) -> crate::Result<()> {
    let staging_buffer = &dev.staging_probe_buffer;

    // Note that we're not calling `.await` here.
    let buffer_slice = staging_buffer.slice(..);
    // Sets the buffer up for mapping, sending over the result of the mapping back to us when it is finished.
    let (sender, receiver) = flume::bounded(1);
    buffer_slice.map_async(wgpu::MapMode::Read, move |v| sender.send(v).unwrap());

    // Poll the device in a blocking manner so that our future resolves.
    // In an actual application, `device.poll(...)` should
    // be called in an event loop or on another thread.
    dev.device.poll(wgpu::Maintain::wait()).panic_on_timeout();

    // Awaits until `buffer_future` can be read from
    if let Ok(Ok(())) = receiver.recv_async().await {
        staging_buffer.unmap(); // Unmaps buffer from memory
                                // If you are familiar with C++ these 2 lines can be thought of similarly to:
                                //   delete myPointer;
                                //   myPointer = NULL;
                                // It effectively frees the memory
        // Returns data from buffer
        Ok(())
    } else {
        panic!("failed to run compute on gpu!")
    }
}



#[instrument]
pub async fn read_data_from_gpu_async_buffer<T: bytemuck::Pod>(
    dev: &WgpuDevice,
    buffer: &wgpu::Buffer,
) -> Vec<T> {
    let dest_size = buffer.size();

    //TODO: use cached staging buffer!
    let staging_buffer = dev.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: dest_size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = dev
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

    encoder.copy_buffer_to_buffer(&buffer, 0, &staging_buffer, 0, dest_size);

    // Submits command encoder for processing
    dev.queue.submit(Some(encoder.finish()));

    // Note that we're not calling `.await` here.
    let buffer_slice = staging_buffer.slice(..);
    // Sets the buffer up for mapping, sending over the result of the mapping back to us when it is finished.
    let (sender, receiver) = flume::bounded(1);
    buffer_slice.map_async(wgpu::MapMode::Read, move |v| sender.send(v).unwrap());

    // Poll the device in a blocking manner so that our future resolves.
    // In an actual application, `device.poll(...)` should
    // be called in an event loop or on another thread.
    dev.device.poll(wgpu::Maintain::wait()).panic_on_timeout();

    // Awaits until `buffer_future` can be read from
    if let Ok(Ok(())) = receiver.recv_async().await {
        // Gets contents of buffer
        let data = buffer_slice.get_mapped_range();
        // Since contents are got in bytes, this converts these bytes back to u32
        let result: Vec<T> = bytemuck::cast_slice(&data).to_vec();

        // With the current interface, we have to make sure all mapped views are
        // dropped before we unmap the buffer.
        drop(data);
        staging_buffer.unmap(); // Unmaps buffer from memory
                                // If you are familiar with C++ these 2 lines can be thought of similarly to:
                                //   delete myPointer;
                                //   myPointer = NULL;
                                // It effectively frees the memory

        // Returns data from buffer
        result
    } else {
        panic!("failed to run compute on gpu!")
    }
}
