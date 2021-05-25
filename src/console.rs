use super::*;
use crate::queue::VirtQueue;
use bitflags::*;
use core::ptr::NonNull;
use core::sync::atomic::spin_loop_hint;
use log::*;
use volatile::{ReadOnly, WriteOnly};

const QUEUE_RECEIVEQ_PORT_0: usize = 0;
const QUEUE_TRANSMITQ_PORT_0: usize = 1;

/// Virtio console. Only one single port is allowed since ``alloc'' is disabled.
/// Emergency and cols/rows unimplemented.
pub struct VirtIOConsole<'a> {
    header: NonNull<VirtIOHeader>,
    receiveq: VirtQueue<'a, 2>,
    transmitq: VirtQueue<'a, 2>,
    queue_buf_dma: DMA,
    queue_buf_rx: &'a mut [u8],
    cursor: usize,
    pending_len: usize,
}

impl VirtIOConsole<'_> {
    /// Create a new VirtIO-Console driver.
    pub fn new<PS: PageSize>(mut header: NonNull<VirtIOHeader>) -> Result<Self> {
        let header_mut = unsafe { header.as_mut() };
        header_mut.begin_init::<PS, _>(|features| {
            let features = Features::from_bits_truncate(features);
            info!("Device features {:?}", features);
            let supported_features = Features::empty();
            (features & supported_features).bits()
        });
        let config = unsafe { &mut *(header_mut.config_space() as *mut Config) };
        info!("Config: {:?}", config);
        let receiveq = VirtQueue::new::<PS>(header_mut, QUEUE_RECEIVEQ_PORT_0)?;
        let transmitq = VirtQueue::new::<PS>(header_mut, QUEUE_TRANSMITQ_PORT_0)?;
        let queue_buf_dma = DMA::new(1)?;
        let queue_buf_rx = unsafe { &mut queue_buf_dma.as_buf::<PS>()[0..] };
        header_mut.finish_init();
        let mut console = VirtIOConsole {
            header,
            receiveq,
            transmitq,
            queue_buf_dma,
            queue_buf_rx,
            cursor: 0,
            pending_len: 0,
        };
        console.poll_retrieve()?;
        Ok(console)
    }
    fn poll_retrieve(&mut self) -> Result<()> {
        self.receiveq.add(&[], &[self.queue_buf_rx])?;
        Ok(())
    }
    /// Acknowledge interrupt.
    pub fn ack_interrupt(&mut self) -> Result<bool> {
        let ack = self.header_mut().ack_interrupt();
        if !ack {
            return Ok(false);
        }
        let mut flag = false;
        while let Some((_token, len)) = self.receiveq.pop_used() {
            assert_eq!(flag, false);
            flag = true;
            assert_ne!(len, 0);
            self.cursor = 0;
            self.pending_len = len as usize;
        }
        Ok(flag)
    }

    /// Try get char.
    pub fn recv(&mut self, pop: bool) -> Result<Option<u8>> {
        if self.cursor == self.pending_len {
            return Ok(None);
        }
        let ch = self.queue_buf_rx[self.cursor];
        if pop {
            self.cursor += 1;
            if self.cursor == self.pending_len {
                self.poll_retrieve()?;
            }
        }
        Ok(Some(ch))
    }
    /// Put a char onto the device.
    pub fn send(&mut self, chr: u8) -> Result<()> {
        let buf: [u8; 1] = [chr];
        self.transmitq.add(&[&buf], &[])?;
        self.header_mut().notify(QUEUE_TRANSMITQ_PORT_0 as u32);
        while !self.transmitq.can_pop() {
            spin_loop_hint();
        }
        self.transmitq.pop_used().ok_or(Error::NotReady)?;
        Ok(())
    }

    #[inline(always)]
    fn header_mut(&self) -> &mut VirtIOHeader {
        unsafe { &mut *self.header.as_ptr() }
    }
}

#[repr(C)]
#[derive(Debug)]
struct Config {
    cols: ReadOnly<u16>,
    rows: ReadOnly<u16>,
    max_nr_ports: ReadOnly<u32>,
    emerg_wr: WriteOnly<u32>,
}

bitflags! {
    struct Features: u64 {
        const SIZE                  = 1 << 0;
        const MULTIPORT             = 1 << 1;
        const EMERG_WRITE           = 1 << 2;

        // device independent
        const NOTIFY_ON_EMPTY       = 1 << 24; // legacy
        const ANY_LAYOUT            = 1 << 27; // legacy
        const RING_INDIRECT_DESC    = 1 << 28;
        const RING_EVENT_IDX        = 1 << 29;
        const UNUSED                = 1 << 30; // legacy
        const VERSION_1             = 1 << 32; // detect legacy

        // since virtio v1.1
        const ACCESS_PLATFORM       = 1 << 33;
        const RING_PACKED           = 1 << 34;
        const IN_ORDER              = 1 << 35;
        const ORDER_PLATFORM        = 1 << 36;
        const SR_IOV                = 1 << 37;
        const NOTIFICATION_DATA     = 1 << 38;
    }
}
