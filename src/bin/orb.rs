#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[cfg(not(feature = "dhat-heap"))]
#[global_allocator]
static ALLOC: orbital_core::alloc_meter::CountingAlloc<std::alloc::System> =
    orbital_core::alloc_meter::CountingAlloc::thread_only(std::alloc::System);

fn main() -> anyhow::Result<()> {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::builder().trim_backtraces(None).build();
    #[cfg(not(feature = "dhat-heap"))]
    orbital_core::alloc_meter::declare_installed();
    orbital_cli::run()
}
