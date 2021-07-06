use super::*;
use crate::header::VirtIOHeader;
use crate::queue::{VirtFuture, VirtQueue};
use bitflags::*;
use core::future::Future;
use core::hint::spin_loop;
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{ready, Context, Poll};
use log::*;
use pin_project::pin_project;
use volatile::Volatile;

const QUEUE_SIZE: usize = 16;

/// The virtio block device is a simple virtual block device (ie. disk).
///
/// Read and write requests (and other exotic requests) are placed in the queue,
/// and serviced (probably out of order) by the device except where noted.
pub struct VirtIOBlk<'a> {
    header: NonNull<VirtIOHeader>,
    queue: VirtQueue<'a, { QUEUE_SIZE }>,
    capacity: usize,
}

impl VirtIOBlk<'_> {
    /// Create a new VirtIO-Blk driver.
    pub fn new<PS: PageSize>(mut header: NonNull<VirtIOHeader>) -> Result<Self> {
        let header_mut = unsafe { header.as_mut() };
        header_mut.begin_init::<PS, _>(|features| {
            let features = BlkFeature::from_bits_truncate(features);
            info!("device features: {:?}", features);
            // negotiate these flags only
            let supported_features = BlkFeature::empty();
            (features & supported_features).bits()
        });

        // read configuration space
        let config = unsafe { &mut *(header_mut.config_space() as *mut BlkConfig) };
        info!("config: {:?}", config);
        info!(
            "found a block device of size {}KB",
            config.capacity.read() / 2
        );

        header_mut.finish_init();
        let queue = VirtQueue::new::<PS>(header_mut, 0)?;

        Ok(VirtIOBlk {
            header,
            queue,
            capacity: config.capacity.read() as usize,
        })
    }

    /// Acknowledge interrupt.
    pub fn ack_interrupt(&mut self) -> bool {
        self.header_mut().ack_interrupt()
    }

    /// Read a block.
    pub fn read_block(&self, block_id: usize, buf: &mut [u8]) -> Result {
        assert_eq!(buf.len(), BLK_SIZE);
        let req = BlkReq {
            type_: ReqType::In,
            reserved: 0,
            sector: block_id as u64,
        };
        let mut resp = BlkResp::default();
        self.queue.add(&[req.as_buf()], &[buf, resp.as_buf_mut()])?;
        self.header_mut().notify(0);
        while !self.queue.can_pop() {
            spin_loop();
        }
        self.queue.pop_used().ok_or(Error::NotReady)?;
        match resp.status {
            RespStatus::Ok => Ok(()),
            _ => Err(Error::IoError),
        }
    }

    /// Write a block.
    pub fn write_block(&self, block_id: usize, buf: &[u8]) -> Result {
        assert_eq!(buf.len(), BLK_SIZE);
        let req = BlkReq {
            type_: ReqType::Out,
            reserved: 0,
            sector: block_id as u64,
        };
        let mut resp = BlkResp::default();
        self.queue.add(&[req.as_buf(), buf], &[resp.as_buf_mut()])?;
        self.header_mut().notify(0);
        while !self.queue.can_pop() {
            spin_loop();
        }
        self.queue.pop_used().ok_or(Error::NotReady)?;
        match resp.status {
            RespStatus::Ok => Ok(()),
            _ => Err(Error::IoError),
        }
    }

    #[inline(always)]
    fn header_mut(&self) -> &mut VirtIOHeader {
        unsafe { &mut *self.header.as_ptr() }
    }
}

impl<'a> VirtIOBlk<'a> {
    /// Async read a block.
    pub fn async_read_block(&'a self, block_id: usize, buf: &'a mut [u8]) -> BlkReadFut<'a> {
        assert_eq!(buf.len(), BLK_SIZE);
        let req = BlkReq {
            type_: ReqType::In,
            reserved: 0,
            sector: block_id as u64,
        };
        let resp = BlkResp::default();
        BlkReadFut {
            req,
            resp,
            blk: self,
            buf,
            virt_fut: None,
        }
    }

    /// Async write a block.
    pub fn async_write_block(&'a self, block_id: usize, buf: &'a [u8]) -> BlkWriteFut<'a> {
        assert_eq!(buf.len(), BLK_SIZE);
        let req = BlkReq {
            type_: ReqType::Out,
            reserved: 0,
            sector: block_id as u64,
        };
        let resp = BlkResp::default();
        BlkWriteFut {
            req,
            resp,
            blk: self,
            buf,
            virt_fut: None,
        }
    }
}

/// Future for the [`VirtIOBlk::async_read_block()`] method.
#[pin_project]
pub struct BlkReadFut<'a> {
    req: BlkReq,
    resp: BlkResp,
    blk: &'a VirtIOBlk<'a>,
    buf: &'a mut [u8],
    #[pin]
    virt_fut: Option<VirtFuture<'a, { QUEUE_SIZE }>>,
}

/// Future for the [`VirtIOBlk::async_write_block()`] method.
#[pin_project]
pub struct BlkWriteFut<'a> {
    req: BlkReq,
    resp: BlkResp,
    blk: &'a VirtIOBlk<'a>,
    buf: &'a [u8],
    #[pin]
    virt_fut: Option<VirtFuture<'a, { QUEUE_SIZE }>>,
}

macro_rules! impl_blk_future {
    ($name:ident) => {
        impl<'a> Future for $name<'a> {
            type Output = Result;

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let mut this = self.project();

                loop {
                    let virt_fut = match this.virt_fut.as_mut().as_pin_mut() {
                        Some(virt_fut) => {
                            ready!(virt_fut.poll(cx));
                            return Poll::Ready(match this.resp.status {
                                RespStatus::Ok => Ok(()),
                                _ => Err(Error::IoError),
                            });
                        }
                        None => match this.blk.queue.async_add(
                            this.blk.header,
                            &[this.req.as_buf(), this.buf],
                            &[this.resp.as_buf_mut()],
                        ) {
                            Ok(fut) => fut,
                            Err(e) => return Poll::Ready(Err(e)),
                        },
                    };
                    this.virt_fut.set(Some(virt_fut));
                }
            }
        }
    };
}

impl_blk_future!(BlkReadFut);
impl_blk_future!(BlkWriteFut);

impl InterruptHandler for VirtIOBlk<'_> {
    fn handle_interrupt(&self) -> core::result::Result<(), HandleIntrError> {
        self.queue.handle_interrupt()
    }
}

#[repr(C)]
#[derive(Debug)]
struct BlkConfig {
    /// Number of 512 Bytes sectors
    capacity: Volatile<u64>,
    size_max: Volatile<u32>,
    seg_max: Volatile<u32>,
    cylinders: Volatile<u16>,
    heads: Volatile<u8>,
    sectors: Volatile<u8>,
    blk_size: Volatile<u32>,
    physical_block_exp: Volatile<u8>,
    alignment_offset: Volatile<u8>,
    min_io_size: Volatile<u16>,
    opt_io_size: Volatile<u32>,
    // ... ignored
}

#[repr(C)]
#[derive(Debug)]
struct BlkReq {
    type_: ReqType,
    reserved: u32,
    sector: u64,
}

#[repr(C)]
#[derive(Debug)]
struct BlkResp {
    status: RespStatus,
}

#[repr(u32)]
#[derive(Debug)]
enum ReqType {
    In = 0,
    Out = 1,
    Flush = 4,
    Discard = 11,
    WriteZeroes = 13,
}

#[repr(u8)]
#[derive(Debug, Eq, PartialEq)]
enum RespStatus {
    Ok = 0,
    IoErr = 1,
    Unsupported = 2,
    _NotReady = 3,
}

impl Default for BlkResp {
    fn default() -> Self {
        BlkResp {
            status: RespStatus::_NotReady,
        }
    }
}

const BLK_SIZE: usize = 512;

bitflags! {
    struct BlkFeature: u64 {
        /// Device supports request barriers. (legacy)
        const BARRIER       = 1 << 0;
        /// Maximum size of any single segment is in `size_max`.
        const SIZE_MAX      = 1 << 1;
        /// Maximum number of segments in a request is in `seg_max`.
        const SEG_MAX       = 1 << 2;
        /// Disk-style geometry specified in geometry.
        const GEOMETRY      = 1 << 4;
        /// Device is read-only.
        const RO            = 1 << 5;
        /// Block size of disk is in `blk_size`.
        const BLK_SIZE      = 1 << 6;
        /// Device supports scsi packet commands. (legacy)
        const SCSI          = 1 << 7;
        /// Cache flush command support.
        const FLUSH         = 1 << 9;
        /// Device exports information on optimal I/O alignment.
        const TOPOLOGY      = 1 << 10;
        /// Device can toggle its cache between writeback and writethrough modes.
        const CONFIG_WCE    = 1 << 11;
        /// Device can support discard command, maximum discard sectors size in
        /// `max_discard_sectors` and maximum discard segment number in
        /// `max_discard_seg`.
        const DISCARD       = 1 << 13;
        /// Device can support write zeroes command, maximum write zeroes sectors
        /// size in `max_write_zeroes_sectors` and maximum write zeroes segment
        /// number in `max_write_zeroes_seg`.
        const WRITE_ZEROES  = 1 << 14;

        // device independent
        const NOTIFY_ON_EMPTY       = 1 << 24; // legacy
        const ANY_LAYOUT            = 1 << 27; // legacy
        const RING_INDIRECT_DESC    = 1 << 28;
        const RING_EVENT_IDX        = 1 << 29;
        const UNUSED                = 1 << 30; // legacy
        const VERSION_1             = 1 << 32; // detect legacy

        // the following since virtio v1.1
        const ACCESS_PLATFORM       = 1 << 33;
        const RING_PACKED           = 1 << 34;
        const IN_ORDER              = 1 << 35;
        const ORDER_PLATFORM        = 1 << 36;
        const SR_IOV                = 1 << 37;
        const NOTIFICATION_DATA     = 1 << 38;
    }
}

unsafe impl AsBuf for BlkReq {}
unsafe impl AsBuf for BlkResp {}
