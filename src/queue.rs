use alloc::{sync::Arc, vec::Vec};
use core::future::Future;
use core::mem::size_of;
use core::pin::Pin;
use core::ptr::NonNull;
use core::slice;
use core::sync::atomic::{fence, Ordering};
use core::task::{Context, Poll, Waker};

use super::*;
use crate::header::VirtIOHeader;
use bitflags::*;

use volatile::Volatile;

/// The mechanism for bulk data transport on virtio devices.
///
/// Each device can have zero or more virtqueues.
#[repr(C)]
pub struct VirtQueue<'a, const QUEUE_SIZE: usize> {
    /// DMA guard
    dma: DMA,
    /// The index of queue
    queue_idx: u32,
    inner: Arc<spin::Mutex<VirtQueueInner<'a>>>,
}

impl<'a, const QUEUE_SIZE: usize> VirtQueue<'a, QUEUE_SIZE> {
    /// Create a new VirtQueue.
    pub fn new<PS: PageSize>(header: &mut VirtIOHeader, idx: usize) -> Result<Self> {
        if header.queue_used(idx as u32) {
            return Err(Error::AlreadyUsed);
        }
        if !QUEUE_SIZE.is_power_of_two() || header.max_queue_size() < QUEUE_SIZE as u32 {
            return Err(Error::InvalidParam);
        }
        let layout = VirtQueueLayout::new(QUEUE_SIZE);
        // alloc continuous pages
        let dma = DMA::new(layout.size / PS::page_size())?;

        header.queue_set(
            idx as u32,
            QUEUE_SIZE as u32,
            PS::page_size() as u32,
            dma.pfn::<PS>(),
        );

        let desc = unsafe { slice::from_raw_parts_mut(dma.vaddr() as *mut Descriptor, QUEUE_SIZE) };
        let avail = unsafe { &mut *((dma.vaddr() + layout.avail_offset) as *mut AvailRing) };
        let used = unsafe { &mut *((dma.vaddr() + layout.used_offset) as *mut UsedRing) };

        // link descriptors together
        for i in 0..(QUEUE_SIZE - 1) {
            desc[i as usize].next.write(i as u16 + 1);
        }

        Ok(VirtQueue {
            dma,
            queue_idx: idx as u32,
            inner: Arc::new(spin::Mutex::new(VirtQueueInner {
                desc,
                avail,
                used,
                num_used: 0,
                free_head: 0,
                avail_idx: 0,
                last_used_idx: 0,
                wakers: vec![None; QUEUE_SIZE],
            })),
        })
    }

    /// Add buffers to the virtqueue, return a token.
    ///
    /// Ref: linux virtio_ring.c virtqueue_add
    pub fn add(&self, inputs: &[&[u8]], outputs: &[&mut [u8]]) -> Result<u16> {
        self.inner.lock().add::<QUEUE_SIZE>(inputs, outputs)
    }

    pub fn async_add(
        &self,
        header: NonNull<VirtIOHeader>,
        inputs: &[&[u8]],
        outputs: &[&mut [u8]],
    ) -> Result<VirtFuture<'a, QUEUE_SIZE>> {
        let desc_idx = self.add(inputs, outputs)?;
        Ok(VirtFuture::<'a, QUEUE_SIZE>::new(
            desc_idx,
            self.queue_idx,
            header,
            self.inner.clone(),
        ))
    }

    /// Whether there is a used element that can pop.
    pub fn can_pop(&self) -> bool {
        self.inner.lock().can_pop()
    }

    /// The number of free descriptors.
    pub fn available_desc(&self) -> usize {
        self.inner.lock().available_desc::<QUEUE_SIZE>()
    }

    /// Get a token from device used buffers, return (token, len).
    ///
    /// Ref: linux virtio_ring.c virtqueue_get_buf_ctx
    pub fn pop_used(&self) -> Option<(u16, u32)> {
        self.inner.lock().pop_used::<QUEUE_SIZE>()
    }

    /// Handle interrupt, waker.wake() will be called in this function.
    pub fn handle_interrupt(&self) -> core::result::Result<(), HandleIntrError> {
        self.inner.lock().handle_interrupt::<QUEUE_SIZE>()
    }
}

struct VirtQueueInner<'a> {
    /// Descriptor table
    desc: &'a mut [Descriptor],
    /// Available ring
    avail: &'a mut AvailRing,
    /// Used ring
    used: &'a mut UsedRing,
    /// The number of used queues.
    num_used: u16,
    /// The head desc index of the free list.
    free_head: u16,
    avail_idx: u16,
    last_used_idx: u16,

    // Can not use fixed array instead of Vec
    // due to the const generic limitations.
    wakers: Vec<Option<Waker>>,
}

impl VirtQueueInner<'_> {
    fn can_pop(&self) -> bool {
        self.last_used_idx != self.used.idx.read()
    }

    fn available_desc<const QUEUE_SIZE: usize>(&self) -> usize {
        QUEUE_SIZE - self.num_used as usize
    }

    /// Recycle descriptors in the list specified by head.
    ///
    /// This will push all linked descriptors at the front of the free list.
    fn recycle_descriptors(&mut self, mut head: u16) {
        let origin_free_head = self.free_head;
        self.free_head = head;
        loop {
            let desc = &mut self.desc[head as usize];
            let flags = desc.flags.read();
            self.num_used -= 1;
            if flags.contains(DescFlags::NEXT) {
                head = desc.next.read();
            } else {
                desc.next.write(origin_free_head);
                return;
            }
        }
    }

    fn next_used<const QUEUE_SIZE: usize>(&self) -> Option<(u16, u32)> {
        if !self.can_pop() {
            return None;
        }

        // read barrier
        fence(Ordering::SeqCst);

        let last_used_slot = self.last_used_idx as usize & (QUEUE_SIZE - 1);
        let index = self.used.ring[last_used_slot].id.read() as u16;
        let len = self.used.ring[last_used_slot].len.read();

        Some((index, len))
    }

    fn pop_used<const QUEUE_SIZE: usize>(&mut self) -> Option<(u16, u32)> {
        if !self.can_pop() {
            return None;
        }

        // read barrier
        fence(Ordering::SeqCst);

        let last_used_slot = self.last_used_idx as usize & (QUEUE_SIZE - 1);
        let index = self.used.ring[last_used_slot].id.read() as u16;
        let len = self.used.ring[last_used_slot].len.read();

        self.recycle_descriptors(index);
        self.last_used_idx = self.last_used_idx.wrapping_add(1);

        Some((index, len))
    }

    fn add<const QUEUE_SIZE: usize>(
        &mut self,
        inputs: &[&[u8]],
        outputs: &[&mut [u8]],
    ) -> Result<u16> {
        if inputs.is_empty() && outputs.is_empty() {
            return Err(Error::InvalidParam);
        }
        if inputs.len() + outputs.len() + self.num_used as usize > QUEUE_SIZE {
            return Err(Error::BufferTooSmall);
        }

        // allocate descriptors from free list
        let head = self.free_head;
        let mut last = self.free_head;
        for input in inputs.iter() {
            let desc = &mut self.desc[self.free_head as usize];
            desc.set_buf(input);
            desc.flags.write(DescFlags::NEXT);
            last = self.free_head;
            self.free_head = desc.next.read();
        }
        for output in outputs.iter() {
            let desc = &mut self.desc[self.free_head as usize];
            desc.set_buf(output);
            desc.flags.write(DescFlags::NEXT | DescFlags::WRITE);
            last = self.free_head;
            self.free_head = desc.next.read();
        }
        // set last_elem.next = NULL
        {
            let desc = &mut self.desc[last as usize];
            let mut flags = desc.flags.read();
            flags.remove(DescFlags::NEXT);
            desc.flags.write(flags);
        }
        self.num_used += (inputs.len() + outputs.len()) as u16;

        let avail_slot = self.avail_idx as usize & (QUEUE_SIZE - 1);
        self.avail.ring[avail_slot as usize].write(head);

        // write barrier
        fence(Ordering::SeqCst);

        // increase head of avail ring
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.avail.idx.write(self.avail_idx);
        Ok(head)
    }

    fn handle_interrupt<const QUEUE_SIZE: usize>(
        &self,
    ) -> core::result::Result<(), HandleIntrError> {
        let (index, _) = self
            .next_used::<QUEUE_SIZE>()
            .ok_or(HandleIntrError::QueueNotReady)?;
        self.wakers[index as usize]
            .as_ref()
            .ok_or(HandleIntrError::WakerNotExist(index))?
            .wake_by_ref();
        Ok(())
    }
}

pub struct VirtFuture<'a, const QUEUE_SIZE: usize> {
    desc_idx: u16,
    queue_idx: u32,
    first_poll: Option<()>,
    header: NonNull<VirtIOHeader>,
    virt_queue_inner: Arc<spin::Mutex<VirtQueueInner<'a>>>,
}

impl<'a, const QUEUE_SIZE: usize> VirtFuture<'a, QUEUE_SIZE> {
    fn new(
        desc_idx: u16,
        queue_idx: u32,
        header: NonNull<VirtIOHeader>,
        virt_queue_inner: Arc<spin::Mutex<VirtQueueInner<'a>>>,
    ) -> Self {
        Self {
            desc_idx,
            queue_idx,
            first_poll: Some(()),
            header,
            virt_queue_inner,
        }
    }
}

impl<const QUEUE_SIZE: usize> Future for VirtFuture<'_, QUEUE_SIZE> {
    type Output = (u16, u32);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        if self.first_poll.take().is_some() {
            let queue_idx = self.queue_idx;
            unsafe { self.header.as_mut() }.notify(queue_idx);
        }

        let mut virt_queue_inner = self.virt_queue_inner.lock();

        if let Some(output) = virt_queue_inner.pop_used::<QUEUE_SIZE>() {
            Poll::Ready(output)
        } else {
            virt_queue_inner.wakers[self.desc_idx as usize] = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// The inner layout of a VirtQueue.
///
/// Ref: 2.6.2 Legacy Interfaces: A Note on Virtqueue Layout
struct VirtQueueLayout {
    avail_offset: usize,
    used_offset: usize,
    size: usize,
}

impl VirtQueueLayout {
    const fn new(queue_size: usize) -> Self {
        let desc = size_of::<Descriptor>() * queue_size;
        let avail = size_of::<u16>() * (3 + queue_size);
        let used = size_of::<u16>() * 3 + size_of::<UsedElem>() * queue_size;
        VirtQueueLayout {
            avail_offset: desc,
            used_offset: align_up(desc + avail, 1 << queue_size),
            size: align_up(desc + avail, 1 << queue_size) + align_up(used, 1 << queue_size),
        }
    }
}

#[repr(C, align(16))]
#[derive(Debug)]
struct Descriptor {
    addr: Volatile<u64>,
    len: Volatile<u32>,
    flags: Volatile<DescFlags>,
    next: Volatile<u16>,
}

impl Descriptor {
    fn set_buf(&mut self, buf: &[u8]) {
        self.addr.write(virt_to_phys(buf.as_ptr() as usize) as u64);
        self.len.write(buf.len() as u32);
    }
}

bitflags! {
    /// Descriptor flags
    struct DescFlags: u16 {
        const NEXT = 1;
        const WRITE = 2;
        const INDIRECT = 4;
    }
}

/// The driver uses the available ring to offer buffers to the device:
/// each ring entry refers to the head of a descriptor chain.
/// It is only written by the driver and read by the device.
#[repr(C)]
#[derive(Debug)]
struct AvailRing {
    flags: Volatile<u16>,
    /// A driver MUST NOT decrement the idx.
    idx: Volatile<u16>,
    ring: [Volatile<u16>; 32], // actual size: queue_size
    used_event: Volatile<u16>, // unused
}

/// The used ring is where the device returns buffers once it is done with them:
/// it is only written to by the device, and read by the driver.
#[repr(C)]
#[derive(Debug)]
struct UsedRing {
    flags: Volatile<u16>,
    idx: Volatile<u16>,
    ring: [UsedElem; 32],       // actual size: queue_size
    avail_event: Volatile<u16>, // unused
}

#[repr(C)]
#[derive(Debug)]
struct UsedElem {
    id: Volatile<u32>,
    len: Volatile<u32>,
}
