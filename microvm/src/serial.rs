use std::io;

use kvm_ioctls::VmFd;
use vm_superio::serial::NoEvents;
use vm_superio::{Serial as VmSerial, Trigger};

pub const SERIAL_IRQ: u32 = 4;

/// IRQ trigger that injects an interrupt via KVM's irqchip
#[derive(Clone)]
pub struct IrqTrigger {
    vm: std::sync::Arc<VmFd>,
    irq: u32,
}

impl IrqTrigger {
    pub fn new(vm: std::sync::Arc<VmFd>, irq: u32) -> Self {
        Self { vm, irq }
    }
}

impl Trigger for IrqTrigger {
    type E = io::Error;

    fn trigger(&self) -> Result<(), Self::E> {
        self.vm.set_irq_line(self.irq, true)
            .map_err(|e| io::Error::other(format!("set_irq_line assert: {e}")))?;
        self.vm.set_irq_line(self.irq, false)
            .map_err(|e| io::Error::other(format!("set_irq_line deassert: {e}")))?;
        Ok(())
    }
}

pub type Serial = VmSerial<IrqTrigger, NoEvents, io::Stdout>;

pub fn create(vm: std::sync::Arc<VmFd>) -> Serial {
    let trigger = IrqTrigger::new(vm, SERIAL_IRQ);
    Serial::new(trigger, io::stdout())
}
