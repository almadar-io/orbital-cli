#[global_allocator]
static ALLOC: orbital_core::alloc_meter::CountingAlloc<std::alloc::System> =
    orbital_core::alloc_meter::CountingAlloc::thread_only(std::alloc::System);

fn main() -> anyhow::Result<()> {
    orbital_core::alloc_meter::declare_installed();
    orbital_cli::run()
}
