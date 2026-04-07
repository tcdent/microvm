use std::io;
use std::sync::Arc;

use kvm_ioctls::VmFd;
use vm_superio::serial::NoEvents;
use vm_superio::{Serial as VmSerial, Trigger};
use vmm_sys_util::eventfd::EventFd;

pub const SERIAL_IRQ: u32 = 4;

/// IRQ trigger via irqfd — single write() injects the interrupt in kernel space
#[derive(Clone)]
pub struct IrqTrigger {
    eventfd: Arc<EventFd>,
}

impl Trigger for IrqTrigger {
    type E = io::Error;

    fn trigger(&self) -> Result<(), Self::E> {
        self.eventfd.write(1).map_err(|e| io::Error::other(format!("irqfd write: {e}")))
    }
}

pub type Serial = VmSerial<IrqTrigger, NoEvents, io::Stdout>;

pub fn create(vm: &VmFd) -> Serial {
    let eventfd = EventFd::new(libc::EFD_NONBLOCK).expect("eventfd");
    vm.register_irqfd(&eventfd, SERIAL_IRQ).expect("register serial irqfd");
    let trigger = IrqTrigger { eventfd: Arc::new(eventfd) };
    Serial::new(trigger, io::stdout())
}
