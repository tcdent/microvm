use std::os::fd::AsRawFd;

use kvm_bindings::{kvm_pit_config, kvm_reinject_control};
use kvm_ioctls::{Cap, Kvm, VmFd};

/// `_IO(KVMIO, 0x71)`; kvm-ioctls has no wrapper.
const KVM_REINJECT_CONTROL: libc::c_ulong = 0xAE71;

pub fn create_vm() -> Result<(Kvm, VmFd), Box<dyn std::error::Error>> {
    let kvm = Kvm::new()?;

    assert!(kvm.check_extension(Cap::UserMemory));
    assert!(kvm.check_extension(Cap::Irqchip));
    assert!(kvm.check_extension(Cap::SetTssAddr));

    let vm = kvm.create_vm()?;

    // x86_64: TSS address, in-kernel irqchip, and PIT
    vm.set_tss_address(0xFFFB_D000)?;
    vm.create_irq_chip()?;
    vm.create_pit2(kvm_pit_config::default())?;
    // Reinjecting missed PIT ticks only helps a guest that keeps time by counting them, and
    // ours keep it with kvm-clock and the local APIC timer. KVM also turns AVIC off for as
    // long as reinjection is on.
    let ctl = kvm_reinject_control { pit_reinject: 0, ..Default::default() };
    if unsafe { libc::ioctl(vm.as_raw_fd(), KVM_REINJECT_CONTROL, &ctl) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    Ok((kvm, vm))
}
