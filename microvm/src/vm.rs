use kvm_bindings::kvm_pit_config;
use kvm_ioctls::{Cap, Kvm, VmFd};

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

    Ok((kvm, vm))
}
