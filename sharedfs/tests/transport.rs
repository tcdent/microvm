//! The queue worker against a hand-built split virtqueue in guest memory:
//! the same descriptor handling the guest driver will exercise, with no
//! kernel involved.

use std::fs;

use sharedfs::device::{Outbox, drain};
use sharedfs::proto::{NOTIFY_BUF, Notification, ROOT_INO, mask, op, ino_of};
use sharedfs::{Request as R, Response as P, Server};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

const DESC: u64 = 0x1000;
const AVAIL: u64 = 0x2000;
const USED: u64 = 0x3000;
const BUF: u64 = 0x10000;
const QSIZE: u16 = 16;

struct Ring {
    mem: GuestMemoryMmap,
    queue: Queue,
    next_desc: u16,
    avail_idx: u16,
    next_buf: u64,
}

impl Ring {
    fn new() -> Ring {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
        let mut queue = Queue::new(QSIZE).unwrap();
        queue.set_size(QSIZE);
        queue.set_desc_table_address(Some(DESC as u32), Some(0));
        queue.set_avail_ring_address(Some(AVAIL as u32), Some(0));
        queue.set_used_ring_address(Some(USED as u32), Some(0));
        queue.set_ready(true);
        Ring { mem, queue, next_desc: 0, avail_idx: 0, next_buf: BUF }
    }

    fn desc(&mut self, addr: u64, len: u32, flags: u16, next: u16) -> u16 {
        let i = self.next_desc;
        self.next_desc += 1;
        let mut d = Vec::new();
        d.extend_from_slice(&addr.to_le_bytes());
        d.extend_from_slice(&len.to_le_bytes());
        d.extend_from_slice(&flags.to_le_bytes());
        d.extend_from_slice(&next.to_le_bytes());
        self.mem.write_slice(&d, GuestAddress(DESC + i as u64 * 16)).unwrap();
        i
    }

    fn alloc(&mut self, len: usize) -> u64 {
        let a = self.next_buf;
        self.next_buf += (len as u64 + 15) & !15;
        a
    }

    /// Post one request with `resp_len` bytes of response space,
    /// optionally split across two writable descriptors.
    fn post(&mut self, req: &[u8], resp_len: usize, split: bool) -> (u16, u64) {
        let ra = self.alloc(req.len());
        self.mem.write_slice(req, GuestAddress(ra)).unwrap();
        let wa = self.alloc(resp_len);
        let head = self.next_desc;
        if split && resp_len > 8 {
            let d1 = head + 1;
            let d2 = head + 2;
            self.desc(ra, req.len() as u32, 1, d1);
            self.desc(wa, 8, 1 | 2, d2);
            self.desc(wa + 8, (resp_len - 8) as u32, 2, 0);
        } else {
            let d1 = head + 1;
            self.desc(ra, req.len() as u32, 1, d1);
            self.desc(wa, resp_len as u32, 2, 0);
        }
        let slot = self.avail_idx % QSIZE;
        self.mem.write_obj(head, GuestAddress(AVAIL + 4 + slot as u64 * 2)).unwrap();
        self.avail_idx += 1;
        self.mem.write_obj(self.avail_idx, GuestAddress(AVAIL + 2)).unwrap();
        (head, wa)
    }

    fn used(&self) -> Vec<(u32, u32)> {
        let n: u16 = self.mem.read_obj(GuestAddress(USED + 2)).unwrap();
        (0..n)
            .map(|i| {
                let id: u32 = self.mem.read_obj(GuestAddress(USED + 4 + i as u64 * 8)).unwrap();
                let len: u32 = self.mem.read_obj(GuestAddress(USED + 8 + i as u64 * 8)).unwrap();
                (id, len)
            })
            .collect()
    }

    fn response(&self, opc: u32, at: u64, len: u32) -> Result<P, i32> {
        let mut b = vec![0u8; len as usize];
        self.mem.read_slice(&mut b, GuestAddress(at)).unwrap();
        P::decode(opc, &b).unwrap().1
    }
}

#[test]
fn chains_round_trip_through_the_worker() {
    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join("hello"), b"hello from the host").unwrap();
    let mut s = Server::open(t.path(), 1 << 20, None, None, false).unwrap();
    let mut r = Ring::new();

    // Batch of three in one kick: a failing readlink, statfs, create. The statfs response is
    // split across two writable descriptors.
    let (h1, a1) = r.post(&R::Readlink { ino: ROOT_INO }.encode(1), 64, false);
    let (h2, a2) = r.post(&R::Statfs.encode(2), 128, true);
    let (h3, a3) = r.post(&R::Create { parent: ROOT_INO, mode: 0o644, flags: 0, name: b"new".to_vec() }.encode(3), 64, true);

    assert_eq!(drain(&mut s, &mut r.queue, &r.mem), 3);
    let used = r.used();
    assert_eq!(used.len(), 3);
    assert_eq!(used.iter().map(|u| u.0 as u16).collect::<Vec<_>>(), [h1, h2, h3]);

    assert_eq!(r.response(op::READLINK, a1, used[0].1), Err(libc::EINVAL));
    assert!(matches!(r.response(op::STATFS, a2, used[1].1), Ok(P::Statfs { .. })));
    let Ok(P::Node { ino, .. }) = r.response(op::CREATE, a3, used[2].1) else { panic!() };
    assert_eq!(ino, ino_of(2, 1));
    assert!(t.path().join("new").exists());

    // An extend using the inode from the create response.
    let (h4, a4) = r.post(&R::Extend { ino, size: 100 }.encode(4), 64, true);
    assert_eq!(drain(&mut s, &mut r.queue, &r.mem), 1);
    let u = r.used();
    assert_eq!(u[3].0 as u16, h4);
    assert_eq!(r.response(op::EXTEND, a4, u[3].1), Ok(P::Empty));
    assert_eq!(fs::metadata(t.path().join("new")).unwrap().len(), 100);

    // Nothing pending: a spurious kick does no work.
    assert_eq!(drain(&mut s, &mut r.queue, &r.mem), 0);
}

#[test]
fn malformed_request_gets_einval_with_its_unique() {
    let t = tempfile::tempdir().unwrap();
    let mut s = Server::open(t.path(), 1 << 20, None, None, false).unwrap();
    let mut r = Ring::new();
    let mut bad = R::Statfs.encode(77);
    bad.extend_from_slice(b"trailing junk");
    let n = bad.len() as u32;
    bad[..4].copy_from_slice(&n.to_le_bytes());
    let (_, a) = r.post(&bad, 32, false);
    assert_eq!(drain(&mut s, &mut r.queue, &r.mem), 1);
    let u = r.used();
    assert_eq!(P::decode(op::STATFS, &{
        let mut b = vec![0u8; u[0].1 as usize];
        r.mem.read_slice(&mut b, GuestAddress(a)).unwrap();
        b
    })
    .unwrap(), (77, Err(libc::EINVAL)));
}

#[test]
fn response_larger_than_buffer_is_truncated_not_overrun() {
    let t = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("z".repeat(300), t.path().join("long")).unwrap();
    let mut s = Server::open(t.path(), 1 << 20, None, None, false).unwrap();
    let mut r = Ring::new();
    let (_, a) = r.post(&R::Readlink { ino: ROOT_INO + 1 }.encode(1), 100, false);
    // Canary right after the 100-byte response buffer.
    let canary = a + 100;
    r.mem.write_slice(b"CANARY", GuestAddress(canary)).unwrap();
    assert_eq!(drain(&mut s, &mut r.queue, &r.mem), 1);
    assert_eq!(r.used()[0].1, 100, "exactly the buffer, no more");
    let mut c = [0u8; 6];
    r.mem.read_slice(&mut c, GuestAddress(canary)).unwrap();
    assert_eq!(&c, b"CANARY");
}

impl Ring {
    /// Post one empty writable buffer, as the guest does on the notification queue.
    fn post_empty(&mut self, len: usize) -> u64 {
        let wa = self.alloc(len);
        let head = self.next_desc;
        self.desc(wa, len as u32, 2, 0);
        let slot = self.avail_idx % QSIZE;
        self.mem.write_obj(head, GuestAddress(AVAIL + 4 + slot as u64 * 2)).unwrap();
        self.avail_idx += 1;
        self.mem.write_obj(self.avail_idx, GuestAddress(AVAIL + 2)).unwrap();
        wa
    }

    fn read_notification(&self, addr: u64) -> Notification {
        let mut buf = vec![0u8; NOTIFY_BUF];
        self.mem.read_slice(&mut buf, GuestAddress(addr)).unwrap();
        Notification::decode(&buf).unwrap()
    }
}

#[test]
fn host_changes_arrive_as_notifications() {
    let t = tempfile::tempdir().unwrap();
    fs::create_dir(t.path().join("d")).unwrap();
    let mut s = Server::open(t.path(), 4 << 20, None, None, false).unwrap();
    let mut ring = Ring::new();
    let mut outbox = Outbox::default();
    let b1 = ring.post_empty(NOTIFY_BUF);
    let b2 = ring.post_empty(NOTIFY_BUF);
    let d = s.index().lookup(0, b"d").unwrap();

    // Writing a new file: create, modify, close_write. Two buffers, so one stays pending.
    fs::write(t.path().join("d/new.txt"), b"x").unwrap();
    outbox.push(s.pump_watch());
    assert_eq!(outbox.flush(&mut ring.queue, &ring.mem), 2);
    let slot = s.index().lookup(d, b"new.txt").unwrap();
    let event = |n: Notification| match n {
        Notification::Event { mask, slot, parent, name, cookie, .. } => (mask, slot, parent, name, cookie),
        other => panic!("{other:?}"),
    };
    let (m, sl, parent, name, _) = event(ring.read_notification(b1));
    assert_eq!((m, sl, parent, name.as_slice()), (mask::CREATE, slot, d, &b"new.txt"[..]));
    let (m, ..) = event(ring.read_notification(b2));
    assert_eq!(m, mask::MODIFY);

    // A rename within the tree keeps the slot and arrives as a from/to pair sharing a cookie.
    let b3 = ring.post_empty(NOTIFY_BUF);
    let b4 = ring.post_empty(NOTIFY_BUF);
    let b5 = ring.post_empty(NOTIFY_BUF);
    fs::rename(t.path().join("d/new.txt"), t.path().join("moved.txt")).unwrap();
    outbox.push(s.pump_watch());
    assert_eq!(outbox.flush(&mut ring.queue, &ring.mem), 3);
    let (m, ..) = event(ring.read_notification(b3));
    assert_eq!(m, mask::CLOSE_WRITE);
    let (m, sl, parent, name, c1) = event(ring.read_notification(b4));
    assert_eq!((m, sl, parent, name.as_slice()), (mask::MOVED_FROM, slot, d, &b"new.txt"[..]));
    let (m, sl, parent, name, c2) = event(ring.read_notification(b5));
    assert_eq!((m, sl, parent, name.as_slice()), (mask::MOVED_TO, slot, 0, &b"moved.txt"[..]));
    assert!(c1 != 0 && c1 == c2);
    assert_eq!(s.index().lookup(0, b"moved.txt"), Some(slot));

    // A change with no buffer posted waits for one.
    fs::remove_file(t.path().join("moved.txt")).unwrap();
    outbox.push(s.pump_watch());
    assert_eq!(outbox.flush(&mut ring.queue, &ring.mem), 0);
    let b6 = ring.post_empty(NOTIFY_BUF);
    assert_eq!(outbox.flush(&mut ring.queue, &ring.mem), 1);
    let (m, sl, parent, name, _) = event(ring.read_notification(b6));
    assert_eq!((m, sl, parent, name.as_slice()), (mask::DELETE, slot, 0, &b"moved.txt"[..]));

    // Past the backlog bound the outbox collapses into one OVERFLOW and stays quiet after it.
    let flood = (0..=Outbox::BACKLOG as u32)
        .map(|i| Notification::Event { mask: mask::MODIFY, slot: i, parent: 0, cookie: 0, generation: 0, name: b"x".to_vec() })
        .collect();
    outbox.push(flood);
    let b7 = ring.post_empty(NOTIFY_BUF);
    ring.post_empty(NOTIFY_BUF);
    assert_eq!(outbox.flush(&mut ring.queue, &ring.mem), 1);
    assert_eq!(ring.read_notification(b7), Notification::Overflow);
    assert_eq!(outbox.flush(&mut ring.queue, &ring.mem), 0, "the flood is gone");
    fs::write(t.path().join("again.txt"), b"y").unwrap();
    outbox.push(s.pump_watch());
    assert!(outbox.flush(&mut ring.queue, &ring.mem) >= 1, "notifications flow again after the overflow");
}
