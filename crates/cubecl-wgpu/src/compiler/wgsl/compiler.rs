use std::{borrow::Cow, sync::Arc};

use super::{shader::ComputeShader, ConstantArray, Item, SharedMemory};
use super::{LocalArray, Subgroup};
use crate::{
    compiler::{base::WgpuCompiler, wgsl},
    WgpuServer,
};
use cubecl_core::{
    ir::{self as cube, HybridAllocator, UIntKind},
    prelude::CompiledKernel,
    server::ComputeServer,
    Feature, Metadata,
};
use cubecl_runtime::{DeviceProperties, ExecutionMode};
use wgpu::{
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BufferBindingType,
    ComputePipeline, DeviceDescriptor, PipelineLayoutDescriptor, ShaderModuleDescriptor,
    ShaderStages,
};

/// Wgsl Compiler.
#[derive(Clone, Default)]
pub struct WgslCompiler {
    num_inputs: usize,
    num_outputs: usize,
    metadata: Metadata,
    ext_meta_pos: Vec<u32>,
    local_invocation_index: bool,
    local_invocation_id: bool,
    global_invocation_id: bool,
    workgroup_id: bool,
    subgroup_size: bool,
    id: bool,
    num_workgroups: bool,
    workgroup_id_no_axis: bool,
    workgroup_size_no_axis: bool,
    num_workgroup_no_axis: bool,
    shared_memories: Vec<SharedMemory>,
    const_arrays: Vec<ConstantArray>,
    local_arrays: Vec<LocalArray>,
}

impl core::fmt::Debug for WgslCompiler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WgslCompiler")
    }
}

impl cubecl_core::Compiler for WgslCompiler {
    type Representation = ComputeShader;

    fn compile(shader: cube::KernelDefinition, _mode: ExecutionMode) -> Self::Representation {
        let mut compiler = Self::default();
        compiler.compile_shader(shader)
    }

    fn elem_size(elem: cube::Elem) -> usize {
        Self::compile_elem(elem).size()
    }

    fn max_shared_memory_size() -> usize {
        32768
    }

    fn local_allocator() -> impl cube::LocalAllocator {
        HybridAllocator::default()
    }
}

impl WgpuCompiler for WgslCompiler {
    fn create_pipeline(
        server: &mut WgpuServer<Self>,
        kernel: CompiledKernel<Self>,
        mode: ExecutionMode,
    ) -> Arc<ComputePipeline> {
        let source = &kernel.source;
        let repr = kernel.repr.unwrap();
        let module = match mode {
            ExecutionMode::Checked => server.device.create_shader_module(ShaderModuleDescriptor {
                label: None,
                source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(source)),
            }),
            ExecutionMode::Unchecked => unsafe {
                server
                    .device
                    .create_shader_module_unchecked(ShaderModuleDescriptor {
                        label: None,
                        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(source)),
                    })
            },
        };

        let num_bindings = repr.inputs.len() + repr.outputs.len() + repr.named.len();
        let bindings = (0..num_bindings)
            .map(|i| BindGroupLayoutEntry {
                binding: i as u32,
                visibility: ShaderStages::COMPUTE,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Storage { read_only: false },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            })
            .collect::<Vec<_>>();
        let layout = server
            .device
            .create_bind_group_layout(&BindGroupLayoutDescriptor {
                label: None,
                entries: &bindings,
            });
        let layout = server
            .device
            .create_pipeline_layout(&PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[&layout],
                push_constant_ranges: &[],
            });

        Arc::new(
            server
                .device
                .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: None,
                    layout: Some(&layout),
                    module: &module,
                    entry_point: "main",
                    compilation_options: wgpu::PipelineCompilationOptions {
                        zero_initialize_workgroup_memory: false,
                        ..Default::default()
                    },
                    cache: None,
                }),
        )
    }

    fn compile(
        _server: &mut WgpuServer<Self>,
        kernel: <WgpuServer<Self> as ComputeServer>::Kernel,
        mode: ExecutionMode,
    ) -> CompiledKernel<Self> {
        kernel.compile(mode)
    }

    async fn request_device(adapter: &wgpu::Adapter) -> (wgpu::Device, wgpu::Queue) {
        let limits = adapter.limits();
        adapter
            .request_device(
                &DeviceDescriptor {
                    label: None,
                    required_features: adapter.features(),
                    required_limits: limits,
                    // The default is MemoryHints::Performance, which tries to do some bigger
                    // block allocations. However, we already batch allocations, so we
                    // can use MemoryHints::MemoryUsage to lower memory usage.
                    memory_hints: wgpu::MemoryHints::MemoryUsage,
                },
                None,
            )
            .await
            .map_err(|err| {
                format!(
                    "Unable to request the device with the adapter {:?}, err {:?}",
                    adapter.get_info(),
                    err
                )
            })
            .unwrap()
    }

    fn register_features(
        _adapter: &wgpu::Adapter,
        _device: &wgpu::Device,
        props: &mut DeviceProperties<Feature>,
    ) {
        register_types(props);
    }
}

fn register_types(props: &mut DeviceProperties<Feature>) {
    use cubecl_core::ir::{Elem, FloatKind, IntKind};

    let supported_types = [
        Elem::UInt(UIntKind::U32),
        Elem::Int(IntKind::I32),
        Elem::AtomicInt(IntKind::I32),
        Elem::AtomicUInt(UIntKind::U32),
        Elem::Float(FloatKind::F32),
        Elem::Float(FloatKind::Flex32),
        Elem::Bool,
    ];

    for ty in supported_types {
        props.register_feature(Feature::Type(ty));
    }
}

impl WgslCompiler {
    fn compile_shader(&mut self, mut value: cube::KernelDefinition) -> wgsl::ComputeShader {
        self.num_inputs = value.inputs.len();
        self.num_outputs = value.outputs.len();
        let num_meta = value.inputs.len() + value.outputs.len();

        self.ext_meta_pos = Vec::new();
        let mut num_ext = 0;

        for binding in value.inputs.iter().chain(value.outputs.iter()) {
            self.ext_meta_pos.push(num_ext);
            if binding.has_extended_meta {
                num_ext += 1;
            }
        }

        self.metadata = Metadata::new(num_meta as u32, num_ext);

        let instructions = self.compile_scope(&mut value.body);
        let extensions = register_extensions(&instructions);
        let body = wgsl::Body {
            instructions,
            id: self.id,
        };

        wgsl::ComputeShader {
            inputs: value
                .inputs
                .into_iter()
                .map(Self::compile_binding)
                .collect(),
            outputs: value
                .outputs
                .into_iter()
                .map(Self::compile_binding)
                .collect(),
            named: value
                .named
                .into_iter()
                .map(|(name, binding)| (name, Self::compile_binding(binding)))
                .collect(),
            shared_memories: self.shared_memories.clone(),
            constant_arrays: self.const_arrays.clone(),
            local_arrays: self.local_arrays.clone(),
            workgroup_size: value.cube_dim,
            global_invocation_id: self.global_invocation_id || self.id,
            local_invocation_index: self.local_invocation_index,
            local_invocation_id: self.local_invocation_id,
            num_workgroups: self.id
                || self.num_workgroups
                || self.num_workgroup_no_axis
                || self.workgroup_id_no_axis,
            workgroup_id: self.workgroup_id || self.workgroup_id_no_axis,
            subgroup_size: self.subgroup_size,
            body,
            extensions,
            num_workgroups_no_axis: self.num_workgroup_no_axis,
            workgroup_id_no_axis: self.workgroup_id_no_axis,
            workgroup_size_no_axis: self.workgroup_size_no_axis,
        }
    }

    fn compile_item(item: cube::Item) -> Item {
        let elem = Self::compile_elem(item.elem);
        match item.vectorization.map(|it| it.get()).unwrap_or(1) {
            1 => wgsl::Item::Scalar(elem),
            2 => wgsl::Item::Vec2(elem),
            3 => wgsl::Item::Vec3(elem),
            4 => wgsl::Item::Vec4(elem),
            _ => panic!("Unsupported vectorizations scheme {:?}", item.vectorization),
        }
    }

    fn compile_elem(value: cube::Elem) -> wgsl::Elem {
        match value {
            cube::Elem::Float(f) => match f {
                cube::FloatKind::F16 => panic!("f16 is not yet supported"),
                cube::FloatKind::BF16 => panic!("bf16 is not a valid WgpuElement"),
                cube::FloatKind::TF32 => panic!("tf32 is not a valid WgpuElement"),
                cube::FloatKind::Flex32 => wgsl::Elem::F32,
                cube::FloatKind::F32 => wgsl::Elem::F32,
                cube::FloatKind::F64 => panic!("f64 is not a valid WgpuElement"),
            },
            cube::Elem::Int(i) => match i {
                cube::IntKind::I32 => wgsl::Elem::I32,
                kind => panic!("{kind:?} is not a valid WgpuElement"),
            },
            cube::Elem::UInt(kind) => match kind {
                cube::UIntKind::U32 => wgsl::Elem::U32,
                kind => panic!("{kind:?} is not a valid WgpuElement"),
            },
            cube::Elem::Bool => wgsl::Elem::Bool,
            cube::Elem::AtomicInt(i) => match i {
                cube::IntKind::I32 => wgsl::Elem::AtomicI32,
                kind => panic!("atomic<{kind:?}> is not a valid WgpuElement"),
            },
            cube::Elem::AtomicUInt(kind) => match kind {
                cube::UIntKind::U32 => wgsl::Elem::AtomicU32,
                kind => panic!("{kind:?} is not a valid WgpuElement"),
            },
        }
    }

    fn ext_meta_pos(&self, var: &cube::Variable) -> u32 {
        let pos = match var.kind {
            cube::VariableKind::GlobalInputArray(id) => id as usize,
            cube::VariableKind::GlobalOutputArray(id) => self.num_inputs + id as usize,
            _ => panic!("Only global arrays have metadata"),
        };
        self.ext_meta_pos[pos]
    }

    pub(crate) fn compile_variable(&mut self, value: cube::Variable) -> wgsl::Variable {
        let item = value.item;
        match value.kind {
            cube::VariableKind::GlobalInputArray(id) => {
                wgsl::Variable::GlobalInputArray(id, Self::compile_item(item))
            }
            cube::VariableKind::GlobalScalar(id) => {
                wgsl::Variable::GlobalScalar(id, Self::compile_elem(item.elem), item.elem)
            }
            cube::VariableKind::Local { id, depth }
            | cube::VariableKind::Versioned { id, depth, .. } => wgsl::Variable::Local {
                id,
                item: Self::compile_item(item),
                depth,
            },
            cube::VariableKind::LocalBinding { id, .. } => wgsl::Variable::LocalBinding {
                id,
                item: Self::compile_item(item),
            },
            cube::VariableKind::Slice { id, depth } => wgsl::Variable::Slice {
                id,
                item: Self::compile_item(item),
                depth,
            },
            cube::VariableKind::GlobalOutputArray(id) => {
                wgsl::Variable::GlobalOutputArray(id, Self::compile_item(item))
            }
            cube::VariableKind::ConstantScalar(value) => {
                wgsl::Variable::ConstantScalar(value, Self::compile_elem(value.elem()))
            }
            cube::VariableKind::SharedMemory { id, length } => {
                let item = Self::compile_item(item);
                if !self.shared_memories.iter().any(|s| s.index == id) {
                    self.shared_memories
                        .push(SharedMemory::new(id, item, length));
                }
                wgsl::Variable::SharedMemory(id, item, length)
            }
            cube::VariableKind::ConstantArray { id, length } => {
                let item = Self::compile_item(item);
                wgsl::Variable::ConstantArray(id, item, length)
            }
            cube::VariableKind::LocalArray { id, depth, length } => {
                let item = Self::compile_item(item);
                if !self.local_arrays.iter().any(|s| s.index == id) {
                    self.local_arrays
                        .push(LocalArray::new(id, item, depth, length));
                }
                wgsl::Variable::LocalArray(id, item, depth, length)
            }
            cube::VariableKind::Builtin(builtin) => match builtin {
                cube::Builtin::AbsolutePos => {
                    self.id = true;
                    wgsl::Variable::Id
                }
                cube::Builtin::UnitPos => {
                    self.local_invocation_index = true;
                    wgsl::Variable::LocalInvocationIndex
                }
                cube::Builtin::UnitPosX => {
                    self.local_invocation_id = true;
                    wgsl::Variable::LocalInvocationIdX
                }
                cube::Builtin::UnitPosY => {
                    self.local_invocation_id = true;
                    wgsl::Variable::LocalInvocationIdY
                }
                cube::Builtin::UnitPosZ => {
                    self.local_invocation_id = true;
                    wgsl::Variable::LocalInvocationIdZ
                }
                cube::Builtin::CubePosX => {
                    self.workgroup_id = true;
                    wgsl::Variable::WorkgroupIdX
                }
                cube::Builtin::CubePosY => {
                    self.workgroup_id = true;
                    wgsl::Variable::WorkgroupIdY
                }
                cube::Builtin::CubePosZ => {
                    self.workgroup_id = true;
                    wgsl::Variable::WorkgroupIdZ
                }
                cube::Builtin::AbsolutePosX => {
                    self.global_invocation_id = true;
                    wgsl::Variable::GlobalInvocationIdX
                }
                cube::Builtin::AbsolutePosY => {
                    self.global_invocation_id = true;
                    wgsl::Variable::GlobalInvocationIdY
                }
                cube::Builtin::AbsolutePosZ => {
                    self.global_invocation_id = true;
                    wgsl::Variable::GlobalInvocationIdZ
                }
                cube::Builtin::CubeDimX => wgsl::Variable::WorkgroupSizeX,
                cube::Builtin::CubeDimY => wgsl::Variable::WorkgroupSizeY,
                cube::Builtin::CubeDimZ => wgsl::Variable::WorkgroupSizeZ,
                cube::Builtin::CubeCountX => {
                    self.num_workgroups = true;
                    wgsl::Variable::NumWorkgroupsX
                }
                cube::Builtin::CubeCountY => {
                    self.num_workgroups = true;
                    wgsl::Variable::NumWorkgroupsY
                }
                cube::Builtin::CubeCountZ => {
                    self.num_workgroups = true;
                    wgsl::Variable::NumWorkgroupsZ
                }
                cube::Builtin::CubePos => {
                    self.workgroup_id_no_axis = true;
                    wgsl::Variable::WorkgroupId
                }
                cube::Builtin::CubeDim => {
                    self.workgroup_size_no_axis = true;
                    wgsl::Variable::WorkgroupSize
                }
                cube::Builtin::CubeCount => {
                    self.num_workgroup_no_axis = true;
                    wgsl::Variable::NumWorkgroups
                }
                cube::Builtin::SubcubeDim => {
                    self.subgroup_size = true;
                    wgsl::Variable::SubgroupSize
                }
            },
            cube::VariableKind::Matrix { .. } => {
                panic!("Cooperative matrix-multiply and accumulate not supported.")
            }
        }
    }

    fn compile_scope(&mut self, value: &mut cube::Scope) -> Vec<wgsl::Instruction> {
        let mut instructions = Vec::new();

        let const_arrays = value
            .const_arrays
            .drain(..)
            .map(|(var, values)| ConstantArray {
                index: var.index().unwrap(),
                item: Self::compile_item(var.item),
                size: values.len() as u32,
                values: values
                    .into_iter()
                    .map(|val| self.compile_variable(val))
                    .collect(),
            })
            .collect::<Vec<_>>();
        self.const_arrays.extend(const_arrays);

        let processing = value.process();

        for var in processing.variables {
            // We don't declare slices.
            if let cube::VariableKind::Slice { .. } = var.kind {
                continue;
            }

            instructions.push(wgsl::Instruction::DeclareVariable {
                var: self.compile_variable(var),
            });
        }

        processing
            .operations
            .into_iter()
            .for_each(|op| self.compile_operation(&mut instructions, op.operation, op.out));

        instructions
    }

    fn compile_operation(
        &mut self,
        instructions: &mut Vec<wgsl::Instruction>,
        operation: cube::Operation,
        out: Option<cube::Variable>,
    ) {
        match operation {
            cube::Operation::Copy(variable) => instructions.push(wgsl::Instruction::Assign {
                input: self.compile_variable(variable),
                out: self.compile_variable(out.unwrap()),
            }),
            cube::Operation::Operator(op) => instructions.push(self.compile_instruction(op, out)),
            cube::Operation::Atomic(op) => instructions.push(self.compile_atomic(op, out)),
            cube::Operation::Metadata(op) => instructions.push(self.compile_metadata(op, out)),
            cube::Operation::Branch(val) => self.compile_branch(instructions, val),
            cube::Operation::Synchronization(val) => {
                self.compile_synchronization(instructions, val)
            }
            cube::Operation::Subcube(op) => self.compile_subgroup(instructions, op, out),
            cube::Operation::CoopMma(_) => {
                panic!("Cooperative matrix-multiply and accumulate isn't supported on wgpu.")
            }
        }
    }

    fn compile_subgroup(
        &mut self,
        instructions: &mut Vec<wgsl::Instruction>,
        subgroup: cube::Subcube,
        out: Option<cube::Variable>,
    ) {
        let out = out.unwrap();
        let op = match subgroup {
            cube::Subcube::Elect => Subgroup::Elect {
                out: self.compile_variable(out),
            },
            cube::Subcube::All(op) => Subgroup::All {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Subcube::Any(op) => Subgroup::Any {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Subcube::Broadcast(op) => Subgroup::Broadcast {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Subcube::Sum(op) => Subgroup::Sum {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Subcube::Prod(op) => Subgroup::Prod {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Subcube::Min(op) => Subgroup::Min {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Subcube::Max(op) => Subgroup::Max {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
        };

        instructions.push(wgsl::Instruction::Subgroup(op));
    }

    fn compile_branch(&mut self, instructions: &mut Vec<wgsl::Instruction>, branch: cube::Branch) {
        match branch {
            cube::Branch::If(mut op) => instructions.push(wgsl::Instruction::If {
                cond: self.compile_variable(op.cond),
                instructions: self.compile_scope(&mut op.scope),
            }),
            cube::Branch::IfElse(mut op) => instructions.push(wgsl::Instruction::IfElse {
                cond: self.compile_variable(op.cond),
                instructions_if: self.compile_scope(&mut op.scope_if),
                instructions_else: self.compile_scope(&mut op.scope_else),
            }),
            cube::Branch::Switch(mut op) => instructions.push(wgsl::Instruction::Switch {
                value: self.compile_variable(op.value),
                instructions_default: self.compile_scope(&mut op.scope_default),
                cases: op
                    .cases
                    .into_iter()
                    .map(|(val, mut scope)| {
                        (self.compile_variable(val), self.compile_scope(&mut scope))
                    })
                    .collect(),
            }),
            cube::Branch::Return => instructions.push(wgsl::Instruction::Return),
            cube::Branch::Break => instructions.push(wgsl::Instruction::Break),
            cube::Branch::RangeLoop(mut range_loop) => {
                instructions.push(wgsl::Instruction::RangeLoop {
                    i: self.compile_variable(range_loop.i),
                    start: self.compile_variable(range_loop.start),
                    end: self.compile_variable(range_loop.end),
                    step: range_loop.step.map(|it| self.compile_variable(it)),
                    inclusive: range_loop.inclusive,
                    instructions: self.compile_scope(&mut range_loop.scope),
                })
            }
            cube::Branch::Loop(mut op) => instructions.push(wgsl::Instruction::Loop {
                instructions: self.compile_scope(&mut op.scope),
            }),
        };
    }

    fn compile_synchronization(
        &mut self,
        instructions: &mut Vec<wgsl::Instruction>,
        synchronization: cube::Synchronization,
    ) {
        match synchronization {
            cube::Synchronization::SyncUnits => {
                instructions.push(wgsl::Instruction::WorkgroupBarrier)
            }
            cube::Synchronization::SyncStorage => {
                instructions.push(wgsl::Instruction::StorageBarrier)
            }
        };
    }

    fn compile_metadata(
        &mut self,
        metadata: cube::Metadata,
        out: Option<cube::Variable>,
    ) -> wgsl::Instruction {
        let out = out.unwrap();
        match metadata {
            cube::Metadata::Rank { var } => {
                let position = self.ext_meta_pos(&var);
                let offset = self.metadata.rank_index(position);
                wgsl::Instruction::Metadata {
                    out: self.compile_variable(out),
                    info_offset: self.compile_variable(offset.into()),
                }
            }
            cube::Metadata::Stride { dim, var } => {
                let position = self.ext_meta_pos(&var);
                let offset = self.metadata.stride_offset_index(position);
                wgsl::Instruction::ExtendedMeta {
                    info_offset: self.compile_variable(offset.into()),
                    dim: self.compile_variable(dim),
                    out: self.compile_variable(out),
                }
            }
            cube::Metadata::Shape { dim, var } => {
                let position = self.ext_meta_pos(&var);
                let offset = self.metadata.shape_offset_index(position);
                wgsl::Instruction::ExtendedMeta {
                    info_offset: self.compile_variable(offset.into()),
                    dim: self.compile_variable(dim),
                    out: self.compile_variable(out),
                }
            }
            cube::Metadata::Length { var } => match var.kind {
                cube::VariableKind::GlobalInputArray(id) => {
                    let offset = self.metadata.len_index(id as u32);
                    wgsl::Instruction::Metadata {
                        out: self.compile_variable(out),
                        info_offset: self.compile_variable(offset.into()),
                    }
                }
                cube::VariableKind::GlobalOutputArray(id) => {
                    let offset = self.metadata.len_index(self.num_inputs as u32 + id as u32);
                    wgsl::Instruction::Metadata {
                        out: self.compile_variable(out),
                        info_offset: self.compile_variable(offset.into()),
                    }
                }
                _ => wgsl::Instruction::Length {
                    var: self.compile_variable(var),
                    out: self.compile_variable(out),
                },
            },
            cube::Metadata::BufferLength { var } => match var.kind {
                cube::VariableKind::GlobalInputArray(id) => {
                    let offset = self.metadata.buffer_len_index(id as u32);
                    wgsl::Instruction::Metadata {
                        out: self.compile_variable(out),
                        info_offset: self.compile_variable(offset.into()),
                    }
                }
                cube::VariableKind::GlobalOutputArray(id) => {
                    let id = self.num_inputs as u32 + id as u32;
                    let offset = self.metadata.buffer_len_index(id);
                    wgsl::Instruction::Metadata {
                        out: self.compile_variable(out),
                        info_offset: self.compile_variable(offset.into()),
                    }
                }
                _ => wgsl::Instruction::Length {
                    var: self.compile_variable(var),
                    out: self.compile_variable(out),
                },
            },
        }
    }

    fn compile_instruction(
        &mut self,
        value: cube::Operator,
        out: Option<cube::Variable>,
    ) -> wgsl::Instruction {
        let out = out.unwrap();
        match value {
            cube::Operator::Max(op) => wgsl::Instruction::Max {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Min(op) => wgsl::Instruction::Min {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Add(op) => wgsl::Instruction::Add {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Fma(op) => wgsl::Instruction::Fma {
                a: self.compile_variable(op.a),
                b: self.compile_variable(op.b),
                c: self.compile_variable(op.c),
                out: self.compile_variable(out),
            },
            cube::Operator::Index(op) => wgsl::Instruction::Index {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::UncheckedIndex(op) => wgsl::Instruction::Index {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Modulo(op) => wgsl::Instruction::Modulo {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Sub(op) => wgsl::Instruction::Sub {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Mul(op) => wgsl::Instruction::Mul {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Div(op) => wgsl::Instruction::Div {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Abs(op) => wgsl::Instruction::Abs {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Exp(op) => wgsl::Instruction::Exp {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Log(op) => wgsl::Instruction::Log {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Log1p(op) => wgsl::Instruction::Log1p {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Cos(op) => wgsl::Instruction::Cos {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Sin(op) => wgsl::Instruction::Sin {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Tanh(op) => wgsl::Instruction::Tanh {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Powf(op) => wgsl::Instruction::Powf {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Sqrt(op) => wgsl::Instruction::Sqrt {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Round(op) => wgsl::Instruction::Round {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Floor(op) => wgsl::Instruction::Floor {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Ceil(op) => wgsl::Instruction::Ceil {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Erf(op) => wgsl::Instruction::Erf {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Recip(op) => wgsl::Instruction::Recip {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Equal(op) => wgsl::Instruction::Equal {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Lower(op) => wgsl::Instruction::Lower {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Clamp(op) => wgsl::Instruction::Clamp {
                input: self.compile_variable(op.input),
                min_value: self.compile_variable(op.min_value),
                max_value: self.compile_variable(op.max_value),
                out: self.compile_variable(out),
            },
            cube::Operator::Greater(op) => wgsl::Instruction::Greater {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::LowerEqual(op) => wgsl::Instruction::LowerEqual {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::GreaterEqual(op) => wgsl::Instruction::GreaterEqual {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::NotEqual(op) => wgsl::Instruction::NotEqual {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Cast(op) => wgsl::Instruction::Assign {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::IndexAssign(op) => wgsl::Instruction::IndexAssign {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::UncheckedIndexAssign(op) => wgsl::Instruction::IndexAssign {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::And(op) => wgsl::Instruction::And {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Or(op) => wgsl::Instruction::Or {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Not(op) => wgsl::Instruction::Not {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::BitwiseOr(op) => wgsl::Instruction::BitwiseOr {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::BitwiseAnd(op) => wgsl::Instruction::BitwiseAnd {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::BitwiseXor(op) => wgsl::Instruction::BitwiseXor {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::ShiftLeft(op) => wgsl::Instruction::ShiftLeft {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::ShiftRight(op) => wgsl::Instruction::ShiftRight {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Remainder(op) => wgsl::Instruction::Remainder {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::Slice(op) => wgsl::Instruction::Slice {
                input: self.compile_variable(op.input),
                start: self.compile_variable(op.start),
                end: self.compile_variable(op.end),
                out: self.compile_variable(out),
            },

            cube::Operator::Bitcast(op) => wgsl::Instruction::Bitcast {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },

            cube::Operator::Neg(op) => wgsl::Instruction::Negate {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Magnitude(op) => wgsl::Instruction::Magnitude {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Normalize(op) => wgsl::Instruction::Normalize {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::Operator::Dot(op) => wgsl::Instruction::Dot {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::Operator::InitLine(op) => wgsl::Instruction::VecInit {
                inputs: op
                    .inputs
                    .into_iter()
                    .map(|var| self.compile_variable(var))
                    .collect(),
                out: self.compile_variable(out),
            },
            cube::Operator::CopyMemory(op) => wgsl::Instruction::Copy {
                input: self.compile_variable(op.input),
                in_index: self.compile_variable(op.in_index),
                out: self.compile_variable(out),
                out_index: self.compile_variable(op.out_index),
            },
            cube::Operator::CopyMemoryBulk(op) => wgsl::Instruction::CopyBulk {
                input: self.compile_variable(op.input),
                in_index: self.compile_variable(op.in_index),
                out: self.compile_variable(out),
                out_index: self.compile_variable(op.out_index),
                len: op.len,
            },
            cube::Operator::Select(op) => wgsl::Instruction::Select {
                cond: self.compile_variable(op.cond),
                then: self.compile_variable(op.then),
                or_else: self.compile_variable(op.or_else),
                out: self.compile_variable(out),
            },
        }
    }

    fn compile_atomic(
        &mut self,
        atomic: cube::AtomicOp,
        out: Option<cube::Variable>,
    ) -> wgsl::Instruction {
        let out = out.unwrap();
        match atomic {
            cube::AtomicOp::Add(op) => wgsl::Instruction::AtomicAdd {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::AtomicOp::Sub(op) => wgsl::Instruction::AtomicSub {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::AtomicOp::Max(op) => wgsl::Instruction::AtomicMax {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::AtomicOp::Min(op) => wgsl::Instruction::AtomicMin {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::AtomicOp::And(op) => wgsl::Instruction::AtomicAnd {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::AtomicOp::Or(op) => wgsl::Instruction::AtomicOr {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::AtomicOp::Xor(op) => wgsl::Instruction::AtomicXor {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::AtomicOp::Load(op) => wgsl::Instruction::AtomicLoad {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::AtomicOp::Store(op) => wgsl::Instruction::AtomicStore {
                input: self.compile_variable(op.input),
                out: self.compile_variable(out),
            },
            cube::AtomicOp::Swap(op) => wgsl::Instruction::AtomicSwap {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(out),
            },
            cube::AtomicOp::CompareAndSwap(op) => wgsl::Instruction::AtomicCompareExchangeWeak {
                lhs: self.compile_variable(op.input),
                cmp: self.compile_variable(op.cmp),
                value: self.compile_variable(op.val),
                out: self.compile_variable(out),
            },
        }
    }

    fn compile_location(value: cube::Location) -> wgsl::Location {
        match value {
            cube::Location::Storage => wgsl::Location::Storage,
            cube::Location::Cube => wgsl::Location::Workgroup,
        }
    }

    fn compile_visibility(value: cube::Visibility) -> wgsl::Visibility {
        match value {
            cube::Visibility::Read => wgsl::Visibility::Read,
            cube::Visibility::ReadWrite => wgsl::Visibility::ReadWrite,
        }
    }

    fn compile_binding(value: cube::Binding) -> wgsl::Binding {
        wgsl::Binding {
            visibility: Self::compile_visibility(value.visibility),
            location: Self::compile_location(value.location),
            item: Self::compile_item(value.item),
            size: value.size,
        }
    }
}

fn register_extensions(instructions: &[wgsl::Instruction]) -> Vec<wgsl::Extension> {
    let mut extensions = Vec::new();

    let mut register_extension = |extension: wgsl::Extension| {
        if !extensions.contains(&extension) {
            extensions.push(extension);
        }
    };

    // Since not all instructions are native to WGSL, we need to add the custom ones.
    for instruction in instructions {
        match instruction {
            wgsl::Instruction::Powf { lhs: _, rhs, out } => {
                register_extension(wgsl::Extension::PowfPrimitive(out.item()));

                if rhs.is_always_scalar() || rhs.item().vectorization_factor() == 1 {
                    register_extension(wgsl::Extension::PowfScalar(out.item()));
                } else {
                    register_extension(wgsl::Extension::Powf(out.item()));
                }
            }
            wgsl::Instruction::Erf { input, out: _ } => {
                register_extension(wgsl::Extension::Erf(input.item()));
            }
            #[cfg(target_os = "macos")]
            wgsl::Instruction::Tanh { input, out: _ } => {
                register_extension(wgsl::Extension::SafeTanh(input.item()))
            }
            wgsl::Instruction::If { instructions, .. } => {
                for extension in register_extensions(instructions) {
                    register_extension(extension);
                }
            }
            wgsl::Instruction::IfElse {
                instructions_if,
                instructions_else,
                ..
            } => {
                for extension in register_extensions(instructions_if) {
                    register_extension(extension);
                }
                for extension in register_extensions(instructions_else) {
                    register_extension(extension);
                }
            }
            wgsl::Instruction::Loop { instructions } => {
                for extension in register_extensions(instructions) {
                    register_extension(extension);
                }
            }
            wgsl::Instruction::RangeLoop { instructions, .. } => {
                for extension in register_extensions(instructions) {
                    register_extension(extension);
                }
            }
            _ => {}
        }
    }

    extensions
}
