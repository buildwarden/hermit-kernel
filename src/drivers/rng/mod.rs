//! A driver for the virtio entropy device, which seeds the kernel's random
//! number generator on platforms whose processor has no random instruction.
//!
//! See Virtio specification v1.2 - 5.4.

use alloc::vec::Vec;

use hermit_sync::InterruptTicketMutex;
use pci_types::InterruptLine;
use smallvec::SmallVec;

use crate::arch::pci::PciConfigRegion;
use crate::drivers::Driver;
use crate::drivers::pci::PciDevice;
use crate::drivers::virtio::ControlRegisters;
use crate::drivers::virtio::error::VirtioError;
use crate::drivers::virtio::transport::pci;
use crate::drivers::virtio::transport::pci::{ComCfg, IsrStatus, NotifCfg};
use crate::drivers::virtio::virtqueue::split::SplitVq;
use crate::drivers::virtio::virtqueue::{
	AvailBufferToken, BufferElem, BufferType, VirtQueue, Virtq,
};
use crate::mm::device_alloc::DeviceAlloc;

/// The number of descriptors in the request queue; requests are synchronous,
/// so one would do.
const QUEUE_SIZE: u16 = 8;

/// The number of requests that may come back empty before [`fill`] gives up.
const MAX_EMPTY_REQUESTS: usize = 64;

static RNG_DRIVER: InterruptTicketMutex<Option<VirtioRngDriver>> = InterruptTicketMutex::new(None);

pub(crate) struct VirtioRngDriver {
	dev_id: u16,
	com_cfg: ComCfg,
	isr_stat: IsrStatus,
	notif_cfg: NotifCfg,
	irq: InterruptLine,
	request_vq: Option<VirtQueue>,
}

impl Driver for VirtioRngDriver {
	fn get_interrupt_number(&self) -> InterruptLine {
		self.irq
	}

	fn get_name(&self) -> &'static str {
		"virtio"
	}
}

impl VirtioRngDriver {
	/// Maps the device's capabilities and brings the device up.
	pub(crate) fn init(device: &PciDevice<PciConfigRegion>) -> Result<Self, VirtioError> {
		let caps = pci::map_caps(device)?;
		let mut drv = VirtioRngDriver {
			dev_id: device.device_id(),
			com_cfg: caps.com_cfg,
			isr_stat: caps.isr_cfg,
			notif_cfg: caps.notif_cfg,
			irq: device.get_irq().unwrap(),
			request_vq: None,
		};

		if let Err(err) = drv.init_dev() {
			drv.com_cfg.set_failed();
			return Err(err);
		}

		Ok(drv)
	}

	/// Initializes the device in adherence to specification v1.2 - 3.1.1.
	fn init_dev(&mut self) -> Result<(), VirtioError> {
		self.com_cfg.reset_dev();
		self.com_cfg.ack_dev();
		self.com_cfg.set_drv();

		let minimal_features = virtio::F::VERSION_1;
		let negotiated_features = self
			.com_cfg
			.control_registers()
			.negotiate_features(minimal_features);
		if !negotiated_features.contains(minimal_features) {
			error!("The virtio entropy device does not offer VERSION_1. Aborting!");
			return Err(VirtioError::DevNotSupported(self.dev_id));
		}

		self.com_cfg.features_ok();
		if !self.com_cfg.check_features() {
			error!("The virtio entropy device did not accept our features. Aborting!");
			return Err(VirtioError::DevNotSupported(self.dev_id));
		}

		let mut vq = VirtQueue::Split(
			SplitVq::new(
				&mut self.com_cfg,
				&self.notif_cfg,
				QUEUE_SIZE,
				0,
				negotiated_features,
			)
			.map_err(|_| VirtioError::DevNotSupported(self.dev_id))?,
		);
		// Requests are polled, so the device needs not interrupt.
		vq.disable_notifs();
		self.request_vq = Some(vq);

		self.com_cfg.drv_ok();
		Ok(())
	}

	/// Fills `buf` with bytes from the device, returning false if the device
	/// keeps answering with none.
	fn fill(&mut self, buf: &mut [u8]) -> bool {
		let vq = self.request_vq.as_mut().unwrap();
		let mut filled = 0;
		let mut empty = 0;
		while filled < buf.len() {
			let want = buf.len() - filled;
			let mut recv = SmallVec::new();
			recv.push(BufferElem::Vector(Vec::with_capacity_in(want, DeviceAlloc)));
			let tkn = AvailBufferToken::new(SmallVec::new(), recv).unwrap();
			let Ok(mut used) = vq.dispatch_blocking(tkn, BufferType::Direct) else {
				return false;
			};
			// dispatch_blocking turns notifications back on.
			vq.disable_notifs();

			let bytes = used.used_recv_buff.pop_front_vec();
			let n = bytes.as_ref().map_or(0, |bytes| bytes.len().min(want));
			if n == 0 {
				empty += 1;
				if empty == MAX_EMPTY_REQUESTS {
					return false;
				}
				continue;
			}
			buf[filled..filled + n].copy_from_slice(&bytes.unwrap()[..n]);
			filled += n;
		}

		true
	}

	fn handle_interrupt(&mut self) {
		self.isr_stat.acknowledge();
	}
}

/// Installs the driver that [`fill`] reads from.
pub(crate) fn install(drv: VirtioRngDriver) {
	info!(
		"Virtio entropy device {:x} seeds the random number generator",
		drv.dev_id
	);
	RNG_DRIVER.lock().replace(drv);
}

/// Fills `buf` from the virtio entropy device, returning false when there is
/// no device or it fails.
pub(crate) fn fill(buf: &mut [u8]) -> bool {
	RNG_DRIVER.lock().as_mut().is_some_and(|drv| drv.fill(buf))
}

/// The device's interrupt line and its handler, which only acknowledges it;
/// the queue's notifications are off, but a device may interrupt anyway.
pub(crate) fn interrupt_handler() -> Option<(InterruptLine, fn())> {
	fn rng_handler() {
		if let Some(drv) = RNG_DRIVER.lock().as_mut() {
			drv.handle_interrupt();
		}
	}

	let irq = RNG_DRIVER.lock().as_ref()?.get_interrupt_number();
	Some((irq, rng_handler))
}
