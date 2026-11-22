use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::VmFd;
use vm_memory::{Address, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

/// Guest RAM stops here; MMIO devices, the sharedfs index and the APICs
/// live in the 3-4 GiB hole. RAM beyond 3 GiB continues at 4 GiB.
pub const LOW_RAM_END: u64 = 0xC000_0000;
pub const HIGH_RAM_START: u64 = 0x1_0000_0000;

/// RAM regions for `size` bytes as `(start, len)`, split around the hole.
pub fn ram_layout(size: u64) -> Vec<(u64, u64)> {
    if size <= LOW_RAM_END {
        vec![(0, size)]
    } else {
        vec![(0, LOW_RAM_END), (HIGH_RAM_START, size - LOW_RAM_END)]
    }
}

pub fn setup_guest_memory(vm: &VmFd, size_mb: usize) -> Result<GuestMemoryMmap, Box<dyn std::error::Error>> {
    let mem_size = size_mb * 1024 * 1024;
    let ranges: Vec<(GuestAddress, usize)> =
        ram_layout(mem_size as u64).into_iter().map(|(a, l)| (GuestAddress(a), l as usize)).collect();
    let guest_memory = GuestMemoryMmap::from_ranges(&ranges)?;

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
