use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::VmFd;
use vm_memory::{Address, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

pub fn setup_guest_memory(vm: &VmFd, size_mb: usize) -> Result<GuestMemoryMmap, Box<dyn std::error::Error>> {
    let mem_size = size_mb * 1024 * 1024;
    let guest_memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_size)])?;

    // Register each region with KVM
    for (index, region) in guest_memory.iter().enumerate() {
        let mem_region = kvm_userspace_memory_region {
            slot: index as u32,
            flags: 0,
            guest_phys_addr: region.start_addr().raw_value(),
            memory_size: region.len(),
            userspace_addr: guest_memory.get_host_address(region.start_addr())? as u64,
        };

        unsafe {
            vm.set_user_memory_region(mem_region)?;
        }
    }

    Ok(guest_memory)
}
