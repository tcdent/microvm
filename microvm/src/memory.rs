use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::VmFd;
use vm_memory::{Address, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

pub const GUEST_MEM_SIZE: usize = 256 * 1024 * 1024; // 256MB

pub fn setup_guest_memory(vm: &VmFd) -> Result<GuestMemoryMmap, Box<dyn std::error::Error>> {
    let guest_memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), GUEST_MEM_SIZE)])?;

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
