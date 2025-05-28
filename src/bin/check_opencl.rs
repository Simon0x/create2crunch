// Check OpenCL setup and devices
use ocl::{Platform, Device, DeviceType};

fn main() -> ocl::Result<()> {
    println!("=== OpenCL Platform and Device Information ===");
    
    // Get all platforms
    let platforms = Platform::list();
    println!("Found {} OpenCL platform(s):", platforms.len());
    
    for (i, platform) in platforms.iter().enumerate() {
        println!("\nPlatform {}: {}", i, platform.name()?);
        println!("  Vendor: {}", platform.vendor()?);
        println!("  Version: {}", platform.version()?);
        
        // Get devices for this platform
        let devices = Device::list(*platform, Some(DeviceType::new().gpu()))?;
        println!("  GPU Devices: {}", devices.len());
        
        for (j, device) in devices.iter().enumerate() {
            println!("    Device {}: {}", j, device.name()?);
            println!("      Max Work Group Size: {}", device.max_wg_size()?);
            println!("      OpenCL Version: {}", device.version()?);
        }
        
        // Also check CPU devices
        let cpu_devices = Device::list(*platform, Some(DeviceType::new().cpu()))?;
        if !cpu_devices.is_empty() {
            println!("  CPU Devices: {}", cpu_devices.len());
            for (j, device) in cpu_devices.iter().enumerate() {
                println!("    CPU Device {}: {}", j, device.name()?);
            }
        }
    }
    
    // Test default platform and device
    println!("\n=== Default Platform/Device ===");
    let default_platform = Platform::new(ocl::core::default_platform()?);
    println!("Default Platform: {}", default_platform.name()?);
    
    // Try to get device 0
    match Device::by_idx_wrap(default_platform, 0) {
        Ok(device) => {
            println!("Device 0: {}", device.name()?);
            println!("  Max Work Group Size: {}", device.max_wg_size()?);
        }
        Err(e) => println!("Error getting device 0: {}", e),
    }
    
    Ok(())
}