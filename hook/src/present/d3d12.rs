//! Direct3D 12 games: the overlay is drawn with d3d11.rs through D3D11On12, on the
//! game's own direct command queue, so it lands in the back buffer before the present
//! that follows. That queue is learnt from `ID3D12CommandQueue::ExecuteCommandLists`
//! (slot 10; its address comes from a throwaway device and queue), the first time the
//! game submits on a direct queue after the hook goes in.
//!
//! Each back buffer is wrapped once (`CreateWrappedResource`, PRESENT state in and out)
//! and acquired / released around the draw; `release_buffers` lets them go before the
//! game resizes its swap chain.

use std::sync::{Mutex, OnceLock};

use retour::GenericDetour;

use super::com::{self, Guid, IID_ID3D11ON12DEVICE, IID_ID3D11TEXTURE2D, IID_ID3D12COMMANDQUEUE, IID_ID3D12DEVICE, IID_ID3D12RESOURCE, release, vcall};
use super::{Frame, d3d11};
use crate::hook::with_lock;
use crate::log;

type Execute = unsafe extern "system" fn(usize, u32, *const usize);
static EXECUTE: OnceLock<GenericDetour<Execute>> = OnceLock::new();

/// Direct command queues the game submitted on, with their device (AddRef'd, kept).
static QUEUES: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

pub fn install() {
    if com::module("d3d12.dll").is_null() {
        return;
    }
    type Create = unsafe extern "system" fn(usize, u32, *const Guid, *mut usize) -> i32;
    let Some(f) = com::proc(com::module("d3d12.dll"), c"D3D12CreateDevice") else { return };
    let create: Create = unsafe { std::mem::transmute(f) };
    let mut dev = 0usize;
    if unsafe { create(0, 0xb000, &IID_ID3D12DEVICE, &mut dev) } < 0 || dev == 0 {
        log!("present: no throwaway D3D12 device; D3D12 games can't show the overlay inside");
        return;
    }
    // D3D12_COMMAND_QUEUE_DESC: type DIRECT, priority, flags, node mask.
    let desc = [0u32; 4];
    let mut queue = 0usize;
    // ID3D12Device::CreateCommandQueue is slot 8.
    if vcall!(dev, 8, fn(*const u32, *const Guid, *mut usize) -> i32, desc.as_ptr(), &IID_ID3D12COMMANDQUEUE, &mut queue) < 0 {
        release(dev);
        return;
    }
    let addr = com::slot(queue, 10);
    release(queue);
    release(dev);
    with_lock(|| {
        let target: Execute = unsafe { std::mem::transmute(addr) };
        match unsafe { GenericDetour::new(target, execute_hook as Execute) } {
            Ok(h) => {
                let _ = EXECUTE.set(h);
                if crate::hook::frozen(addr, || unsafe { EXECUTE.get().unwrap().enable() }).is_err() {
                    log!("present: ExecuteCommandLists not enabled");
                }
            }
            Err(e) => log!("present: ExecuteCommandLists not hooked: {e}"),
        }
    });
}

unsafe extern "system" fn execute_hook(queue: usize, n: u32, lists: *const usize) {
    note_queue(queue);
    unsafe { EXECUTE.get().unwrap().call(queue, n, lists) }
}

fn note_queue(queue: usize) {
    let mut q = QUEUES.lock().unwrap();
    if q.iter().any(|(x, _)| *x == queue) {
        return;
    }
    // ID3D12CommandQueue::GetDesc (slot 18) returns its struct through a hidden pointer.
    let mut desc = [0u32; 4];
    vcall!(queue, 18, fn(*mut u32) -> *mut u32, desc.as_mut_ptr());
    if desc[0] != 0 {
        return; // not a direct queue
    }
    let mut dev = 0usize;
    // ID3D12DeviceChild::GetDevice is slot 7.
    if vcall!(queue, 7, fn(*const Guid, *mut usize) -> i32, &IID_ID3D12DEVICE, &mut dev) < 0 {
        return;
    }
    vcall!(queue, 1, fn() -> u32);
    q.push((queue, dev));
    release(dev);
}

/// Whether a direct queue of `device` has been seen yet.
pub fn queue_known(device: usize) -> bool {
    QUEUES.lock().unwrap().iter().any(|(_, d)| *d == device)
}

pub struct Renderer {
    d11: d3d11::Renderer,
    on12: usize,
    /// Per back buffer: the wrapped texture and its render target view.
    buffers: Vec<(usize, usize)>,
}

unsafe impl Send for Renderer {}

impl Renderer {
    pub fn new(device: usize, _swap: usize) -> Option<Renderer> {
        let queue = QUEUES.lock().unwrap().iter().find(|(_, d)| *d == device).map(|(q, _)| *q)?;
        type Create = unsafe extern "system" fn(usize, u32, *const u32, u32, *const usize, u32, u32, *mut usize, *mut usize, *mut u32) -> i32;
        let create: Create = unsafe { std::mem::transmute(com::proc(com::load("d3d11.dll"), c"D3D11On12CreateDevice")?) };
        let (mut d11, mut ctx) = (0usize, 0usize);
        let hr = unsafe { create(device, 0x20, std::ptr::null(), 0, &queue, 1, 0, &mut d11, &mut ctx, std::ptr::null_mut()) };
        if hr < 0 || d11 == 0 {
            log!("present: D3D11On12CreateDevice failed ({hr:#x})");
            return None;
        }
        release(ctx);
        let on12 = com::query(d11, &IID_ID3D11ON12DEVICE);
        let r = d3d11::Renderer::new(d11);
        release(d11);
        if on12 == 0 {
            return None;
        }
        Some(Renderer { d11: r?, on12, buffers: Vec::new() })
    }

    fn buffer(&mut self, swap: usize, i: usize) -> Option<(usize, usize)> {
        if let Some(b) = self.buffers.get(i).filter(|b| b.0 != 0) {
            return Some(*b);
        }
        let mut res = 0usize;
        if vcall!(swap, 9, fn(u32, *const Guid, *mut usize) -> i32, i as u32, &IID_ID3D12RESOURCE, &mut res) < 0 {
            return None;
        }
        // D3D11_RESOURCE_FLAGS: bind as a render target.
        let flags = [0x20u32, 0, 0, 0];
        let mut tex = 0usize;
        let hr = vcall!(self.on12, 3, fn(usize, *const u32, u32, u32, *const Guid, *mut usize) -> i32, res, flags.as_ptr(), 0, 0, &IID_ID3D11TEXTURE2D, &mut tex);
        release(res);
        if hr < 0 {
            return None;
        }
        let rtv = self.d11.rtv_for(tex);
        if self.buffers.len() <= i {
            self.buffers.resize(i + 1, (0, 0));
        }
        self.buffers[i] = (tex, rtv);
        Some((tex, rtv))
    }

    pub fn draw(&mut self, swap: usize, w: u32, h: u32, f: &Frame) -> bool {
        let swap3 = com::query(swap, &com::IID_IDXGISWAPCHAIN3);
        if swap3 == 0 {
            return false;
        }
        let i = vcall!(swap3, 36, fn() -> u32) as usize;
        release(swap3);
        let Some((tex, rtv)) = self.buffer(swap, i) else { return false };
        if rtv == 0 {
            return false;
        }
        vcall!(self.on12, 5, fn(*const usize, u32) -> (), &tex, 1);
        self.d11.draw(rtv, w, h, f);
        vcall!(self.on12, 4, fn(*const usize, u32) -> (), &tex, 1);
        self.d11.flush();
        true
    }

    pub fn release_buffers(&mut self) {
        for (tex, rtv) in self.buffers.drain(..) {
            release(rtv);
            release(tex);
        }
        self.d11.flush();
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.release_buffers();
        release(self.on12);
    }
}
