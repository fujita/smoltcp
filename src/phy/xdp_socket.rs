use afxdp::buf::Buf;
use afxdp::mmaparea::{MmapArea, MmapAreaOptions};
use afxdp::socket::{Socket, SocketOptions, SocketRx, SocketTx};
use afxdp::umem::{Umem, UmemCompletionQueue, UmemFillQueue};
use afxdp::PENDING_LEN;
use arraydeque::{ArrayDeque, Wrapping};
use libbpf_sys::{XSK_RING_CONS__DEFAULT_NUM_DESCS, XSK_RING_PROD__DEFAULT_NUM_DESCS};
use std::cell::RefCell;
use std::cmp::min;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::sync::Arc;
use std::vec::Vec;

use phy::{self, Device, DeviceCapabilities};
use time::Instant;
use Result;

pub struct XdpSocket<'a> {
    //    lower: Arc<Socket<'a, BufCustom>>,
    rx: SocketRx<'a, BufCustom>,
    tx: Rc<RefCell<SocketTx<'a, BufCustom>>>,
    umem: Arc<Umem<'a, BufCustom>>,
    cq: UmemCompletionQueue<'a, BufCustom>,
    fq: Rc<RefCell<UmemFillQueue<'a, BufCustom>>>,
    v: Rc<RefCell<ArrayDeque<[Buf<'a, BufCustom>; PENDING_LEN], Wrapping>>>,
    bufs: Rc<RefCell<Vec<Buf<'a, BufCustom>>>>,
    fd: RawFd,
    mtu: usize,
}

impl AsRawFd for XdpSocket<'_> {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

#[derive(Default, Copy, Clone)]
struct BufCustom {}

impl<'a> XdpSocket<'a> {
    pub fn new(name: &str) -> io::Result<XdpSocket<'a>> {
        let bufnum = 4096;
        let bufsize = 4096;

        let options = MmapAreaOptions { huge_tlb: false };
        let r = MmapArea::new(bufnum, bufsize, options);
        let (area, buf_pool) = match r {
            Ok((area, buf_pool)) => (area, buf_pool),
            Err(err) => panic!("no mmap for you: {:?}", err),
        };

        let r = Umem::new(
            area.clone(),
            XSK_RING_CONS__DEFAULT_NUM_DESCS,
            XSK_RING_PROD__DEFAULT_NUM_DESCS,
        );
        let (umem1, umem1cq, mut umem1fq) = match r {
            Ok(umem) => umem,
            Err(err) => panic!("no umem for you: {:?}", err),
        };
        let options = SocketOptions::default();

        let mut bufs: Vec<Buf<BufCustom>> = Vec::with_capacity(bufnum);
        let r = buf_pool.lock().unwrap().get(&mut bufs, bufnum);
        match r {
            Ok(n) => {
                if n != bufnum {
                    panic!("failed to get initial bufs {} {}", n, bufnum,);
                }
            }
            Err(err) => panic!("error: {:?}", err),
        }

        let r = umem1fq.fill(
            &mut bufs,
            min(XSK_RING_PROD__DEFAULT_NUM_DESCS as usize, bufnum),
        );
        match r {
            Ok(n) => {
                if n != min(XSK_RING_PROD__DEFAULT_NUM_DESCS as usize, bufnum) {
                    panic!(
                        "Initial fill of umem incomplete. Wanted {} got {}.",
                        bufnum, n
                    );
                }
            }
            Err(err) => panic!("error: {:?}", err),
        }

        let socket = if let Ok(skt) = Socket::new(
            umem1.clone(),
            name,
            0,
            XSK_RING_CONS__DEFAULT_NUM_DESCS,
            XSK_RING_PROD__DEFAULT_NUM_DESCS,
            options,
        ) {
            let fd = skt.1.fd;
            XdpSocket {
                //                lower: skt.0,
                rx: skt.1,
                tx: Rc::new(RefCell::new(skt.2)),
                umem: umem1.clone(),
                cq: umem1cq,
                fq: Rc::new(RefCell::new(umem1fq)),
                bufs: Rc::new(RefCell::new(bufs)),
                v: Rc::new(RefCell::new(ArrayDeque::new())),
                fd,
                mtu: 9001,
            }
        } else {
            panic!("no socket for you");
        };
        Ok(socket)
    }
}

impl<'a, 'b: 'a> Device<'a> for XdpSocket<'b> {
    type RxToken = RxToken;
    type TxToken = TxToken<'b>;

    fn capabilities(&self) -> DeviceCapabilities {
        DeviceCapabilities {
            max_transmission_unit: self.mtu,
            ..DeviceCapabilities::default()
        }
    }

    fn receive(&'a mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        let custom = BufCustom {};
        let mut bufs = self.bufs.borrow_mut();
        let mut v = self.v.borrow_mut();
        let r = self.cq.service(&mut bufs, 1);
        match r {
            Ok(_) => {}
            Err(err) => panic!("error: {:?}", err),
        }

        match self.rx.try_recv(&mut v, 1, custom) {
            Ok(n) => {
                if n == 0 {
                    let mut fq = self.fq.borrow_mut();
                    if fq.needs_wakeup() {
                        self.rx.wake();
                    }
                    return None;
                }
                let b = v.back_mut().unwrap();
                let mut buffer = vec![0; b.data.len() as usize];
                for i in 0..b.len as usize {
                    buffer[i] = b.data[i];
                }
                return Some((
                    RxToken { buffer: buffer },
                    TxToken {
                        tx: self.tx.clone(),
                        bufs: self.bufs.clone(),
                        fq: self.fq.clone(),
                        v: self.v.clone(),
                        umem: self.umem.clone(),
                    },
                ));
            }
            Err(err) => panic!("{:?}", err),
        }
    }

    fn transmit(&'a mut self) -> Option<Self::TxToken> {
        Some(TxToken {
            tx: self.tx.clone(),
            fq: self.fq.clone(),
            bufs: self.bufs.clone(),
            umem: self.umem.clone(),
            v: self.v.clone(),
        })
    }
}

pub struct RxToken {
    buffer: Vec<u8>,
}

impl phy::RxToken for RxToken {
    fn consume<R, F>(mut self, _timestamp: Instant, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        f(&mut self.buffer[..])
    }
}

pub struct TxToken<'a> {
    tx: Rc<RefCell<SocketTx<'a, BufCustom>>>,
    fq: Rc<RefCell<UmemFillQueue<'a, BufCustom>>>,
    bufs: Rc<RefCell<Vec<Buf<'a, BufCustom>>>>,
    v: Rc<RefCell<ArrayDeque<[Buf<'a, BufCustom>; PENDING_LEN], Wrapping>>>,
    umem: Arc<Umem<'a, BufCustom>>,
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, _timestamp: Instant, len: usize, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        let mut v = self.v.borrow_mut();
        if v.len() == 0 {
            let mut buffer = vec![0; 0];
            let result = f(&mut buffer);
            return result;
        }
        let mut buffer = vec![0; len];
        let result = f(&mut buffer);
        let b = v.front_mut().unwrap();
        unsafe {
            let ptr = self.umem.area.ptr.offset(b.addr as isize);
            b.data = std::slice::from_raw_parts_mut(ptr as *mut u8, len as usize);
        }

        for i in 0..len {
            b.data[i] = buffer[i];
        }
        b.len = len as u32;

        let mut tx = self.tx.borrow_mut();
        let r = tx.try_send(&mut v, 1);
        match r {
            Ok(_) => {}
            Err(_) => panic!("shouldn't happen"),
        }

        let mut bufs = self.bufs.borrow_mut();
        let mut fq = self.fq.borrow_mut();
        let r = fq.fill(&mut bufs, 1);
        match r {
            Ok(_) => {}
            Err(err) => panic!("error: {:?}", err),
        }

        result
    }
}
