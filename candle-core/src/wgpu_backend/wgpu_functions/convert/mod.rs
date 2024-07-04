use std::sync::Arc;


use crate::{wgpu::{cache::BufferReference, device::Pipelines}, WgpuDevice};

use super::{create_bind_group_input1, enqueue, get_meta};

pub fn queue_convert_u32_to_f32(
    dev: &WgpuDevice,
    buffer_dest: Arc<BufferReference>,
    buffer_input: Arc<BufferReference>,
    input_layout: &crate::Layout,
) -> crate::Result<()> {
    let mut meta = get_meta(&dev);
    meta.add_layout(&input_layout);

    let pipeline = dev.get_pipeline(super::Shader::Convert(crate::DType::U32), Pipelines::ConvertU32ToF32)?;
    let bind_group = create_bind_group_input1( buffer_dest, buffer_input);
    enqueue(
        meta,
        pipeline,
        bind_group,
        input_layout.shape().elem_count() as u32,
        #[cfg(feature = "wgpu_debug")] 
        crate::wgpu::device::QueueDebugInfo::new(&format!("u32_to_f32"), input_layout.shape().elem_count()),
    );
    return Ok(());
}


pub fn queue_convert_u8_to_f32(
    dev: &WgpuDevice,
    buffer_dest: Arc<BufferReference>,
    buffer_input: Arc<BufferReference>,
    input_layout: &crate::Layout,
) -> crate::Result<()> {
    let mut meta = get_meta(&dev);
    meta.add_layout(&input_layout);

    let pipeline = dev.get_pipeline(super::Shader::Convert(crate::DType::U8), Pipelines::ConvertU8ToF32)?;
    let bind_group = create_bind_group_input1( buffer_dest, buffer_input);
    enqueue(
        meta,
        pipeline,
        bind_group,
        input_layout.shape().elem_count() as u32,
        #[cfg(feature = "wgpu_debug")] 
        crate::wgpu::device::QueueDebugInfo::new(&format!("u8_to_f32"), input_layout.shape().elem_count()),
    );
    return Ok(());
}

pub fn queue_convert_f32_to_u32(
    dev: &WgpuDevice,
    buffer_dest: Arc<BufferReference>,
    buffer_input: Arc<BufferReference>,
    input_layout: &crate::Layout,
) -> crate::Result<()> {
    let mut meta = get_meta(&dev);
    meta.add_layout(&input_layout);

    let pipeline = dev.get_pipeline(super::Shader::Convert(crate::DType::F32), Pipelines::ConvertF32ToU32)?;

    let bind_group = create_bind_group_input1( buffer_dest, buffer_input);
    enqueue(
        meta,
        pipeline,
        bind_group,
        input_layout.shape().elem_count() as u32,
        #[cfg(feature = "wgpu_debug")] 
        crate::wgpu::device::QueueDebugInfo::new(&format!("f32_to_u32"), input_layout.shape().elem_count()),
    );
    return Ok(());
}


pub fn queue_convert_u32_to_u8(
    dev: &WgpuDevice,
    buffer_dest: Arc<BufferReference>,
    buffer_input: Arc<BufferReference>,
    start_offset: u32,
    size : u32
) -> crate::Result<()> {
    let mut meta = get_meta(&dev);
    meta.add(start_offset);
    meta.add(size);

    let pipeline = dev.get_pipeline(super::Shader::Convert(crate::DType::U32), Pipelines::ConvertU32ToU8)?;

    let bind_group = create_bind_group_input1( buffer_dest, buffer_input);
    enqueue(
        meta,
        pipeline,
        bind_group,
        ((size + 3) / 4) as u32,
        #[cfg(feature = "wgpu_debug")] 
        crate::wgpu::device::QueueDebugInfo::new(&format!("u32_to_u8"), size),
    );
    return Ok(());
}

pub fn queue_convert_f32_to_u8(
    dev: &WgpuDevice,
    buffer_dest: Arc<BufferReference>,
    buffer_input: Arc<BufferReference>,
    start_offset: u32,
    size : u32
) -> crate::Result<()> {
    let mut meta = get_meta(&dev);
    meta.add(start_offset);
    meta.add(size);

    let pipeline = dev.get_pipeline(super::Shader::Convert(crate::DType::F32), Pipelines::ConvertF32ToU8)?;

    let bind_group = create_bind_group_input1( buffer_dest, buffer_input);
    enqueue(
        meta,
        pipeline,
        bind_group,
        ((size + 3) / 4) as u32,
        #[cfg(feature = "wgpu_debug")] 
        crate::wgpu::device::QueueDebugInfo::new(&format!("f32_to_u8"), size),
    );
    return Ok(());
}