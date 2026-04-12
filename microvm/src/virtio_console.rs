// Virtio console — replaces 8250 UART serial for interactive terminal.
//
// Two virtqueues: RX (host→guest) and TX (guest→host).
// No FIFO limits, no baud rate — just raw bytes through virtio descriptors.
// The guest sees /dev/hvc0 via CONFIG_VIRTIO_CONSOLE.
//
// Input/output are abstracted as Read/Write trait objects so the console
// can be driven by stdin (interactive), a Unix socket, a TCP connection,
// or a channel from a REST API. The VMM wires up the concrete I/O at
// construction time.
//
// RX: worker thread reads from input source, writes to guest descriptors,
//     fires irqfd. Blocks on Read — zero CPU when idle.
// TX: guest writes output, we read descriptors and write to output sink.
//     Processed inline on vCPU thread (fast — just a write syscall).

use std::io::{self, Read, Write};
use std::sync::atomic::Ordering;
use std::thread;

use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestMemory, GuestMemoryMmap};

use crate::virtio_mmio::{QueueConfig, VirtioMmioDevice};

const VIRTIO_DEV_CONSOLE: u32 = 3;
const VENDOR_ID: u32 = 0x554D_4551;
const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: usize = 2;

const RX_QUEUE: usize = 0;
const TX_QUEUE: usize = 1;

// Feature bits
const VIRTIO_CONSOLE_F_SIZE: u64 = 1 << 0;

pub struct VirtioConsole {
    pub mmio: VirtioMmioDevice,
    pub mmio_base: u64,
    tx_queue: Queue,
    rx_queue: Option<Queue>,
    /// Input source for the guest — stdin, socket, pipe, etc.
    input: Option<Box<dyn Read + Send>>,
    /// Output sink for guest writes — stdout, socket, pipe, etc.
    output: Box<dyn Write + Send>,
    _rx_worker: Option<thread::JoinHandle<()>>,
    cols: u16,
    rows: u16,
}

impl VirtioConsole {
    /// Create a console with custom I/O endpoints.
    /// `input`: where guest input comes from (stdin, socket, channel)
    /// `output`: where guest output goes (stdout, socket, channel)
    pub fn new(
        irq: u32,
        mmio_base: u64,
        input: Box<dyn Read + Send>,
        output: Box<dyn Write + Send>,
    ) -> Self {
        let mmio = VirtioMmioDevice::new(
            VIRTIO_DEV_CONSOLE,
            VENDOR_ID,
            VIRTIO_CONSOLE_F_SIZE,
            irq,
            NUM_QUEUES,
            QUEUE_SIZE,
        );

        Self {
            mmio,
            mmio_base,
            tx_queue: Queue::new(QUEUE_SIZE).unwrap(),
            rx_queue: Some(Queue::new(QUEUE_SIZE).unwrap()),
            input: Some(input),
            output,
            _rx_worker: None,
            cols: 80,
            rows: 24,
        }
    }

    /// Convenience: create a console wired to stdin/stdout.
    pub fn stdio(irq: u32, mmio_base: u64) -> Self {
        Self::new(
            irq,
            mmio_base,
            Box::new(io::stdin()),
            Box::new(io::stdout()),
        )
    }

    pub fn config_read(&self, offset: u64) -> u32 {
        // Config space: cols (u16) + rows (u16)
        let config: [u8; 4] = [
            self.cols as u8, (self.cols >> 8) as u8,
            self.rows as u8, (self.rows >> 8) as u8,
        ];
        let off = offset as usize;
        if off < config.len() {
            let mut buf = [0u8; 4];
            let len = std::cmp::min(4, config.len() - off);
            buf[..len].copy_from_slice(&config[off..off + len]);
            u32::from_le_bytes(buf)
        } else {
            0
        }
    }

    pub fn activate_queue(&mut self, queue_idx: usize) {
        let qc = match self.mmio.queues.get(queue_idx) {
            Some(qc) => qc.clone(),
            None => return,
        };

        fn configure(q: &mut Queue, qc: &QueueConfig) {
            q.set_size(qc.size);
            q.set_desc_table_address(
                Some((qc.desc_addr & 0xFFFF_FFFF) as u32),
                Some((qc.desc_addr >> 32) as u32),
            );
            q.set_avail_ring_address(
                Some((qc.avail_addr & 0xFFFF_FFFF) as u32),
                Some((qc.avail_addr >> 32) as u32),
            );
            q.set_used_ring_address(
                Some((qc.used_addr & 0xFFFF_FFFF) as u32),
                Some((qc.used_addr >> 32) as u32),
            );
            q.set_ready(true);
        }

        match queue_idx {
            RX_QUEUE => {
                if let Some(ref mut q) = self.rx_queue {
                    configure(q, &qc);
                }
            }
            TX_QUEUE => configure(&mut self.tx_queue, &qc),
            _ => {}
        }
    }

    /// Start the RX worker — reads from input source, delivers to guest.
    pub fn start_rx_worker(&mut self, memory: GuestMemoryMmap) {
        let rx_queue = match self.rx_queue.take() {
            Some(q) => q,
            None => return,
        };
        let mut input = match self.input.take() {
            Some(r) => r,
            None => return,
        };

        let irqfd = self.mmio.irqfd.try_clone().expect("clone irqfd");
        // Clone the Arc to give the worker its own reference; the MMIO
        // device keeps its original so the vCPU thread can still read/
        // write the InterruptStatus register.
        let interrupt_status = self.mmio.interrupt_status.clone();

        self._rx_worker = Some(thread::spawn(move || {
            let mut rx_queue = rx_queue;
            let mut buf = [0u8; 4096];

            // Wait for guest to post buffers before draining
            loop {
                if rx_queue.pop_descriptor_chain(&memory).is_some() {
                    // Put it back — go_to_previous_position not available,
                    // but the guest will re-post on next interrupt cycle.
                    // Break to start the main loop.
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }

            loop {
                // Block on input read — zero CPU when idle
                let n = match input.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };

                let mut written = 0;
                while written < n {
                    let dc = match rx_queue.pop_descriptor_chain(&memory) {
                        Some(dc) => dc,
                        None => {
                            // No guest buffers — wait briefly and retry
                            std::thread::sleep(std::time::Duration::from_millis(1));
                            continue;
                        }
                    };
                    let head = dc.head_index();

                    let mut chunk_written = 0;
                    for desc in dc.writable() {
                        if written + chunk_written >= n { break; }
                        let remaining = n - written - chunk_written;
                        let len = std::cmp::min(desc.len() as usize, remaining);
                        memory.write_slice(
                            &buf[written + chunk_written..written + chunk_written + len],
                            desc.addr(),
                        ).ok();
                        chunk_written += len;
                    }

                    rx_queue.add_used(&memory, head, chunk_written as u32).ok();
                    written += chunk_written;

                    // Interrupt after each descriptor to keep latency low.
                    // `Arc<AtomicU32>` derefs to `&AtomicU32` — no unsafe needed.
                    interrupt_status.fetch_or(0x01, Ordering::Relaxed);
                    irqfd.write(1).ok();
                }
            }
        }));
    }

    /// Process TX — guest sent output, write to output sink.
    /// Called on QueueNotify MMIO exit.
    pub fn process_tx(&mut self, memory: &GuestMemoryMmap) {
        while let Some(desc_chain) = self.tx_queue.pop_descriptor_chain(memory) {
            let head_index = desc_chain.head_index();

            for desc in desc_chain.readable() {
                let mut buf = vec![0u8; desc.len() as usize];
                if memory.read_slice(&mut buf, desc.addr()).is_ok() {
                    self.output.write_all(&buf).ok();
                }
            }

            self.tx_queue.add_used(memory, head_index, 0).ok();
        }

        self.output.flush().ok();
    }
}
