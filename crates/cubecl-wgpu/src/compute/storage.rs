use cubecl_runtime::storage::{ComputeStorage, StorageHandle, StorageId};
use hashbrown::HashMap;
use std::num::NonZeroU64;

#[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
mod _impl {
    use std::sync::Arc;

    use cubecl_runtime::storage::StorageUtilization;

    use super::*;

    /// Buffer storage for wgpu.
    pub struct WgpuStorage {
        memory: HashMap<StorageId, Arc<wgpu::Buffer>>,
        deallocations: Vec<StorageId>,
        device: Arc<wgpu::Device>,
    }

    impl core::fmt::Debug for WgpuStorage {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(format!("WgpuStorage {{ device: {:?} }}", self.device).as_str())
        }
    }

    /// Keeps actual wgpu buffer references in a hashmap with ids as key.
    impl WgpuStorage {
        /// Create a new storage on the given [device](wgpu::Device).
        pub fn new(device: Arc<wgpu::Device>) -> Self {
            Self {
                memory: HashMap::new(),
                deallocations: Vec::new(),
                device,
            }
        }

        /// Actually deallocates buffers tagged to be deallocated.
        pub fn perform_deallocations(&mut self) {
            for id in self.deallocations.drain(..) {
                if let Some(buffer) = self.memory.remove(&id) {
                    buffer.destroy()
                }
            }
        }
    }

    impl ComputeStorage for WgpuStorage {
        type Resource = WgpuResource;

        // 32 bytes is enough to handle a double4 worth of alignment.
        // See: https://github.com/gfx-rs/wgpu/issues/3508
        // NB: cudamalloc and co. actually align to _256_ bytes. Worth
        // trying this in the future to see if it reduces memory coalescing.
        const ALIGNMENT: u64 = 32;

        fn get(&mut self, handle: &StorageHandle) -> Self::Resource {
            let buffer = self.memory.get(&handle.id).unwrap();
            WgpuResource::new(buffer.clone(), handle.offset(), handle.size())
        }

        fn alloc(&mut self, size: u64) -> StorageHandle {
            let id = StorageId::new();
            let buffer = Arc::new(self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size,
                usage: wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_SRC
                    | wgpu::BufferUsages::INDIRECT,
                mapped_at_creation: false,
            }));

            self.memory.insert(id, buffer);
            StorageHandle::new(id, StorageUtilization { offset: 0, size })
        }

        fn dealloc(&mut self, id: StorageId) {
            self.deallocations.push(id);
        }
    }

    /// The memory resource that can be allocated for wgpu.
    #[derive(new)]
    pub struct WgpuResource {
        /// The wgpu buffer.
        pub buffer: Arc<wgpu::Buffer>,

        offset: u64,
        size: u64,
    }

    impl WgpuResource {
        /// Return the binding view of the buffer.
        pub fn as_wgpu_bind_resource(&self) -> wgpu::BindingResource {
            let binding = wgpu::BufferBinding {
                buffer: &self.buffer,
                offset: self.offset,
                size: Some(
                    NonZeroU64::new(self.size).expect("0 size resources are not yet supported."),
                ),
            };
            wgpu::BindingResource::Buffer(binding)
        }

        /// Return the buffer size.
        pub fn size(&self) -> u64 {
            self.size
        }

        /// Return the buffer offset.
        pub fn offset(&self) -> u64 {
            self.offset
        }
    }
}

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
mod _impl {
    use std::rc::Rc;

    use crate::{compiler::base::WgpuCompiler, compute::server::ServerCommand};

    use super::*;

    /// Buffer storage for wgpu.
    pub struct WgpuStorage<C: WgpuCompiler> {
        deallocations: Vec<StorageId>,
        tx: std::sync::mpsc::Sender<ServerCommand<C>>,
    }

    /// Keeps actual wgpu buffer references in a hashmap with ids as key.
    impl<C: WgpuCompiler> WgpuStorage<C> {
        /// Create a new storage on the given [device](wgpu::Device).
        pub fn new(tx: std::sync::mpsc::Sender<ServerCommand<C>>) -> Self {
            Self {
                deallocations: Vec::new(),
                tx,
            }
        }

        /// Actually deallocates buffers tagged to be deallocated.
        pub fn perform_deallocations(&mut self) {
            let (tx, rx) = futures::channel::oneshot::channel();
            self.tx
                .send(ServerCommand::PerformDeallocations {
                    tx,
                    deallocations: self.deallocations.drain(..).collect(),
                })
                .expect("Failed to send command to the wgpu server");

            if futures::executor::block_on(rx).is_err() {
                panic!("Failed to receive the response from the WgpuServerInner")
            }
        }
    }

    impl<C: WgpuCompiler> ComputeStorage for WgpuStorage<C> {
        type Resource = WgpuResource;

        // 32 bytes is enough to handle a double4 worth of alignment.
        // See: https://github.com/gfx-rs/wgpu/issues/3508
        // NB: cudamalloc and co. actually align to _256_ bytes. Worth
        // trying this in the future to see if it reduces memory coalescing.
        const ALIGNMENT: u64 = 32;

        fn get(&mut self, handle: &StorageHandle) -> Self::Resource {
            WgpuResource::new(handle.id, handle.offset(), handle.size())
        }

        fn alloc(&mut self, size: u64) -> StorageHandle {
            let (tx, rx) = futures::channel::oneshot::channel();
            self.tx
                .send(ServerCommand::Alloc { tx, size })
                .expect("Failed to send command to the wgpu server");

            if let Ok(handle) = futures::executor::block_on(rx) {
                handle
            } else {
                panic!("Failed to receive the response from the WgpuServerInner")
            }
        }

        fn dealloc(&mut self, id: StorageId) {
            self.deallocations.push(id);
        }
    }

    /// The memory resource that can be allocated for wgpu.
    #[derive(new)]
    pub struct WgpuResource {
        /// The storage id.
        pub buffer: StorageId,
        offset: u64,
        size: u64,
    }

    impl WgpuResource {
        /// Return the binding view of the buffer.
        pub fn as_wgpu_bind_resource<'a>(
            &self,
            buffers: &'a HashMap<StorageId, Rc<wgpu::Buffer>>,
        ) -> wgpu::BindingResource<'a> {
            let buffer = buffers.get(&self.buffer).expect("Buffer does not exist");
            let binding = wgpu::BufferBinding {
                buffer,
                offset: self.offset,
                size: Some(
                    NonZeroU64::new(self.size).expect("0 size resources are not yet supported."),
                ),
            };
            wgpu::BindingResource::Buffer(binding)
        }

        /// Return the buffer size.
        pub fn size(&self) -> u64 {
            self.size
        }

        /// Return the buffer offset.
        pub fn offset(&self) -> u64 {
            self.offset
        }
    }
}

pub use _impl::*;
