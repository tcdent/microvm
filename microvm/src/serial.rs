use std::io::{self, Read};
use std::sync::{Arc, Mutex};
use std::thread;

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

type InnerSerial = VmSerial<IrqTrigger, NoEvents, io::Stdout>;

pub struct Serial {
    inner: Arc<Mutex<InnerSerial>>,
}

impl Serial {
    pub fn write(&self, offset: u8, value: u8) {
        self.inner.lock().unwrap().write(offset, value).ok();
    }

    pub fn read(&self, offset: u8) -> u8 {
        self.inner.lock().unwrap().read(offset)
    }

    /// Spawn a thread that reads stdin and feeds bytes to the guest serial.
    pub fn start_stdin_thread(&self) {
        let inner = Arc::clone(&self.inner);
        thread::Builder::new().name("vm-stdin".into()).spawn(move || {
            let stdin = io::stdin();
            let mut handle = stdin.lock();
            let mut buf = [0u8; 64];
            loop {
                match handle.read(&mut buf) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        let mut serial = inner.lock().unwrap();
                        serial.enqueue_raw_bytes(&buf[..n]).ok();
                    }
                    Err(_) => break,
                }
            }
        }).unwrap();
    }
}

pub fn create(vm: &VmFd) -> Serial {
    let eventfd = EventFd::new(libc::EFD_NONBLOCK).expect("eventfd");
    vm.register_irqfd(&eventfd, SERIAL_IRQ).expect("register serial irqfd");
    let trigger = IrqTrigger { eventfd: Arc::new(eventfd) };
    let inner = InnerSerial::new(trigger, io::stdout());
    Serial { inner: Arc::new(Mutex::new(inner)) }
}
