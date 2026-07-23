use anyhow::Result;
use candle_core::Device;

/// Picks the best available compute device: CUDA, then Metal, then CPU.
///
/// `Device::cuda_if_available` / `metal_if_available` silently fall back to
/// `Cpu` when the corresponding cargo feature wasn't compiled in, so this is
/// safe to call unconditionally on every platform/build.
pub fn select() -> Result<Device> {
    let cuda = Device::cuda_if_available(0)?;
    if cuda.is_cuda() {
        println!("gpu backend: cuda");
        return Ok(cuda);
    }

    let metal = Device::metal_if_available(0)?;
    if metal.is_metal() {
        println!("gpu backend: metal");
        return Ok(metal);
    }

    println!("gpu backend: none, using cpu");
    Ok(Device::Cpu)
}
