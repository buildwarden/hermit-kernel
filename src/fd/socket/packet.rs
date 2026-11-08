//! Packet sockets: whole Ethernet frames to and from the network device whose
//! MAC address is `HERMIT_RAW_MAC`.
//!
//! The kernel's network stack never sees that device, so nothing in the
//! kernel moves a frame between it and the stack's device; only the
//! application that holds the socket does. One socket at a time may hold the
//! device.

use alloc::boxed::Box;
use core::future;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::Poll;

use async_trait::async_trait;
use hermit_sync::InterruptTicketMutex;
use pci_types::InterruptLine;
use smoltcp::phy::{Device, RxToken, TxToken};
use smoltcp::wire::EthernetAddress;

use crate::drivers::Driver;
use crate::drivers::net::{NetworkDevice, NetworkDriver};
use crate::errno::Errno;
use crate::executor::WakerRegistration;
use crate::executor::network::now;
use crate::fd::{self, ObjectInterface, PollEvent};
use crate::io;

struct PacketDevice {
	device: NetworkDevice,
	waker: WakerRegistration,
}

static PACKET_DEVICE: InterruptTicketMutex<Option<PacketDevice>> = InterruptTicketMutex::new(None);

/// Whether a socket holds the device.
static CLAIMED: AtomicBool = AtomicBool::new(false);

/// The MAC address in `HERMIT_RAW_MAC`, if it is set. An unparsable value
/// stops the boot, so a typo cannot hand the device to the network stack.
pub(crate) fn raw_mac() -> Option<EthernetAddress> {
	let value = hermit_var!("HERMIT_RAW_MAC")?;
	let mut mac = [0u8; 6];
	let mut parts = value.split([':', '-']);
	for byte in &mut mac {
		*byte = parts
			.next()
			.and_then(|part| u8::from_str_radix(part, 16).ok())
			.unwrap_or_else(|| panic!("HERMIT_RAW_MAC is not a MAC address: {value}"));
	}
	assert!(
		parts.next().is_none(),
		"HERMIT_RAW_MAC is not a MAC address: {value}"
	);
	Some(EthernetAddress(mac))
}

/// Installs the device that packet sockets read and write.
pub(crate) fn install(mut device: NetworkDevice) {
	info!(
		"Network device {} serves packet sockets",
		EthernetAddress(device.get_mac_address())
	);
	// Readers wait for the device's interrupt.
	device.set_polling_mode(false);
	PACKET_DEVICE.lock().replace(PacketDevice {
		device,
		waker: WakerRegistration::new(),
	});
}

/// The device's interrupt line and its handler, which wakes the reader.
pub(crate) fn interrupt_handler() -> Option<(InterruptLine, fn())> {
	fn packet_handler() {
		if let Some(dev) = PACKET_DEVICE.lock().as_mut() {
			dev.device.handle_interrupt();
			dev.waker.wake();
		}
	}

	let irq = PACKET_DEVICE.lock().as_ref()?.device.get_interrupt_number();
	Some((irq, packet_handler))
}

pub struct Socket {
	is_nonblocking: bool,
}

impl Socket {
	/// Claims the device, failing if there is none or a socket holds it.
	pub fn claim() -> Result<Self, Errno> {
		if PACKET_DEVICE.lock().is_none() {
			return Err(Errno::Netdown);
		}
		if CLAIMED.swap(true, Ordering::AcqRel) {
			return Err(Errno::Busy);
		}
		Ok(Self {
			is_nonblocking: false,
		})
	}
}

impl Drop for Socket {
	fn drop(&mut self) {
		CLAIMED.store(false, Ordering::Release);
	}
}

#[async_trait]
impl ObjectInterface for Socket {
	async fn poll(&self, event: PollEvent) -> io::Result<PollEvent> {
		future::poll_fn(|cx| {
			let mut guard = PACKET_DEVICE.lock();
			let dev = guard.as_mut().ok_or(Errno::Netdown)?;

			let mut available = PollEvent::POLLOUT | PollEvent::POLLWRNORM;
			if dev.device.has_packet() {
				available |= PollEvent::POLLIN | PollEvent::POLLRDNORM;
			}

			let ret = event & available;
			if ret.is_empty() {
				dev.waker.register(cx.waker());
				Poll::Pending
			} else {
				Poll::Ready(Ok(ret))
			}
		})
		.await
	}

	/// Reads one frame, truncated to `buffer`, and returns the bytes copied.
	async fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
		future::poll_fn(|cx| {
			let mut guard = PACKET_DEVICE.lock();
			let dev = guard.as_mut().ok_or(Errno::Netdown)?;

			if let Some((rx, _tx)) = dev.device.receive(now()) {
				let n = rx.consume(|frame| {
					let n = frame.len().min(buffer.len());
					buffer[..n].copy_from_slice(&frame[..n]);
					n
				});
				Poll::Ready(Ok(n))
			} else if self.is_nonblocking {
				Poll::Ready(Err(Errno::Again))
			} else {
				dev.waker.register(cx.waker());
				Poll::Pending
			}
		})
		.await
	}

	/// Sends `buffer` as one frame.
	async fn write(&self, buffer: &[u8]) -> io::Result<usize> {
		future::poll_fn(|cx| {
			let mut guard = PACKET_DEVICE.lock();
			let dev = guard.as_mut().ok_or(Errno::Netdown)?;

			if buffer.len() > dev.device.capabilities().max_transmission_unit {
				return Poll::Ready(Err(Errno::Msgsize));
			}

			if let Some(tx) = dev.device.transmit(now()) {
				tx.consume(buffer.len(), |frame| frame.copy_from_slice(buffer));
				Poll::Ready(Ok(buffer.len()))
			} else if self.is_nonblocking {
				Poll::Ready(Err(Errno::Again))
			} else {
				// The send queue frees up as the device completes frames; try
				// again on the executor's next pass.
				cx.waker().wake_by_ref();
				Poll::Pending
			}
		})
		.await
	}

	async fn status_flags(&self) -> io::Result<fd::StatusFlags> {
		let status_flags = if self.is_nonblocking {
			fd::StatusFlags::O_NONBLOCK
		} else {
			fd::StatusFlags::empty()
		};

		Ok(status_flags)
	}

	async fn set_status_flags(&mut self, status_flags: fd::StatusFlags) -> io::Result<()> {
		self.is_nonblocking = status_flags.contains(fd::StatusFlags::O_NONBLOCK);
		Ok(())
	}
}
