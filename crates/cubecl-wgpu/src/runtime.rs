use std::marker::PhantomData;
#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
use std::{cell::RefCell, rc::Rc};

use crate::{
    compiler::{base::WgpuCompiler, wgsl::WgslCompiler},
    compute::{WgpuServer, WgpuStorage},
    AutoGraphicsApi, GraphicsApi, Pdrc, WgpuDevice,
};
#[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
use cubecl_core::future;
#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
use cubecl_core::{channel::ComputeChannel, server::ComputeServer};
use cubecl_core::{Feature, Runtime};
#[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
use cubecl_runtime::channel::MutexComputeChannel;
use cubecl_runtime::{
    client::ComputeClient,
    memory_management::{MemoryConfiguration, MemoryDeviceProperties, MemoryManagement},
    storage::ComputeStorage,
    ComputeRuntime, DeviceProperties,
};
use wgpu::RequestAdapterOptions;

/// Runtime that uses the [wgpu] crate with the wgsl compiler. This is used in the Wgpu backend.
/// For advanced configuration, use [`init_sync`] to pass in runtime options or to select a
/// specific graphics API.
#[derive(Debug)]
pub struct WgpuRuntime<C: WgpuCompiler = WgslCompiler>(PhantomData<C>);

type Server = WgpuServer<WgslCompiler>;

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
thread_local! {
    static LOCAL_DEVICE: RefCell<hashbrown::HashMap<WgpuDevice, Rc<RefCell<Server>>>> = RefCell::new(hashbrown::HashMap::default());
}

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
static RUNTIME: ComputeRuntime<WgpuDevice, Server, ThreadLocalChannel> = ComputeRuntime::new();

/// The compute instance is shared across all [wgpu runtimes](WgpuRuntime).
#[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
static RUNTIME: ComputeRuntime<WgpuDevice, Server, MutexComputeChannel<Server>> =
    ComputeRuntime::new();

impl Runtime for WgpuRuntime<WgslCompiler> {
    type Compiler = WgslCompiler;
    type Server = Server;

    #[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
    type Channel = MutexComputeChannel<Server>;
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    type Channel = ThreadLocalChannel;
    type Device = WgpuDevice;

    fn client(device: &Self::Device) -> ComputeClient<Self::Server, Self::Channel> {
        RUNTIME.client(device, move || {
            #[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
            {
                let setup = future::block_on(create_setup_for_device::<
                    AutoGraphicsApi,
                    WgslCompiler,
                >(device));
                create_client_on_setup(setup, RuntimeOptions::default())
            }

            #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
            {
                let server = LOCAL_DEVICE.with_borrow_mut(|runtime| {
                    runtime
                        .get(device)
                        .expect(&format!("The wgpu server for {device:?} was not initialized with `init_thread_server`. `init_thread_server` needs to be called once on each thread before any computation is done on that thread"))
                        .clone()
                });
                let server = server.borrow();

                let limits = server.device.limits();
                let mem_props = MemoryDeviceProperties {
                    max_page_size: limits.max_storage_buffer_binding_size as u64,
                    alignment: WgpuStorage::ALIGNMENT
                        .max(limits.min_storage_buffer_offset_alignment as u64),
                };

                let features = server.device.features();
                let mut device_props = DeviceProperties::new(&[], mem_props);

                if features.contains(wgpu::Features::SUBGROUP)
                    && server.adapter.get_info().device_type != wgpu::DeviceType::Cpu
                {
                    device_props.register_feature(Feature::Subcube);
                }
                <WgslCompiler as WgpuCompiler>::register_features(
                    &server.adapter,
                    &server.device,
                    &mut device_props,
                );

                ComputeClient::new(
                    ThreadLocalChannel {
                        device: device.clone(),
                    },
                    device_props,
                )
            }
        })
    }

    fn name() -> &'static str {
        "wgpu<wgsl>"
    }

    fn supported_line_sizes() -> &'static [u8] {
        &[4, 2]
    }
}

/// The values that control how a WGPU Runtime will perform its calculations.
pub struct RuntimeOptions {
    /// Control the amount of compute tasks to be aggregated into a single GPU command.
    pub tasks_max: usize,
    /// Configures the memory management.
    pub memory_config: MemoryConfiguration,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        #[cfg(test)]
        const DEFAULT_MAX_TASKS: usize = 1;
        #[cfg(not(test))]
        const DEFAULT_MAX_TASKS: usize = 16;

        let tasks_max = match std::env::var("CUBECL_WGPU_MAX_TASKS") {
            Ok(value) => value
                .parse::<usize>()
                .expect("CUBECL_WGPU_MAX_TASKS should be a positive integer."),
            Err(_) => DEFAULT_MAX_TASKS,
        };

        Self {
            tasks_max,
            memory_config: MemoryConfiguration::default(),
        }
    }
}

/// A complete setup used to run wgpu.
///
/// These can either be created with [`init_setup`] or [`init_setup_async`].
#[derive(Clone, Debug)]
pub struct WgpuSetup {
    /// The underlying wgpu instance.
    pub instance: Pdrc<wgpu::Instance>,
    /// The selected 'adapter'. This corresponds to a physical device.
    pub adapter: Pdrc<wgpu::Adapter>,
    /// The wgpu device Burn will use. Nb: There can only be one device per adapter.
    pub device: Pdrc<wgpu::Device>,
    /// The queue Burn commands will be submitted to.
    pub queue: Pdrc<wgpu::Queue>,
}

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
pub async fn init_thread_server(device: WgpuDevice, options: RuntimeOptions) {
    if !LOCAL_DEVICE.with_borrow(|map| map.contains_key(&device)) {
        let setup = create_setup_for_device::<AutoGraphicsApi, WgslCompiler>(&device).await;

        let limits = setup.device.limits();
        let mem_props = MemoryDeviceProperties {
            max_page_size: limits.max_storage_buffer_binding_size as u64,
            alignment: WgpuStorage::ALIGNMENT
                .max(limits.min_storage_buffer_offset_alignment as u64),
        };
        let memory_management = {
            let mem_props = mem_props.clone();
            let config = options.memory_config;
            let storage = WgpuStorage::new(setup.device.clone());
            MemoryManagement::from_configuration(storage, mem_props, config)
        };
        let server = crate::compute::WgpuServer::new(
            memory_management,
            setup.device,
            setup.queue,
            setup.adapter,
            options.tasks_max,
        );

        LOCAL_DEVICE.with_borrow_mut(|map| map.insert(device, Rc::new(RefCell::new(server))));
    }
}

/// Create a [`WgpuDevice`] on an existing [`WgpuSetup`].
/// Useful when you want to share a device between CubeCL and other wgpu-dependent libraries.
#[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
pub fn init_device(setup: WgpuSetup, options: RuntimeOptions) -> WgpuDevice {
    let device_id = WgpuDevice::Existing(setup.device.as_ref().global_id());
    let client = create_client_on_setup(setup, options);
    RUNTIME.register(&device_id, client);
    device_id
}

/// Like [`init_setup_async`], but synchronous.
/// On wasm, it is necessary to use [`init_setup_async`] instead.
#[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
pub fn init_setup<G: GraphicsApi>(device: &WgpuDevice, options: RuntimeOptions) -> WgpuSetup {
    cfg_if::cfg_if! {
        if #[cfg(target_family = "wasm")] {
            let _ = (device, options);
            panic!("Creating a wgpu setup synchronously is unsupported on wasm. Use init_async instead");
        } else {
            future::block_on(init_setup_async::<G>(device, options))
        }
    }
}

/// Initialize a client on the given device with the given options.
/// This function is useful to configure the runtime options
/// or to pick a different graphics API.
#[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
pub async fn init_setup_async<G: GraphicsApi>(
    device: &WgpuDevice,
    options: RuntimeOptions,
) -> WgpuSetup {
    let setup = create_setup_for_device::<G, WgslCompiler>(device).await;
    let return_setup = setup.clone();
    let client = create_client_on_setup(setup, options);
    RUNTIME.register(device, client);
    return_setup
}

#[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
pub(crate) fn create_client_on_setup<C: WgpuCompiler>(
    setup: WgpuSetup,
    options: RuntimeOptions,
) -> ComputeClient<WgpuServer<C>, MutexComputeChannel<WgpuServer<C>>> {
    let limits = setup.device.limits();
    let mem_props = MemoryDeviceProperties {
        max_page_size: limits.max_storage_buffer_binding_size as u64,
        alignment: WgpuStorage::ALIGNMENT.max(limits.min_storage_buffer_offset_alignment as u64),
    };

    let memory_management = {
        let device = setup.device.clone();
        let mem_props = mem_props.clone();
        let config = options.memory_config;
        let storage = WgpuStorage::new(device.clone());
        MemoryManagement::from_configuration(storage, mem_props, config)
    };
    let server = WgpuServer::new(
        memory_management,
        setup.device.clone(),
        setup.queue,
        setup.adapter.clone(),
        options.tasks_max,
    );
    let channel = MutexComputeChannel::new(server);

    let features = setup.adapter.features();
    let mut device_props = DeviceProperties::new(&[], mem_props);

    if features.contains(wgpu::Features::SUBGROUP)
        && setup.adapter.get_info().device_type != wgpu::DeviceType::Cpu
    {
        device_props.register_feature(Feature::Subcube);
    }
    C::register_features(&setup.adapter, &setup.device, &mut device_props);
    ComputeClient::new(channel, device_props)
}

/// Select the wgpu device and queue based on the provided [device](WgpuDevice).
pub(crate) async fn create_setup_for_device<G: GraphicsApi, C: WgpuCompiler>(
    device: &WgpuDevice,
) -> WgpuSetup {
    let (instance, adapter) = request_adapter::<G>(device).await;
    let (device, queue) = C::request_device(&adapter).await;

    log::info!(
        "Created wgpu compute server on device {:?} => {:?}",
        device,
        adapter.get_info()
    );

    WgpuSetup {
        instance: Pdrc::new(instance),
        adapter: Pdrc::new(adapter),
        device: Pdrc::new(device),
        queue: Pdrc::new(queue),
    }
}

async fn request_adapter<G: GraphicsApi>(device: &WgpuDevice) -> (wgpu::Instance, wgpu::Adapter) {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: G::backend().into(),
        ..Default::default()
    });

    #[allow(deprecated)]
    let override_device = if matches!(
        device,
        WgpuDevice::DefaultDevice | WgpuDevice::BestAvailable
    ) {
        get_device_override()
    } else {
        None
    };

    let device = override_device.unwrap_or_else(|| device.clone());

    let adapter = match device {
        #[cfg(not(target_family = "wasm"))]
        WgpuDevice::DiscreteGpu(num) => {
            select_from_adapter_list::<G>(num, "No Discrete GPU device found", &instance, &device)
        }
        #[cfg(not(target_family = "wasm"))]
        WgpuDevice::IntegratedGpu(num) => {
            select_from_adapter_list::<G>(num, "No Integrated GPU device found", &instance, &device)
        }
        #[cfg(not(target_family = "wasm"))]
        WgpuDevice::VirtualGpu(num) => {
            select_from_adapter_list::<G>(num, "No Virtual GPU device found", &instance, &device)
        }
        #[cfg(not(target_family = "wasm"))]
        WgpuDevice::Cpu => {
            select_from_adapter_list::<G>(0, "No CPU device found", &instance, &device)
        }
        WgpuDevice::Existing(_) => {
            unreachable!("Cannot select an adapter for an existing device.")
        }
        _ => instance
            .request_adapter(&RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
            })
            .await
            .expect("No possible adapter available for backend. Falling back to first available."),
    };

    log::info!("Using adapter {:?}", adapter.get_info());

    (instance, adapter)
}

#[cfg(not(target_family = "wasm"))]
fn select_from_adapter_list<G: GraphicsApi>(
    num: usize,
    error: &str,
    instance: &wgpu::Instance,
    device: &WgpuDevice,
) -> wgpu::Adapter {
    let mut adapters_other = Vec::new();
    let mut adapters = Vec::new();

    instance
        .enumerate_adapters(G::backend().into())
        .into_iter()
        .for_each(|adapter| {
            let device_type = adapter.get_info().device_type;

            if let wgpu::DeviceType::Other = device_type {
                adapters_other.push(adapter);
                return;
            }

            let is_same_type = match device {
                WgpuDevice::DiscreteGpu(_) => device_type == wgpu::DeviceType::DiscreteGpu,
                WgpuDevice::IntegratedGpu(_) => device_type == wgpu::DeviceType::IntegratedGpu,
                WgpuDevice::VirtualGpu(_) => device_type == wgpu::DeviceType::VirtualGpu,
                WgpuDevice::Cpu => device_type == wgpu::DeviceType::Cpu,
                #[allow(deprecated)]
                WgpuDevice::DefaultDevice | WgpuDevice::BestAvailable => true,
                WgpuDevice::Existing(_) => {
                    unreachable!("Cannot select an adapter for an existing device.")
                }
            };

            if is_same_type {
                adapters.push(adapter);
            }
        });

    if adapters.len() <= num {
        if adapters_other.len() <= num {
            panic!(
                "{}, adapters {:?}, other adapters {:?}",
                error,
                adapters
                    .into_iter()
                    .map(|adapter| adapter.get_info())
                    .collect::<Vec<_>>(),
                adapters_other
                    .into_iter()
                    .map(|adapter| adapter.get_info())
                    .collect::<Vec<_>>(),
            );
        }

        return adapters_other.remove(num);
    }

    adapters.remove(num)
}

fn get_device_override() -> Option<WgpuDevice> {
    // If BestAvailable, check if we should instead construct as
    // if a specific device was specified.
    std::env::var("CUBECL_WGPU_DEFAULT_DEVICE")
        .ok()
        .and_then(|var| {
            let override_device = if let Some(inner) = var.strip_prefix("DiscreteGpu(") {
                inner
                    .strip_suffix(")")
                    .and_then(|s| s.parse().ok())
                    .map(WgpuDevice::DiscreteGpu)
            } else if let Some(inner) = var.strip_prefix("IntegratedGpu(") {
                inner
                    .strip_suffix(")")
                    .and_then(|s| s.parse().ok())
                    .map(WgpuDevice::IntegratedGpu)
            } else if let Some(inner) = var.strip_prefix("VirtualGpu(") {
                inner
                    .strip_suffix(")")
                    .and_then(|s| s.parse().ok())
                    .map(WgpuDevice::VirtualGpu)
            } else if var == "Cpu" {
                Some(WgpuDevice::Cpu)
            } else {
                None
            };

            if override_device.is_none() {
                log::warn!("Unknown CUBECL_WGPU_DEVICE override {var}");
            }
            override_device
        })
}

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
#[derive(Debug, Clone)]
pub struct ThreadLocalChannel {
    device: WgpuDevice,
}

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
impl ThreadLocalChannel {
    fn make_server(device: &WgpuDevice) -> Rc<RefCell<Server>> {
        let setup = futures_lite::future::block_on(create_setup_for_device::<
            AutoGraphicsApi,
            WgslCompiler,
        >(device));

        let limits = setup.device.limits();
        let mem_props = MemoryDeviceProperties {
            max_page_size: limits.max_storage_buffer_binding_size as u64,
            alignment: WgpuStorage::ALIGNMENT
                .max(limits.min_storage_buffer_offset_alignment as u64),
        };

        let options = RuntimeOptions::default();
        let memory_management = {
            let mem_props = mem_props.clone();
            let config = options.memory_config;
            let storage = WgpuStorage::new(setup.device.clone());
            MemoryManagement::from_configuration(storage, mem_props, config)
        };
        let server = crate::compute::WgpuServer::new(
            memory_management,
            setup.device,
            setup.queue,
            setup.adapter,
            options.tasks_max,
        );

        Rc::new(RefCell::new(server))
    }
}

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
impl ComputeChannel<Server> for ThreadLocalChannel {
    fn read(
        &self,
        binding: cubecl_core::server::Binding,
    ) -> impl std::future::Future<Output = Vec<u8>> {
        LOCAL_DEVICE.with_borrow_mut(|runtime| {
            let server = runtime
                .entry(self.device.clone())
                .or_insert_with(|| Self::make_server(&self.device))
                .clone();
            async move { server.borrow_mut().read(binding).await }
        })
    }

    fn get_resource(
        &self,
        binding: cubecl_core::server::Binding,
    ) -> cubecl_runtime::storage::BindingResource<Server> {
        LOCAL_DEVICE.with_borrow_mut(|runtime| {
            let server = runtime
                .get(&self.device)
                .expect(&format!(
                    "The wgpu server for {:?} was not initialized with `init_thread_server`. `init_thread_server` needs to be called once on each thread before any computation is done on that thread",
                    self.device,
                ));
            server.borrow_mut().get_resource(binding)
        })
    }

    fn create(&self, data: &[u8]) -> cubecl_core::server::Handle {
        LOCAL_DEVICE.with_borrow_mut(|runtime| {
            let server = runtime
                .get(&self.device)
                .expect(&format!(
                    "The wgpu server for {:?} was not initialized with `init_thread_server`. `init_thread_server` needs to be called once on each thread before any computation is done on that thread",
                    self.device,
                ));
            server.borrow_mut().create(data)
        })
    }

    fn empty(&self, size: usize) -> cubecl_core::server::Handle {
        LOCAL_DEVICE.with_borrow_mut(|runtime| {
            let server = runtime
                .get(&self.device)
                .expect(&format!(
                    "The wgpu server for {:?} was not initialized with `init_thread_server`. `init_thread_server` needs to be called once on each thread before any computation is done on that thread",
                    self.device,
                ));
            server.borrow_mut().empty(size)
        })
    }

    unsafe fn execute(
        &self,
        kernel: <Server as ComputeServer>::Kernel,
        count: cubecl_core::CubeCount,
        bindings: Vec<cubecl_core::server::Binding>,
        mode: cubecl_core::ExecutionMode,
    ) {
        LOCAL_DEVICE.with_borrow_mut(|runtime| {
            let server = runtime
                .get(&self.device)
                .expect(&format!(
                    "The wgpu server for {:?} was not initialized with `init_thread_server`. `init_thread_server` needs to be called once on each thread before any computation is done on that thread",
                    self.device,
                ));
            unsafe { server.borrow_mut().execute(kernel, count, bindings, mode) }
        })
    }

    fn flush(&self) {
        LOCAL_DEVICE.with_borrow_mut(|runtime| {
            let server = runtime
                .get(&self.device)
                .expect(&format!(
                    "The wgpu server for {:?} was not initialized with `init_thread_server`. `init_thread_server` needs to be called once on each thread before any computation is done on that thread",
                    self.device,
                ));
            server.borrow_mut().flush()
        })
    }

    fn sync(&self) -> impl std::future::Future<Output = ()> {
        LOCAL_DEVICE.with_borrow_mut(|runtime| {
            let server = runtime
                .get(&self.device)
                .expect(&format!(
                    "The wgpu server for {:?} was not initialized with `init_thread_server`. `init_thread_server` needs to be called once on each thread before any computation is done on that thread",
                    self.device,
                ))
                .clone();
            async move { server.borrow_mut().sync().await }
        })
    }

    fn sync_elapsed(&self) -> impl std::future::Future<Output = cubecl_runtime::TimestampsResult> {
        LOCAL_DEVICE.with_borrow_mut(|runtime| {
            let server = runtime
                .get(&self.device)
                .expect(&format!(
                    "The wgpu server for {:?} was not initialized with `init_thread_server`. `init_thread_server` needs to be called once on each thread before any computation is done on that thread",
                    self.device,
                ))
                .clone();
            async move { server.borrow_mut().sync_elapsed().await }
        })
    }

    fn memory_usage(&self) -> cubecl_runtime::memory_management::MemoryUsage {
        LOCAL_DEVICE.with_borrow_mut(|runtime| {
            let server = runtime
                .get(&self.device)
                .expect(&format!(
                    "The wgpu server for {:?} was not initialized with `init_thread_server`. `init_thread_server` needs to be called once on each thread before any computation is done on that thread",
                    self.device,
                ));
            server.borrow_mut().memory_usage()
        })
    }

    fn enable_timestamps(&self) {
        LOCAL_DEVICE.with_borrow_mut(|runtime| {
            let server = runtime
                .get(&self.device)
                .expect(&format!(
                    "The wgpu server for {:?} was not initialized with `init_thread_server`. `init_thread_server` needs to be called once on each thread before any computation is done on that thread",
                    self.device,
                ));
            server.borrow_mut().enable_timestamps()
        })
    }

    fn disable_timestamps(&self) {
        LOCAL_DEVICE.with_borrow_mut(|runtime| {
            let server = runtime
                .get(&self.device)
                .expect(&format!(
                    "The wgpu server for {:?} was not initialized with `init_thread_server`. `init_thread_server` needs to be called once on each thread before any computation is done on that thread",
                    self.device,
                ));
            server.borrow_mut().disable_timestamps()
        })
    }
}
